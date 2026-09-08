//! Durable operation driver: state dispatch, provider responses, tools, and recovery.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;

use crate::context::Context;
use crate::message::AgentMessage;
use crate::session::address::{
    branch_tip, operation_preparation_prefix, operation_state, operation_tool_args,
    operation_tool_args_prefix, operation_tool_memo, operation_tool_memo_prefix,
    pending_assistant_frames, pending_tool_output, pending_tool_output_prefix,
};
use crate::session::operation::{
    CheckpointData, Continuation, Control, DeferredScope, GenerationContext, Operation,
    OperationError, OperationIntent, OperationKind, OperationScope, OperationState, ReplayPolicy,
    RetryWait, SummaryContext, SummaryGenerationScope, SummaryRequestRef, ToolBatch, ToolCall,
    ToolCallStatus,
};
use crate::session::traits::SessionReaderExt;
use crate::session::{
    EntryId, LaneName, LaneState, NewUsageRow, OperationId, OperationResultRecord, PendingEntry,
    SettledAssistantMessage, TerminalStatus, UsageId, Write,
};
use crate::tool::{AgentToolResult, ToolExecutionMode};

use super::lane::{DriveController, LaneRuntime};
use super::support::{
    assistant_agent_message, assistant_message, branch_entries, captured_configuration,
    context_messages, context_window, entry_write, map_session_error, new_entry_id, new_usage_id,
    op_cleanup_writes, response_limit, set_json,
};
use crate::harness::api::DriveOptions;
use crate::harness::event::HarnessEventPayload;
use crate::harness::gate::{Gate, GateRejection};
use crate::harness::hooks::{
    AfterResponseEvent, BeforeDriveEvent, BeforePayloadEvent, BeforeRequestEvent,
    BeforeRequestStep, BeforeRunEndEvent, HookRunError,
};
use crate::harness::result::{DriveOutcome, DriveResult, HarnessError, HarnessFault};
use crate::harness::stream::{
    AssistantStreamObserver, HarnessAfterResponse, HarnessAssistantStreamConfig,
    HarnessDeferredStreamConfig, HarnessRequestContext, TransformRequestContext,
    apply_stream_options_patch, native_stream_options, stream_harness_assistant,
    stream_harness_deferred,
};
use crate::harness::tool::ToolInvocation;
use crate::session::configuration::HarnessStreamOptions;

/// Drive one operation to the next durable boundary.
pub(crate) async fn drive(lane: &LaneRuntime, options: DriveOptions, cx: &Context) -> DriveResult {
    lane.ensure_open()?;
    loop {
        let existing = lane.active_drive.lock().await.clone();
        if let Some(existing) = existing {
            if existing.operation_id != options.operation_id {
                return Err(HarnessError::LaneBusy {
                    lane: lane.name.clone(),
                    operation_id: existing.operation_id.clone(),
                    operation_kind: current_kind(lane).await.unwrap_or(OperationKind::Run),
                    message: "another operation is currently driving this lane".to_owned(),
                });
            }
            let notified = existing.done.notified();
            notified.await;
            continue;
        }
        let controller = DriveController::new(options.operation_id.clone());
        let mut active = lane.active_drive.lock().await;
        if active.is_some() {
            continue;
        }
        *active = Some(Arc::clone(&controller));
        drop(active);
        let result = drive_loop(lane, &controller, &options, cx).await;
        let mut active = lane.active_drive.lock().await;
        let owned = active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &controller));
        if owned {
            *active = None;
        }
        drop(active);
        controller.done.notify_waiters();
        if lane.current_operation().await.is_none() {
            lane.idle.notify_waiters();
        }
        return result;
    }
}

async fn drive_loop(
    lane: &LaneRuntime,
    controller: &DriveController,
    options: &DriveOptions,
    cx: &Context,
) -> DriveResult {
    let mut before_drive_invoked = false;
    loop {
        lane.ensure_open()?;
        let operation = current_drive_operation(lane, options).await?;
        if !before_drive_invoked {
            before_drive_invoked = true;
            run_before_drive(lane, &operation, controller, cx).await?;
        }
        if matches!(
            &operation.state.scope().control,
            Control::CancelRequested { .. }
        ) {
            let step = reconcile_abort(lane, &operation, controller, cx).await?;
            match step {
                DriveStep::Continue => continue,
                step => return drive_outcome(&options.operation_id, step),
            }
        }
        let before_state = operation.state.clone();
        let step = dispatch_state(lane, &operation, controller, options, cx).await?;
        match step {
            DriveStep::Continue => {
                let current = lane
                    .current_operation()
                    .await
                    .ok_or_else(|| invariant("drive continued after operation disappeared"))?;
                if current.state == before_state
                    && matches!(&current.state.scope().control, Control::Running)
                {
                    return Err(invariant("drive procedure made no progress"));
                }
            }
            step => return drive_outcome(&options.operation_id, step),
        }
    }
}

/// The lane's current operation, or the mismatch error that stops the drive.
async fn current_drive_operation(
    lane: &LaneRuntime,
    options: &DriveOptions,
) -> Result<Operation, HarnessError> {
    let (current_operation, last_operation_id) = {
        let data = lane.data.lock().await;
        (data.operation.clone(), data.state.last_operation_id.clone())
    };
    let Some(operation) = current_operation else {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: options.operation_id.clone(),
            current_operation_id: None,
            last_operation_id,
            message: "operation is no longer current".to_owned(),
        });
    };
    if operation.meta.operation_id != options.operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: options.operation_id.clone(),
            current_operation_id: Some(operation.meta.operation_id.clone()),
            last_operation_id,
            message: "drive operation id does not match current operation".to_owned(),
        });
    }
    Ok(operation)
}

/// Invoke `before_drive` once when the operation is under running control.
async fn run_before_drive(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<(), HarnessError> {
    if !matches!(&operation.state.scope().control, Control::Running)
        || !lane.owner.hooks.has::<crate::harness::hooks::BeforeDrive>()
    {
        return Ok(());
    }
    let event = BeforeDriveEvent {
        lane: lane.name.clone(),
        run_id: operation.meta.operation_id.to_string(),
        operation: super::support::operation_kind(&operation.meta.intent),
    };
    match lane
        .owner
        .hooks
        .run_with_gate::<crate::harness::hooks::BeforeDrive>(event, &controller.gate, cx)
        .await
    {
        Ok(()) => Ok(()),
        Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
            abort.wait().await;
            Ok(())
        }
        Err(error) => Err(hook_error(error)),
    }
}

/// Dispatch one drive step from the operation's durable state.
async fn dispatch_state(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    options: &DriveOptions,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    match &operation.state {
        OperationState::Starting { .. } => start_run(lane, operation, controller, cx).await,
        OperationState::Checkpoint { .. } => checkpoint(lane, operation, controller, cx).await,
        OperationState::AssistantReady { .. } => generation(lane, operation, controller, cx).await,
        OperationState::AssistantEffectPending { .. } => {
            recover_assistant_effect(lane, operation, controller, cx).await
        }
        OperationState::AssistantRetryWait { .. } | OperationState::SummaryRetryWait { .. } => {
            retry_wait(lane, operation, controller, options.wait_for_retry, cx).await
        }
        OperationState::Tools { .. } => tools(lane, operation, controller, cx).await,
        OperationState::DeferredSuspended { .. } => {
            deferred_suspended(lane, operation, controller, options.poll_deferred, cx).await
        }
        OperationState::DeferredEffectPending { .. } => {
            recover_deferred_effect(lane, operation, controller, options.poll_deferred, cx).await
        }
        OperationState::SummaryDeciding { .. }
        | OperationState::SummaryReady { .. }
        | OperationState::SummaryEffectPending { .. } => {
            structural(lane, operation, controller, options.wait_for_retry, cx).await
        }
        OperationState::NavigationReadyToCommit { .. } => navigation(lane, operation, cx).await,
    }
}

#[derive(Clone, Debug)]
enum DriveStep {
    Continue,
    WaitingRetry { not_before: i64 },
    WaitingDeferred { deferred: pi_ai::DeferredHandle },
    Settled(OperationResultRecord),
}

/// Maps one terminal drive step to its drive outcome.
fn drive_outcome(
    operation_id: &OperationId,
    step: DriveStep,
) -> Result<DriveOutcome, HarnessError> {
    match step {
        DriveStep::Continue => Err(invariant("terminal drive step requested for continue")),
        DriveStep::WaitingRetry { not_before } => Ok(DriveOutcome::WaitingRetry {
            operation_id: operation_id.clone(),
            not_before,
        }),
        DriveStep::WaitingDeferred { deferred } => Ok(DriveOutcome::WaitingDeferred {
            operation_id: operation_id.clone(),
            deferred,
        }),
        DriveStep::Settled(record) => Ok(DriveOutcome::Settled(record)),
    }
}

async fn current_kind(lane: &LaneRuntime) -> Option<OperationKind> {
    lane.current_operation()
        .await
        .map(|operation| super::support::operation_kind(&operation.meta.intent))
}

async fn start_run(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    if let OperationIntent::Run { prompt_entry_ids } = &operation.meta.intent
        && !prompt_entry_ids.is_empty()
        && lane.owner.hooks.has::<crate::harness::hooks::BeforeRun>()
    {
        let prompt = read_entries_by_id(lane, prompt_entry_ids, cx).await?;
        let event = crate::harness::hooks::BeforeRunEvent {
            lane: lane.name.clone(),
            run_id: operation.meta.operation_id.to_string(),
            prompt,
            resources: lane.owner.config_snapshot().await.resources,
        };
        match lane
            .owner
            .hooks
            .run_with_gate::<crate::harness::hooks::BeforeRun>(event, &controller.gate, cx)
            .await
        {
            Ok(Some(result)) => {
                if let Some(messages) = result.messages {
                    append_injected_messages(lane, messages, cx).await?;
                }
            }
            Ok(None) => {}
            Err(error) => return Err(hook_error(error)),
        }
    }
    let tip = lane.data.lock().await.tip.clone();
    let trigger = tip.ok_or_else(|| HarnessError::InvalidMessage {
        lane: lane.name.clone(),
        reason: "missing_prompt".to_owned(),
        message: "run has no prompt entry".to_owned(),
    })?;
    let next = OperationState::Checkpoint {
        scope: operation.state.scope().clone(),
        data: CheckpointData {
            continuation: Continuation::NeedAssistant {
                overflow_recovery_used: false,
            },
            trigger_entry_id: trigger,
        },
    };
    transition(lane, operation, next, cx).await?;
    Ok(DriveStep::Continue)
}
async fn checkpoint(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let (continuation, mut trigger) = match &operation.state {
        OperationState::Checkpoint { data, .. } => {
            (data.continuation.clone(), data.trigger_entry_id.clone())
        }
        _ => return Err(invariant("checkpoint dispatcher received another state")),
    };
    let drained = drain_inbox(lane, operation, cx).await?;
    if !drained.messages.is_empty() {
        trigger = lane
            .data
            .lock()
            .await
            .tip
            .clone()
            .ok_or_else(|| invariant("drained queue has no branch tip"))?;
    }
    match continuation {
        Continuation::NeedAssistant {
            overflow_recovery_used,
        } => {
            let force_compaction = if overflow_recovery_used {
                is_length_response(lane, &trigger, cx).await?
            } else {
                false
            };
            if let Some(step) =
                maybe_start_compaction(lane, operation, trigger.clone(), force_compaction, cx)
                    .await?
            {
                return Ok(step);
            }
            let generation_context =
                generation_context(lane, operation, trigger, overflow_recovery_used, cx).await?;
            let next = OperationState::AssistantReady {
                scope: operation.state.scope().clone(),
                generation_context,
                next_attempt: 1,
            };
            transition(lane, operation, next, cx).await?;
            Ok(DriveStep::Continue)
        }
        Continuation::MayFinish {
            include_final_assistant,
        } => {
            if let Some(trigger) = drained.trigger {
                return renew_assistant_ready(lane, operation, trigger, cx).await;
            }
            finish_run_boundary(lane, operation, controller, include_final_assistant, cx).await
        }
    }
}

/// Whether cancellation reached the operation or its drive gate.
///
/// Source `continueOperation` returns `cancel_requested` before running a
/// boundary planner; the drive loop then reconciles the abort.
async fn boundary_cancelled(lane: &LaneRuntime, controller: &DriveController) -> bool {
    controller.gate.token().is_cancelled()
        || lane
            .current_operation()
            .await
            .as_ref()
            .is_some_and(|operation| {
                matches!(
                    &operation.state.scope().control,
                    Control::CancelRequested { .. }
                )
            })
}

/// Project the lane's retained branch context into hook-visible messages.
///
/// Mirrors `readBoundedContext` in `transcript.ts`: `branch_entries` reads the
/// whole branch and `context_messages` bounds it at the latest compaction.
async fn session_context_messages(
    lane: &LaneRuntime,
    cx: &Context,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let branch = lane.branch(cx).await?;
    let entries = branch_entries(branch.as_ref(), cx)
        .await
        .map_err(map_session_error)?;
    let entry_projectors = lane.owner.config_snapshot().await.entry_projectors;
    context_messages(&entries, &entry_projectors, cx)
        .await
        .map_err(map_session_error)
}

/// Renew the run at `assistant.ready` for a boundary trigger.
///
/// Mirrors `assistantReadyAtBoundary` in `boundary.ts`: a fresh generation
/// context with `overflow_recovery_used` reset and the attempt counter back at
/// one.
async fn renew_assistant_ready(
    lane: &LaneRuntime,
    operation: &Operation,
    trigger: EntryId,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let generation_context = generation_context(lane, operation, trigger, false, cx).await?;
    let next = OperationState::AssistantReady {
        scope: operation.state.scope().clone(),
        generation_context,
        next_attempt: 1,
    };
    transition(lane, operation, next, cx).await?;
    Ok(DriveStep::Continue)
}

/// Run `before_run_end` and either renew the run on a follow-up or settle it.
///
/// Mirrors `finishRunBoundary` in `boundary.ts`: the hook sees the bounded
/// context projection under the drive gate; a returned follow-up becomes a
/// synthetic user message appended at the tip and the run renews at
/// `assistant.ready`; inbox work admitted during the hook preempts the
/// follow-up; otherwise the run settles completed.
async fn finish_run_boundary(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    include_final_assistant: bool,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    if boundary_cancelled(lane, controller).await {
        return Ok(DriveStep::Continue);
    }
    let follow_up = if lane
        .owner
        .hooks
        .has::<crate::harness::hooks::BeforeRunEnd>()
    {
        let messages = session_context_messages(lane, cx).await?;
        let event = BeforeRunEndEvent {
            lane: lane.name.clone(),
            run_id: operation.meta.operation_id.to_string(),
            messages,
        };
        match lane
            .owner
            .hooks
            .run_with_gate::<crate::harness::hooks::BeforeRunEnd>(event, &controller.gate, cx)
            .await
        {
            Ok(result) => result.and_then(|result| result.follow_up),
            Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
                abort.wait().await;
                return reconcile_abort(lane, operation, controller, cx).await;
            }
            Err(error) => return Err(hook_error(error)),
        }
    } else {
        None
    };
    if boundary_cancelled(lane, controller).await {
        return Ok(DriveStep::Continue);
    }
    let redrain = drain_inbox(lane, operation, cx).await?;
    if let Some(trigger) = redrain.trigger {
        return renew_assistant_ready(lane, operation, trigger, cx).await;
    }
    if !redrain.committed
        && let Some(follow_up) = follow_up
    {
        return append_run_follow_up(lane, operation, follow_up, cx).await;
    }
    if lane.data.lock().await.tip.is_none() {
        return Err(invariant("completed run has no tip"));
    }
    if include_final_assistant && operation.state.scope().latest_assistant_entry_id.is_none() {
        return Err(invariant("completed run is missing its final assistant"));
    }
    let record = settle(lane, operation, TerminalStatus::Completed, None, cx).await?;
    Ok(DriveStep::Settled(record))
}

/// Append a `before_run_end` follow-up as a synthetic user message and renew
/// the run at `assistant.ready` in one commit.
///
/// Mirrors the follow-up replay in `finishRunBoundary`: the entry lands at the
/// current tip, the branch tip advances to it, and the operation transitions
/// with the follow-up entry as the generation trigger.
async fn append_run_follow_up(
    lane: &LaneRuntime,
    operation: &Operation,
    follow_up: String,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let entry_id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let message = crate::message::user_text(follow_up, std::iter::empty());
    let generation = generation_context(lane, operation, entry_id.clone(), false, cx).await?;
    let next = OperationState::AssistantReady {
        scope: operation.state.scope().clone(),
        generation_context: generation,
        next_attempt: 1,
    };
    let mut data = lane.data.lock().await;
    let current = data
        .operation
        .as_ref()
        .ok_or_else(|| invariant("operation disappeared during follow-up"))?;
    if current.meta.operation_id != operation.meta.operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: operation.meta.operation_id.clone(),
            current_operation_id: Some(current.meta.operation_id.clone()),
            last_operation_id: data.state.last_operation_id.clone(),
            message: "operation changed during follow-up".to_owned(),
        });
    }
    let writes = vec![
        entry_write(entry_id.clone(), data.tip.clone(), message, false),
        set_json(&branch_tip(lane.name.as_str()), &Some(entry_id.clone()))
            .map_err(map_session_error)?,
        set_json(&operation_state(&operation.meta.operation_id), &next)
            .map_err(map_session_error)?,
    ];
    lane.commit(writes, cx).await?;
    data.tip = Some(entry_id.clone());
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state: next,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    let entry = lane
        .owner
        .session
        .get_entry(&entry_id, cx)
        .await
        .map_err(map_session_error)?
        .ok_or_else(|| invariant("follow-up entry was not committed"))?;
    lane.emit(HarnessEventPayload::EntryAdded { entry }, cx)
        .await;
    Ok(DriveStep::Continue)
}

async fn is_length_response(
    lane: &LaneRuntime,
    entry_id: &EntryId,
    cx: &Context,
) -> Result<bool, HarnessError> {
    let Some(entry) = lane
        .owner
        .session
        .get_entry(entry_id, cx)
        .await
        .map_err(map_session_error)?
    else {
        return Err(invariant("overflow checkpoint trigger entry is missing"));
    };
    Ok(matches!(
        entry.message().and_then(assistant_message),
        Some(message) if matches!(message.stop_reason, pi_ai::StopReason::Length)
    ))
}

async fn maybe_start_compaction(
    lane: &LaneRuntime,
    operation: &Operation,
    trigger: EntryId,
    force: bool,
    cx: &Context,
) -> Result<Option<DriveStep>, HarnessError> {
    let settings = operation.state.scope().settings.compaction;
    if !settings.enabled {
        return Ok(None);
    }
    let config = lane.data.lock().await.config.clone();
    let Some(model) = lane
        .owner
        .models
        .get_model(&config.model.provider, &config.model.model_id)
    else {
        let record = settle_failure(
            lane,
            operation,
            "model_unavailable".to_owned(),
            format!(
                "configured model {}/{} is unavailable",
                config.model.provider, config.model.model_id
            ),
            cx,
        )
        .await?;
        return Ok(Some(DriveStep::Settled(record)));
    };
    let branch = lane.branch(cx).await?;
    let entries = branch_entries(branch.as_ref(), cx)
        .await
        .map_err(map_session_error)?;
    if !force && !compaction_threshold_met(&entries, &model, &settings) {
        return Ok(None);
    }
    let preparation = match crate::harness::compaction::prepare_compaction(&entries, &settings) {
        Ok(Some(preparation)) => preparation,
        Ok(None) => return Ok(None),
        Err(error) => {
            let record = settle_failure(
                lane,
                operation,
                error.code().to_owned(),
                error.to_string(),
                cx,
            )
            .await?;
            return Ok(Some(DriveStep::Settled(record)));
        }
    };
    let task_id = format!("{}:compaction:{}", operation.meta.operation_id, trigger);
    let reason = if force {
        crate::session::CompactionReason::Overflow
    } else {
        crate::session::CompactionReason::Threshold
    };
    let task = crate::session::SummaryTask {
        task_id: task_id.clone(),
        reason: Some(reason),
        custom_instructions: None,
        boundary: crate::session::ResultBoundary::ResumeCheckpoint {
            resume_after: CheckpointData {
                continuation: Continuation::NeedAssistant {
                    overflow_recovery_used: force,
                },
                trigger_entry_id: trigger,
            },
        },
    };
    let next = OperationState::SummaryDeciding {
        scope: operation.state.scope().clone(),
        task,
    };
    transition_with_preparation(
        lane,
        operation,
        &task_id,
        preparation.to_durable(),
        next,
        cx,
    )
    .await?;
    lane.emit(
        HarnessEventPayload::CompactionStart {
            run_id: operation.meta.operation_id.clone(),
            reason,
            started_at: crate::message::now_millis(),
        },
        cx,
    )
    .await;
    Ok(Some(DriveStep::Continue))
}

/// Whether the retained context crosses the configured compaction threshold.
fn compaction_threshold_met(
    entries: &[crate::session::Entry],
    model: &pi_ai::Model,
    settings: &crate::session::configuration::CompactionSettings,
) -> bool {
    let context_tokens = entries
        .iter()
        .filter_map(crate::session::Entry::message)
        .map(crate::harness::compaction::estimate_tokens)
        .fold(0_u64, u64::saturating_add);
    let context_window = if model.context_window == 0 {
        128_000
    } else {
        model.context_window
    };
    crate::harness::compaction::should_compact(context_tokens, context_window, settings)
}

/// Settle the operation as failed with a plain operation error.
async fn settle_failure(
    lane: &LaneRuntime,
    operation: &Operation,
    code: String,
    message: String,
    cx: &Context,
) -> Result<OperationResultRecord, HarnessError> {
    settle(
        lane,
        operation,
        TerminalStatus::Failed,
        Some(OperationError {
            code,
            message,
            details: None,
        }),
        cx,
    )
    .await
}

async fn generation_context(
    lane: &LaneRuntime,
    operation: &Operation,
    trigger: EntryId,
    overflow_recovery_used: bool,
    _cx: &Context,
) -> Result<GenerationContext, HarnessError> {
    let config = lane.data.lock().await.config.clone();
    let options = lane.owner.config_snapshot().await.stream_options;
    let retry = super::support::normalized_retry(lane.owner.config_snapshot().await.retry)?;
    let step_id = format!("{}:{}", operation.meta.operation_id, trigger);
    Ok(GenerationContext {
        step_id,
        trigger_entry_id: trigger,
        configuration: config,
        stream_options: options,
        retry_policy: retry,
        overflow_recovery_used,
    })
}

#[derive(Clone, Copy)]
struct CurrentOperation<'a> {
    lane: &'a LaneRuntime,
    operation: &'a Operation,
    controller: &'a DriveController,
    cx: &'a Context,
}

enum AssistantModel {
    Ready(pi_ai::Model),
    Settled(OperationResultRecord),
}

struct AssistantCommit {
    response_id: EntryId,
    assistant: AgentMessage,
    usage: pi_ai::Usage,
    usage_id: UsageId,
    next_state: OperationState,
    status: TerminalStatus,
    error: Option<OperationError>,
}

struct ResponseTransition {
    next_state: OperationState,
    status: TerminalStatus,
    error: Option<OperationError>,
}

async fn require_assistant_model(
    lane: &LaneRuntime,
    operation: &Operation,
    context: &GenerationContext,
    cx: &Context,
) -> Result<AssistantModel, HarnessError> {
    let Some(model) = lane.owner.models.get_model(
        &context.configuration.model.provider,
        &context.configuration.model.model_id,
    ) else {
        let record = settle(
            lane,
            operation,
            TerminalStatus::Failed,
            Some(OperationError {
                code: "model_unavailable".to_owned(),
                message: format!(
                    "configured model {}/{} is unavailable",
                    context.configuration.model.provider, context.configuration.model.model_id
                ),
                details: None,
            }),
            cx,
        )
        .await?;
        return Ok(AssistantModel::Settled(record));
    };
    Ok(AssistantModel::Ready(model))
}

async fn assistant_stream_config(
    current: CurrentOperation<'_>,
    model: pi_ai::Model,
    system_prompt: String,
    tools: Vec<pi_ai::Tool>,
    context: &GenerationContext,
    stream_options: HarnessStreamOptions,
    response_id: EntryId,
) -> HarnessAssistantStreamConfig {
    HarnessAssistantStreamConfig {
        models: Arc::clone(&current.lane.owner.models),
        model,
        session_id: format!(
            "{}:{}",
            current.lane.owner.session.metadata().id,
            current.lane.name.as_str()
        ),
        system_prompt,
        tools,
        thinking_level: context.configuration.thinking_level,
        stream_options,
        transform_context: transform_context(current.lane, current.operation, current.controller),
        to_provider_messages: current
            .lane
            .owner
            .config_snapshot()
            .await
            .to_provider_messages,
        on_payload: before_payload_callback(
            current.lane,
            current.operation,
            current.controller,
            current.cx,
        ),
        on_response: None,
        after_response: after_response_callback(
            current.lane,
            current.operation,
            current.controller,
        ),
        observer: Arc::new(ResponseObserver::new(
            current.lane,
            current.operation.meta.operation_id.clone(),
            response_id,
        )),
    }
}

async fn generation(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let current = CurrentOperation {
        lane,
        operation,
        controller,
        cx,
    };
    let (scope, context, attempt) = match &operation.state {
        OperationState::AssistantReady {
            scope,
            generation_context,
            next_attempt,
        } => (scope.clone(), generation_context.clone(), *next_attempt),
        _ => return Err(invariant("generation dispatcher received another state")),
    };
    let model = match require_assistant_model(lane, operation, &context, cx).await? {
        AssistantModel::Ready(model) => model,
        AssistantModel::Settled(record) => return Ok(DriveStep::Settled(record)),
    };
    let tools = configured_tools(lane, &context.configuration.active_tool_names).await?;
    let branch = lane.branch(cx).await?;
    let entries = branch_entries(branch.as_ref(), cx)
        .await
        .map_err(map_session_error)?;
    let entry_projectors = lane.owner.config_snapshot().await.entry_projectors;
    let messages = context_messages(&entries, &entry_projectors, cx)
        .await
        .map_err(map_session_error)?;
    let system_prompt = resolve_system_prompt(lane, controller, cx).await?;
    let stream_options = match before_request_options(
        current,
        &model,
        BeforeRequestStep::Assistant,
        attempt,
        context.stream_options.clone(),
    )
    .await
    {
        Ok(options) => options,
        Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
            abort.wait().await;
            return reconcile_abort(lane, operation, controller, cx).await;
        }
        Err(error) => return Err(hook_error(error)),
    };
    let response_id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let usage_id = new_usage_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let pending = OperationState::AssistantEffectPending {
        scope: scope.clone(),
        generation_context: context.clone(),
        attempt,
        response_entry_id: response_id.clone(),
        usage_id: usage_id.clone(),
        intended_output_limit: response_limit(&model),
        context_window: context_window(&model),
    };
    transition(lane, operation, pending, cx).await?;
    let stream_config = assistant_stream_config(
        current,
        model,
        system_prompt,
        tools,
        &context,
        stream_options,
        response_id.clone(),
    )
    .await;
    let response = match stream_harness_assistant(&messages, &stream_config, &controller.gate, cx)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return stream_failure(lane, operation, controller, context, attempt, error, cx).await;
        }
    };
    let pending = lane
        .current_operation()
        .await
        .ok_or_else(|| invariant("assistant operation disappeared before publication"))?;
    if matches!(
        &pending.state.scope().control,
        Control::CancelRequested { .. }
    ) {
        let record = publish_interrupted(lane, operation, &response_id, cx).await?;
        return Ok(DriveStep::Settled(record));
    }
    publish_response(lane, operation, pending.state, response, cx).await
}

fn pending_response_id(state: &OperationState) -> Result<EntryId, HarnessError> {
    match state {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => Ok(response_entry_id.clone()),
        _ => Err(invariant(
            "response publication without response reservation",
        )),
    }
}

async fn stream_failure(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    context: GenerationContext,
    attempt: u64,
    error: crate::harness::stream::HarnessStreamError,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    match error {
        crate::harness::stream::HarnessStreamError::Gate(GateRejection::Aborted(_)) => {
            reconcile_abort(lane, operation, controller, cx).await
        }
        crate::harness::stream::HarnessStreamError::Gate(GateRejection::Closed(fault)) => {
            Err(HarnessError::Closed {
                message: fault.message.clone(),
            })
        }
        crate::harness::stream::HarnessStreamError::Fault(fault) => {
            lane.owner.fault(fault, cx).await;
            Err(lane.owner.closed_error())
        }
        crate::harness::stream::HarnessStreamError::Preparation(error) => {
            let record = settle(
                lane,
                operation,
                TerminalStatus::Failed,
                Some(OperationError {
                    code: "request_preparation".to_owned(),
                    message: error.to_string(),
                    details: None,
                }),
                cx,
            )
            .await?;
            Ok(DriveStep::Settled(record))
        }
        crate::harness::stream::HarnessStreamError::Provider(error) => {
            if controller.gate.token().is_cancelled()
                || matches!(
                    lane.current_operation()
                        .await
                        .as_ref()
                        .map(|operation| operation.state.scope().control.clone()),
                    Some(Control::CancelRequested { .. })
                )
            {
                return reconcile_abort(lane, operation, controller, cx).await;
            }
            let message = error.to_string();
            if attempt < context.retry_policy.max_attempts {
                let delay = retry_delay(context.retry_policy.base_delay_ms, attempt)?;
                let not_before = crate::message::now_millis()
                    .checked_add(i64::try_from(delay).map_err(|_| HarnessError::Closed {
                        message: "retry delay exceeds timestamp range".to_owned(),
                    })?)
                    .ok_or_else(|| HarnessError::Closed {
                        message: "retry timestamp overflow".to_owned(),
                    })?;
                let next = OperationState::AssistantRetryWait {
                    scope: operation.state.scope().clone(),
                    generation_context: context.clone(),
                    retry: RetryWait {
                        next_attempt: attempt.checked_add(1).ok_or_else(|| {
                            HarnessError::Closed {
                                message: "retry attempt overflow".to_owned(),
                            }
                        })?,
                        not_before,
                        error_message: message.clone(),
                    },
                };
                transition(lane, operation, next, cx).await?;
                lane.emit(
                    HarnessEventPayload::RetryScheduled {
                        run_id: operation.meta.operation_id.clone(),
                        step: context.step_id.clone(),
                        attempt,
                        max_attempts: context.retry_policy.max_attempts,
                        delay_ms: delay,
                        not_before,
                        error_message: message,
                    },
                    cx,
                )
                .await;
                Ok(DriveStep::WaitingRetry { not_before })
            } else {
                let record = settle(
                    lane,
                    operation,
                    TerminalStatus::Failed,
                    Some(OperationError {
                        code: "provider_error".to_owned(),
                        message,
                        details: None,
                    }),
                    cx,
                )
                .await?;
                Ok(DriveStep::Settled(record))
            }
        }
    }
}

async fn publish_response(
    lane: &LaneRuntime,
    operation: &Operation,
    pending: OperationState,
    response: SettledAssistantMessage,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let message = response.into_inner();
    let response_id = pending_response_id(&pending)?;
    let usage_id = pending_usage_id(&pending)?;
    let assistant = assistant_agent_message(message.clone())?;
    let calls = assistant_tool_calls(&message);
    let transition =
        assistant_response_state(lane, &pending, &message, &calls, &response_id).await?;
    let record = commit_assistant(
        lane,
        operation,
        AssistantCommit {
            response_id,
            assistant,
            usage: message.usage.clone(),
            usage_id,
            next_state: transition.next_state,
            status: transition.status,
            error: transition.error,
        },
        cx,
    )
    .await?;
    if record.status == TerminalStatus::Failed && record.error.is_some() {
        return Ok(DriveStep::Settled(record));
    }
    if matches!(
        &operation.state,
        OperationState::AssistantEffectPending { .. }
    ) && matches!(message.stop_reason, pi_ai::StopReason::Deferred)
    {
        return Ok(DriveStep::WaitingDeferred {
            deferred: message.deferred.ok_or_else(|| HarnessError::Closed {
                message: "deferred response omitted its handle".to_owned(),
            })?,
        });
    }
    if matches!(message.stop_reason, pi_ai::StopReason::Error)
        && record.status == TerminalStatus::Failed
    {
        let current = lane
            .current_operation()
            .await
            .ok_or_else(|| invariant("retry state disappeared"))?;
        if let OperationState::AssistantRetryWait { retry, .. } = current.state {
            return Ok(DriveStep::WaitingRetry {
                not_before: retry.not_before,
            });
        }
    }
    Ok(DriveStep::Continue)
}

fn pending_usage_id(state: &OperationState) -> Result<UsageId, HarnessError> {
    match state {
        OperationState::AssistantEffectPending { usage_id, .. }
        | OperationState::DeferredEffectPending { usage_id, .. } => Ok(usage_id.clone()),
        _ => Err(invariant("response publication without pending state")),
    }
}

fn assistant_tool_calls(message: &pi_ai::AssistantMessage) -> Vec<(u32, pi_ai::ToolCall)> {
    message
        .content
        .iter()
        .enumerate()
        .filter_map(|(source_index, block)| match block {
            pi_ai::AssistantContent::ToolCall(call) => {
                Some((u32::try_from(source_index).ok()?, call.clone()))
            }
            _ => None,
        })
        .collect()
}

async fn assistant_response_state(
    lane: &LaneRuntime,
    pending: &OperationState,
    message: &pi_ai::AssistantMessage,
    calls: &[(u32, pi_ai::ToolCall)],
    response_id: &EntryId,
) -> Result<ResponseTransition, HarnessError> {
    if matches!(message.stop_reason, pi_ai::StopReason::Error) {
        error_response_state(lane, pending, message, response_id).await
    } else if matches!(message.stop_reason, pi_ai::StopReason::Length) {
        length_response_state(pending, response_id)
    } else if matches!(message.stop_reason, pi_ai::StopReason::Deferred) {
        deferred_response_state(pending, message, response_id)
    } else if !calls.is_empty() && matches!(message.stop_reason, pi_ai::StopReason::ToolUse) {
        tool_use_response_state(lane, pending, calls, response_id)
    } else {
        Ok(finish_response_state(pending, response_id))
    }
}

async fn error_response_state(
    lane: &LaneRuntime,
    pending: &OperationState,
    message: &pi_ai::AssistantMessage,
    response_id: &EntryId,
) -> Result<ResponseTransition, HarnessError> {
    let attempt = match pending {
        OperationState::AssistantEffectPending { attempt, .. } => *attempt,
        _ => 1,
    };
    let scope = pending.scope().clone();
    let context = match pending {
        OperationState::AssistantEffectPending {
            generation_context, ..
        } => generation_context.clone(),
        OperationState::DeferredEffectPending { deferred, .. } => GenerationContext {
            step_id: deferred.step_id.clone(),
            trigger_entry_id: deferred.source_entry_id.clone(),
            configuration: deferred.configuration.clone(),
            stream_options: deferred.stream_options.clone(),
            retry_policy: super::support::normalized_retry(
                lane.owner.config_snapshot().await.retry,
            )?,
            overflow_recovery_used: false,
        },
        _ => return Err(invariant("error response without generation context")),
    };
    let error_message = message
        .error_message
        .clone()
        .unwrap_or_else(|| "provider returned an error".to_owned());
    if attempt < context.retry_policy.max_attempts {
        let delay = retry_delay(context.retry_policy.base_delay_ms, attempt)?;
        let not_before = crate::message::now_millis()
            .checked_add(i64::try_from(delay).map_err(|_| HarnessError::Closed {
                message: "retry delay exceeds timestamp range".to_owned(),
            })?)
            .ok_or_else(|| HarnessError::Closed {
                message: "retry timestamp overflow".to_owned(),
            })?;
        let next = OperationState::AssistantRetryWait {
            scope,
            generation_context: context,
            retry: RetryWait {
                next_attempt: attempt.checked_add(1).ok_or_else(|| HarnessError::Closed {
                    message: "retry attempt overflow".to_owned(),
                })?,
                not_before,
                error_message,
            },
        };
        return Ok(ResponseTransition {
            next_state: next,
            status: TerminalStatus::Failed,
            error: None,
        });
    }
    Ok(ResponseTransition {
        next_state: OperationState::Checkpoint {
            scope,
            data: CheckpointData {
                continuation: Continuation::MayFinish {
                    include_final_assistant: true,
                },
                trigger_entry_id: response_id.clone(),
            },
        },
        status: TerminalStatus::Failed,
        error: Some(OperationError {
            code: "provider_error".to_owned(),
            message: error_message,
            details: None,
        }),
    })
}

fn length_response_state(
    pending: &OperationState,
    response_id: &EntryId,
) -> Result<ResponseTransition, HarnessError> {
    let (scope, trigger_entry_id, overflow_recovery_used) = match pending {
        OperationState::AssistantEffectPending {
            scope,
            response_entry_id,
            generation_context,
            ..
        } => (
            scope.clone(),
            response_entry_id.clone(),
            generation_context.overflow_recovery_used,
        ),
        OperationState::DeferredEffectPending { deferred, .. } => {
            (deferred.scope.clone(), response_id.clone(), true)
        }
        _ => return Err(invariant("length response without generation context")),
    };
    if overflow_recovery_used {
        return Ok(ResponseTransition {
            next_state: OperationState::Checkpoint {
                scope,
                data: CheckpointData {
                    continuation: Continuation::MayFinish {
                        include_final_assistant: true,
                    },
                    trigger_entry_id,
                },
            },
            status: TerminalStatus::Failed,
            error: Some(OperationError {
                code: "context_overflow".to_owned(),
                message: "assistant response exceeded the context window after compaction"
                    .to_owned(),
                details: None,
            }),
        });
    }
    Ok(ResponseTransition {
        next_state: OperationState::Checkpoint {
            scope,
            data: CheckpointData {
                continuation: Continuation::NeedAssistant {
                    overflow_recovery_used: true,
                },
                trigger_entry_id,
            },
        },
        status: TerminalStatus::Completed,
        error: None,
    })
}

fn deferred_response_state(
    pending: &OperationState,
    message: &pi_ai::AssistantMessage,
    response_id: &EntryId,
) -> Result<ResponseTransition, HarnessError> {
    if message.deferred.is_none() {
        return Err(HarnessError::Closed {
            message: "deferred response omitted its handle".to_owned(),
        });
    }
    let (scope, configuration, stream_options, step_id) = match pending {
        OperationState::AssistantEffectPending {
            scope,
            generation_context,
            ..
        } => (
            scope.clone(),
            generation_context.configuration.clone(),
            generation_context.stream_options.clone(),
            generation_context.step_id.clone(),
        ),
        OperationState::DeferredEffectPending { deferred, .. } => (
            deferred.scope.clone(),
            deferred.configuration.clone(),
            deferred.stream_options.clone(),
            deferred.step_id.clone(),
        ),
        _ => return Err(invariant("deferred response without scope")),
    };
    let deferred = DeferredScope {
        scope,
        step_id,
        source_entry_id: response_id.clone(),
        poll: 0,
        configuration,
        stream_options,
    };
    Ok(ResponseTransition {
        next_state: OperationState::DeferredSuspended { deferred },
        status: TerminalStatus::Completed,
        error: None,
    })
}

fn tool_use_response_state(
    lane: &LaneRuntime,
    pending: &OperationState,
    calls: &[(u32, pi_ai::ToolCall)],
    response_id: &EntryId,
) -> Result<ResponseTransition, HarnessError> {
    let (mut scope, configuration, step_id) = match pending {
        OperationState::AssistantEffectPending {
            scope,
            generation_context,
            ..
        } => (
            scope.clone(),
            generation_context.configuration.clone(),
            generation_context.step_id.clone(),
        ),
        OperationState::DeferredEffectPending { deferred, .. } => (
            deferred.scope.clone(),
            deferred.configuration.clone(),
            deferred.step_id.clone(),
        ),
        _ => return Err(invariant("tool response without scope")),
    };
    let mut tool_calls = Vec::with_capacity(calls.len());
    for (source_index, _call) in calls {
        let result_entry_id =
            new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
        tool_calls.push(ToolCall {
            source_index: *source_index,
            result_entry_id,
            status: ToolCallStatus::Planned,
        });
    }
    scope.latest_assistant_entry_id = Some(response_id.clone());
    Ok(ResponseTransition {
        next_state: OperationState::Tools {
            scope,
            batch: ToolBatch {
                assistant_entry_id: response_id.clone(),
                configuration,
                turn_id: step_id,
                calls: tool_calls,
            },
        },
        status: TerminalStatus::Completed,
        error: None,
    })
}

fn finish_response_state(pending: &OperationState, response_id: &EntryId) -> ResponseTransition {
    let mut scope = pending.scope().clone();
    scope.latest_assistant_entry_id = Some(response_id.clone());
    ResponseTransition {
        next_state: OperationState::Checkpoint {
            scope,
            data: CheckpointData {
                continuation: Continuation::MayFinish {
                    include_final_assistant: true,
                },
                trigger_entry_id: response_id.clone(),
            },
        },
        status: TerminalStatus::Completed,
        error: None,
    }
}

async fn commit_assistant(
    lane: &LaneRuntime,
    operation: &Operation,
    commit: AssistantCommit,
    cx: &Context,
) -> Result<OperationResultRecord, HarnessError> {
    let AssistantCommit {
        response_id,
        assistant,
        usage,
        usage_id,
        next_state,
        status,
        error,
    } = commit;
    let mut data = lane.data.lock().await;
    let parent = data.tip.clone();
    let entry = entry_write(response_id.clone(), parent, assistant, false);
    let usage_row = Write::Usage {
        row: NewUsageRow {
            id: usage_id,
            usage,
            entry_id: Some(response_id.clone()),
            adjustment: false,
            details: None,
        },
    };
    let state_write = set_json(&operation_state(&operation.meta.operation_id), &next_state)
        .map_err(map_session_error)?;
    let tip = Some(response_id.clone());
    let tip_write = set_json(&branch_tip(lane.name.as_str()), &tip).map_err(map_session_error)?;
    let writes = vec![
        entry,
        usage_row,
        state_write,
        tip_write,
        crate::session::delete_list(&pending_assistant_frames(
            &operation.meta.operation_id,
            &response_id,
        )),
    ];
    lane.commit(writes, cx).await?;
    data.tip = tip;
    if matches!(
        &next_state,
        OperationState::DeferredSuspended { .. }
            | OperationState::AssistantRetryWait { .. }
            | OperationState::Checkpoint { .. }
            | OperationState::Tools { .. }
    ) {
        let mut current = data
            .operation
            .clone()
            .ok_or_else(|| invariant("assistant operation disappeared"))?;
        current.state = next_state.clone();
        data.operation = Some(current);
    }
    drop(data);
    lane.state_changed.notify_waiters();
    lane.emit(
        HarnessEventPayload::EntryAdded {
            entry: lane
                .owner
                .session
                .get_entry(&response_id, cx)
                .await
                .map_err(map_session_error)?
                .ok_or_else(|| invariant("assistant entry was not committed"))?,
        },
        cx,
    )
    .await;
    if status == TerminalStatus::Completed
        && matches!(
            next_state_at(&next_state),
            crate::session::OperationAt::Tools
        )
    {
        lane.emit(
            HarnessEventPayload::TurnStart {
                run_id: operation.meta.operation_id.clone(),
                turn_id: operation.meta.operation_id.to_string(),
            },
            cx,
        )
        .await;
    }
    if status == TerminalStatus::Failed
        && error.is_some()
        && matches!(&next_state, OperationState::Checkpoint { .. })
    {
        let record = settle(lane, operation, status, error, cx).await?;
        return Ok(record);
    }
    Ok(OperationResultRecord {
        operation_id: operation.meta.operation_id.clone(),
        kind: OperationKind::Run,
        status,
        error,
        from_tip_id: operation.meta.source_tip_id.clone(),
        tip_id: Some(response_id),
        started_at: operation.meta.started_at,
        ended_at: crate::message::now_millis(),
    })
}

fn next_state_at(state: &OperationState) -> crate::session::OperationAt {
    state.at()
}

async fn retry_wait(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    wait: bool,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let (OperationState::AssistantRetryWait { retry, .. }
    | OperationState::SummaryRetryWait { retry, .. }) = &operation.state
    else {
        return Err(invariant("retry wait dispatcher received another state"));
    };
    if !wait {
        return Ok(DriveStep::WaitingRetry {
            not_before: retry.not_before,
        });
    }
    let now = crate::message::now_millis();
    if retry.not_before > now {
        let delay = u64::try_from(retry.not_before.saturating_sub(now)).map_err(|_| {
            HarnessError::Closed {
                message: "retry delay is negative".to_owned(),
            }
        })?;
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
            () = controller.gate.token().cancelled() => {
                return reconcile_abort(lane, operation, controller, cx).await;
            }
        }
    }
    let next = match &operation.state {
        OperationState::AssistantRetryWait {
            scope,
            generation_context,
            retry,
        } => OperationState::AssistantReady {
            scope: scope.clone(),
            generation_context: generation_context.clone(),
            next_attempt: retry.next_attempt,
        },
        OperationState::SummaryRetryWait {
            scope,
            generation,
            retry,
        } => OperationState::SummaryReady {
            scope: scope.clone(),
            generation: generation.clone(),
            next_attempt: retry.next_attempt,
        },
        _ => return Err(invariant("retry wait changed state")),
    };
    transition(lane, operation, next, cx).await?;
    Ok(DriveStep::Continue)
}

async fn deferred_suspended(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    poll: bool,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let deferred = match &operation.state {
        OperationState::DeferredSuspended { deferred } => deferred.clone(),
        _ => return Err(invariant("deferred dispatcher received another state")),
    };
    let handle = deferred_handle(lane, &deferred, cx).await?;
    if !poll {
        return Ok(DriveStep::WaitingDeferred { deferred: handle });
    }
    let model = lane
        .owner
        .models
        .get_model(
            &deferred.configuration.model.provider,
            &deferred.configuration.model.model_id,
        )
        .ok_or_else(|| HarnessError::Closed {
            message: format!(
                "deferred model {}/{} is unavailable",
                deferred.configuration.model.provider, deferred.configuration.model.model_id
            ),
        })?;
    let poll_attempt = deferred
        .poll
        .checked_add(1)
        .ok_or_else(|| HarnessError::Closed {
            message: "deferred poll count overflow".to_owned(),
        })?;
    let current = CurrentOperation {
        lane,
        operation,
        controller,
        cx,
    };
    let stream_options = match before_request_options(
        current,
        &model,
        BeforeRequestStep::Deferred,
        u64::from(poll_attempt),
        deferred.stream_options.clone(),
    )
    .await
    {
        Ok(options) => options,
        Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
            abort.wait().await;
            return reconcile_abort(lane, operation, controller, cx).await;
        }
        Err(error) => return Err(hook_error(error)),
    };
    let response_id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let usage_id = new_usage_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let next_deferred = DeferredScope {
        poll: poll_attempt,
        ..deferred.clone()
    };
    let next = OperationState::DeferredEffectPending {
        deferred: next_deferred,
        response_entry_id: response_id.clone(),
        usage_id,
    };
    transition(lane, operation, next, cx).await?;
    let config = deferred_stream_config(
        current,
        &deferred,
        model,
        stream_options,
        response_id.clone(),
    );
    let response =
        match stream_harness_deferred(&config, handle.clone(), &controller.gate, cx).await {
            Ok(response) => response,
            Err(error) => {
                return deferred_stream_failure(lane, operation, controller, error, cx).await;
            }
        };
    publish_deferred_response(lane, operation, handle, response, cx).await
}

async fn recover_deferred_effect(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    poll_deferred: bool,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let deferred = match &operation.state {
        OperationState::DeferredEffectPending { deferred, .. } => deferred.clone(),
        _ => return Err(invariant("deferred effect recovery received another state")),
    };
    let handle = deferred_handle(lane, &deferred, cx).await?;
    if !poll_deferred {
        return Ok(DriveStep::WaitingDeferred { deferred: handle });
    }
    let old_response_id = match &operation.state {
        OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => return Err(invariant("deferred effect has no response reservation")),
    };
    let model = lane
        .owner
        .models
        .get_model(
            &deferred.configuration.model.provider,
            &deferred.configuration.model.model_id,
        )
        .ok_or_else(|| HarnessError::Closed {
            message: format!(
                "deferred model {}/{} is unavailable",
                deferred.configuration.model.provider, deferred.configuration.model.model_id
            ),
        })?;
    let current = CurrentOperation {
        lane,
        operation,
        controller,
        cx,
    };
    let stream_options = match before_request_options(
        current,
        &model,
        BeforeRequestStep::Deferred,
        u64::from(deferred.poll),
        deferred.stream_options.clone(),
    )
    .await
    {
        Ok(options) => options,
        Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
            abort.wait().await;
            return reconcile_abort(lane, operation, controller, cx).await;
        }
        Err(error) => return Err(hook_error(error)),
    };
    let response_id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let usage_id = new_usage_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
    let next = OperationState::DeferredEffectPending {
        deferred: deferred.clone(),
        response_entry_id: response_id.clone(),
        usage_id,
    };
    transition_with_writes(
        lane,
        operation,
        next,
        vec![crate::session::delete_list(&pending_assistant_frames(
            &operation.meta.operation_id,
            &old_response_id,
        ))],
        cx,
    )
    .await?;
    let config = deferred_stream_config(current, &deferred, model, stream_options, response_id);
    let response =
        match stream_harness_deferred(&config, handle.clone(), &controller.gate, cx).await {
            Ok(response) => response,
            Err(error) => {
                return deferred_stream_failure(lane, operation, controller, error, cx).await;
            }
        };
    publish_deferred_response(lane, operation, handle, response, cx).await
}

fn deferred_stream_config(
    current: CurrentOperation<'_>,
    deferred: &DeferredScope,
    model: pi_ai::Model,
    stream_options: HarnessStreamOptions,
    response_id: EntryId,
) -> HarnessDeferredStreamConfig {
    HarnessDeferredStreamConfig {
        models: Arc::clone(&current.lane.owner.models),
        model,
        session_id: format!(
            "{}:{}",
            current.lane.owner.session.metadata().id,
            current.lane.name.as_str()
        ),
        thinking_level: deferred.configuration.thinking_level,
        stream_options,
        on_payload: before_payload_callback(
            current.lane,
            current.operation,
            current.controller,
            current.cx,
        ),
        on_response: None,
        after_response: after_response_callback(
            current.lane,
            current.operation,
            current.controller,
        ),
        observer: Arc::new(ResponseObserver::new(
            current.lane,
            current.operation.meta.operation_id.clone(),
            response_id,
        )),
    }
}

async fn publish_deferred_response(
    lane: &LaneRuntime,
    operation: &Operation,
    _handle: pi_ai::DeferredHandle,
    response: SettledAssistantMessage,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let pending = lane
        .current_operation()
        .await
        .ok_or_else(|| invariant("deferred operation disappeared before publication"))?
        .state;
    publish_response(lane, operation, pending, response, cx).await
}

async fn deferred_handle(
    lane: &LaneRuntime,
    deferred: &DeferredScope,
    cx: &Context,
) -> Result<pi_ai::DeferredHandle, HarnessError> {
    let entry = lane
        .owner
        .session
        .get_entry(&deferred.source_entry_id, cx)
        .await
        .map_err(map_session_error)?
        .ok_or_else(|| invariant("deferred source entry is missing"))?;
    let message = entry
        .message()
        .and_then(assistant_message)
        .ok_or_else(|| invariant("deferred source entry is not assistant"))?;
    message
        .deferred
        .clone()
        .ok_or_else(|| invariant("deferred source entry has no handle"))
}

async fn recover_assistant_effect(
    lane: &LaneRuntime,
    operation: &Operation,
    _controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let (response_id, usage_id) = match &operation.state {
        OperationState::AssistantEffectPending {
            response_entry_id,
            usage_id,
            ..
        } => (response_entry_id.clone(), usage_id.clone()),
        _ => return Err(invariant("assistant recovery received another state")),
    };
    let frames = lane
        .owner
        .session
        .read_list(
            &pending_assistant_frames(&operation.meta.operation_id, &response_id),
            None,
            cx,
        )
        .await
        .map_err(map_session_error)?;
    let partial = if frames.is_empty() {
        let config = lane.data.lock().await.config.clone();
        let model = lane
            .owner
            .models
            .get_model(&config.model.provider, &config.model.model_id)
            .ok_or_else(|| HarnessError::Closed {
                message: "recovery model is unavailable".to_owned(),
            })?;
        let mut message = pi_ai::AssistantMessage::new(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            crate::message::now_millis(),
        );
        message.stop_reason = pi_ai::StopReason::Aborted;
        message.error_message = Some("assistant response interrupted before completion".to_owned());
        message
    } else {
        let frames = frames
            .into_iter()
            .map(|frame| frame.value)
            .collect::<Vec<_>>();
        let mut message =
            crate::harness::stream::reduce_persisted_frames(&frames).map_err(map_session_error)?;
        message.stop_reason = pi_ai::StopReason::Aborted;
        message.error_message = Some("assistant response interrupted during recovery".to_owned());
        message.timestamp = crate::message::now_millis();
        message
    };
    let assistant = assistant_agent_message(partial)?;
    let next = OperationState::Checkpoint {
        scope: operation.state.scope().clone(),
        data: CheckpointData {
            continuation: Continuation::MayFinish {
                include_final_assistant: true,
            },
            trigger_entry_id: response_id.clone(),
        },
    };
    let _record = commit_assistant(
        lane,
        operation,
        AssistantCommit {
            response_id,
            assistant,
            usage: pi_ai::Usage::default(),
            usage_id,
            next_state: next,
            status: TerminalStatus::Aborted,
            error: Some(OperationError {
                code: "interrupted".to_owned(),
                message: "assistant response was interrupted".to_owned(),
                details: None,
            }),
        },
        cx,
    )
    .await?;
    let record = settle(
        lane,
        operation,
        TerminalStatus::Aborted,
        Some(OperationError {
            code: "interrupted".to_owned(),
            message: "assistant response was interrupted".to_owned(),
            details: None,
        }),
        cx,
    )
    .await?;
    Ok(DriveStep::Settled(record))
}

async fn tools(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let current = CurrentOperation {
        lane,
        operation,
        controller,
        cx,
    };
    let mut batch = match &operation.state {
        OperationState::Tools { batch, .. } => batch.clone(),
        _ => return Err(invariant("tools dispatcher received another state")),
    };
    let assistant_entry = lane
        .owner
        .session
        .get_entry(&batch.assistant_entry_id, cx)
        .await
        .map_err(map_session_error)?
        .ok_or_else(|| invariant("tool batch assistant entry is missing"))?;
    let assistant = assistant_entry
        .message()
        .and_then(assistant_message)
        .ok_or_else(|| invariant("tool batch assistant entry is not assistant"))?;
    let calls = assistant_tool_calls(&assistant);
    let config = lane.owner.config_snapshot().await;
    let all_calls_are_parallel = operation.state.scope().settings.tool_execution
        == ToolExecutionMode::Parallel
        && calls.iter().all(|(source, _)| {
            batch
                .calls
                .iter()
                .find(|record| record.source_index == *source)
                .is_some_and(|record| {
                    matches!(
                        &record.status,
                        ToolCallStatus::Planned
                            | ToolCallStatus::EffectPending {
                                replay: ReplayPolicy::Safe
                            }
                    )
                })
        })
        && calls.iter().all(|(_, call)| {
            config
                .tools
                .iter()
                .find(|tool| tool.name() == call.name)
                .is_none_or(|tool| tool.execution_mode() == ToolExecutionMode::Parallel)
        });
    if all_calls_are_parallel && calls.len() > 1 {
        return parallel_tools(current, batch, &calls).await;
    }
    let mut tool_results = Vec::with_capacity(calls.len());
    let mut terminate = false;
    for (source, call) in calls {
        let index = batch
            .calls
            .iter()
            .position(|record| record.source_index == source)
            .ok_or_else(|| invariant("tool batch call index is missing"))?;
        let record = batch
            .calls
            .get(index)
            .cloned()
            .ok_or_else(|| invariant("tool batch call disappeared"))?;
        match run_sequential_tool(current, &mut batch, index, &record, &call).await? {
            SequentialToolOutcome::Ready(result, call_terminate) => {
                terminate = terminate || call_terminate;
                tool_results.push(result);
            }
            SequentialToolOutcome::Settled(record) => {
                return Ok(DriveStep::Settled(record));
            }
        }
    }
    let record = commit_tool_results(lane, operation, &batch, tool_results, terminate, cx).await?;
    if record.status == TerminalStatus::Completed {
        Ok(DriveStep::Continue)
    } else {
        Ok(DriveStep::Settled(record))
    }
}

enum SequentialToolOutcome {
    Ready(pi_ai::ToolResultMessage, bool),
    Settled(OperationResultRecord),
}

async fn run_sequential_tool(
    current: CurrentOperation<'_>,
    batch: &mut ToolBatch,
    index: usize,
    record: &ToolCall,
    call: &pi_ai::ToolCall,
) -> Result<SequentialToolOutcome, HarnessError> {
    match record.status.clone() {
        ToolCallStatus::Planned
        | ToolCallStatus::EffectPending {
            replay: ReplayPolicy::Safe,
        } => {
            let mut pending_batch = batch.clone();
            if let Some(record) = pending_batch.calls.get_mut(index) {
                record.status = ToolCallStatus::EffectPending {
                    replay: ReplayPolicy::Never,
                };
            }
            let args_write = set_json(
                &operation_tool_args(
                    &current.operation.meta.operation_id,
                    &batch.turn_id,
                    record.source_index,
                ),
                &call.arguments,
            )
            .map_err(map_session_error)?;
            transition_with_writes(
                current.lane,
                current.operation,
                OperationState::Tools {
                    scope: batch_scope(current.operation, &pending_batch),
                    batch: pending_batch.clone(),
                },
                vec![args_write],
                current.cx,
            )
            .await?;
            batch.clone_from(&pending_batch);
            let (result, call_terminate, durable) = execute_tool(
                current.lane,
                current.operation,
                batch,
                record,
                call,
                current.controller,
                current.cx,
            )
            .await?;
            let pending_output = crate::session::set_value(
                &pending_tool_output(
                    &current.operation.meta.operation_id,
                    &record.result_entry_id,
                ),
                &durable,
            )
            .map_err(map_session_error)?;
            current
                .lane
                .commit(vec![pending_output], current.cx)
                .await?;
            let mut ready_batch = batch.clone();
            if let Some(record) = ready_batch.calls.get_mut(index) {
                record.status = ToolCallStatus::OutcomeReady {
                    terminate: call_terminate,
                };
            }
            transition(
                current.lane,
                current.operation,
                OperationState::Tools {
                    scope: batch_scope(current.operation, &ready_batch),
                    batch: ready_batch.clone(),
                },
                current.cx,
            )
            .await?;
            batch.clone_from(&ready_batch);
            Ok(SequentialToolOutcome::Ready(result, call_terminate))
        }
        ToolCallStatus::EffectPending {
            replay: ReplayPolicy::Never,
        }
        | ToolCallStatus::OutcomeReady { .. }
        | ToolCallStatus::Completed { .. } => recover_sequential_tool(current, record, call).await,
    }
}

async fn recover_sequential_tool(
    current: CurrentOperation<'_>,
    record: &ToolCall,
    call: &pi_ai::ToolCall,
) -> Result<SequentialToolOutcome, HarnessError> {
    let staged = staged_tool_result(
        current.lane,
        &current.operation.meta.operation_id,
        &record.result_entry_id,
        current.cx,
    )
    .await?;
    let Some(staged) = staged else {
        let failure = settle(
            current.lane,
            current.operation,
            TerminalStatus::Failed,
            Some(OperationError {
                code: "tool_effect_unknown".to_owned(),
                message: format!(
                    "tool result {} is unavailable during recovery",
                    record.result_entry_id
                ),
                details: None,
            }),
            current.cx,
        )
        .await?;
        return Ok(SequentialToolOutcome::Settled(failure));
    };
    let call_terminate = staged.terminate.unwrap_or(false);
    Ok(SequentialToolOutcome::Ready(
        native_tool_result(call, &staged, call_terminate),
        call_terminate,
    ))
}

async fn parallel_tools(
    current: CurrentOperation<'_>,
    batch: ToolBatch,
    calls: &[(u32, pi_ai::ToolCall)],
) -> Result<DriveStep, HarnessError> {
    let mut pending_batch = prepare_parallel_tools(current, &batch, calls).await?;
    match execute_parallel_tools(current, &mut pending_batch, calls).await? {
        ParallelToolExecution::Step(step) => Ok(step),
        ParallelToolExecution::Completed(outcome) => {
            finish_parallel_tools(current.lane, &pending_batch, *outcome, current.cx).await
        }
    }
}

struct ParallelToolResults {
    operation: Operation,
    results: Vec<pi_ai::ToolResultMessage>,
    terminate: bool,
}

enum ParallelToolExecution {
    Completed(Box<ParallelToolResults>),
    Step(DriveStep),
}

async fn prepare_parallel_tools(
    current: CurrentOperation<'_>,
    batch: &ToolBatch,
    calls: &[(u32, pi_ai::ToolCall)],
) -> Result<ToolBatch, HarnessError> {
    let mut pending_batch = batch.clone();
    let mut writes = Vec::with_capacity(calls.len().saturating_mul(2));
    for (source, call) in calls {
        let index = pending_batch
            .calls
            .iter()
            .position(|record| record.source_index == *source)
            .ok_or_else(|| invariant("parallel tool batch call index is missing"))?;
        let turn_id = pending_batch.turn_id.clone();
        let record = pending_batch
            .calls
            .get_mut(index)
            .ok_or_else(|| invariant("parallel tool batch call disappeared"))?;
        if !matches!(
            &record.status,
            ToolCallStatus::Planned
                | ToolCallStatus::EffectPending {
                    replay: ReplayPolicy::Safe
                }
        ) {
            return Err(invariant("parallel tool batch contains an ineligible call"));
        }
        record.status = ToolCallStatus::EffectPending {
            replay: ReplayPolicy::Never,
        };
        writes.push(
            set_json(
                &operation_tool_args(&current.operation.meta.operation_id, &turn_id, *source),
                &call.arguments,
            )
            .map_err(map_session_error)?,
        );
    }
    transition_with_writes(
        current.lane,
        current.operation,
        OperationState::Tools {
            scope: batch_scope(current.operation, &pending_batch),
            batch: pending_batch.clone(),
        },
        writes,
        current.cx,
    )
    .await?;
    Ok(pending_batch)
}

async fn execute_parallel_tools(
    current: CurrentOperation<'_>,
    batch: &mut ToolBatch,
    calls: &[(u32, pi_ai::ToolCall)],
) -> Result<ParallelToolExecution, HarnessError> {
    let execution_batch = Arc::new(batch.clone());
    let mut running = FuturesUnordered::new();
    for (source, call) in calls {
        let index = execution_batch
            .calls
            .iter()
            .position(|record| record.source_index == *source)
            .ok_or_else(|| invariant("parallel tool call index is missing"))?;
        let record = execution_batch
            .calls
            .get(index)
            .cloned()
            .ok_or_else(|| invariant("parallel tool call disappeared"))?;
        let call = call.clone();
        let execution_batch = Arc::clone(&execution_batch);
        running.push(async move {
            (
                index,
                execute_tool(
                    current.lane,
                    current.operation,
                    execution_batch.as_ref(),
                    &record,
                    &call,
                    current.controller,
                    current.cx,
                )
                .await,
            )
        });
    }
    let mut results = Vec::with_capacity(batch.calls.len());
    results.resize_with(batch.calls.len(), || None);
    let mut terminate = false;
    while let Some((index, result)) = running.next().await {
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                drop(running);
                return Err(error);
            }
        };
        match commit_parallel_tool_result(current, batch, index, result).await? {
            ParallelToolResult::Ready {
                index,
                message,
                terminate: call_terminate,
            } => {
                results[index] = Some(message);
                terminate = terminate || call_terminate;
            }
            ParallelToolResult::Step(step) => {
                drop(running);
                return Ok(ParallelToolExecution::Step(step));
            }
        }
    }
    let results = results
        .into_iter()
        .map(|result| result.ok_or_else(|| invariant("parallel tool result is missing")))
        .collect::<Result<Vec<_>, _>>()?;
    let current_operation = current
        .lane
        .current_operation()
        .await
        .ok_or_else(|| invariant("parallel tool operation disappeared before commit"))?;
    if boundary_cancelled(current.lane, current.controller).await {
        return Ok(ParallelToolExecution::Step(
            reconcile_abort(
                current.lane,
                &current_operation,
                current.controller,
                current.cx,
            )
            .await?,
        ));
    }
    Ok(ParallelToolExecution::Completed(Box::new(
        ParallelToolResults {
            operation: current_operation,
            results,
            terminate,
        },
    )))
}

enum ParallelToolResult {
    Ready {
        index: usize,
        message: pi_ai::ToolResultMessage,
        terminate: bool,
    },
    Step(DriveStep),
}

async fn commit_parallel_tool_result(
    current: CurrentOperation<'_>,
    batch: &mut ToolBatch,
    index: usize,
    result: (pi_ai::ToolResultMessage, bool, AgentToolResult),
) -> Result<ParallelToolResult, HarnessError> {
    let (message, call_terminate, durable) = result;
    if boundary_cancelled(current.lane, current.controller).await {
        return Ok(ParallelToolResult::Step(
            reconcile_abort(
                current.lane,
                current.operation,
                current.controller,
                current.cx,
            )
            .await?,
        ));
    }
    let result_entry_id = batch
        .calls
        .get(index)
        .ok_or_else(|| invariant("parallel tool result call disappeared"))?
        .result_entry_id
        .clone();
    let pending_output = crate::session::set_value(
        &pending_tool_output(&current.operation.meta.operation_id, &result_entry_id),
        &durable,
    )
    .map_err(map_session_error)?;
    current
        .lane
        .commit(vec![pending_output], current.cx)
        .await?;
    let current_operation = current
        .lane
        .current_operation()
        .await
        .ok_or_else(|| invariant("parallel tool operation disappeared"))?;
    if matches!(
        &current_operation.state.scope().control,
        Control::CancelRequested { .. }
    ) {
        return Ok(ParallelToolResult::Step(
            reconcile_abort(
                current.lane,
                &current_operation,
                current.controller,
                current.cx,
            )
            .await?,
        ));
    }
    if let Some(record) = batch.calls.get_mut(index) {
        record.status = ToolCallStatus::OutcomeReady {
            terminate: call_terminate,
        };
    }
    transition(
        current.lane,
        &current_operation,
        OperationState::Tools {
            scope: batch_scope(&current_operation, batch),
            batch: batch.clone(),
        },
        current.cx,
    )
    .await?;
    Ok(ParallelToolResult::Ready {
        index,
        message,
        terminate: call_terminate,
    })
}

async fn finish_parallel_tools(
    lane: &LaneRuntime,
    batch: &ToolBatch,
    outcome: ParallelToolResults,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let record = commit_tool_results(
        lane,
        &outcome.operation,
        batch,
        outcome.results,
        outcome.terminate,
        cx,
    )
    .await?;
    if record.status == TerminalStatus::Completed {
        Ok(DriveStep::Continue)
    } else {
        Ok(DriveStep::Settled(record))
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_tool(
    lane: &LaneRuntime,
    operation: &Operation,
    batch: &ToolBatch,
    record: &ToolCall,
    call: &pi_ai::ToolCall,
    controller: &DriveController,
    cx: &Context,
) -> Result<(pi_ai::ToolResultMessage, bool, AgentToolResult), HarnessError> {
    let config = lane.owner.config_snapshot().await;
    let tool = config.tools.iter().find(|tool| tool.name() == call.name);
    let invocation = Invocation {
        lane,
        operation_id: operation.meta.operation_id.clone(),
        turn_id: batch.turn_id.clone(),
        entry_id: record.result_entry_id.clone(),
    };
    let updates = Arc::new(Mutex::new(Vec::<AgentToolResult>::new()));
    let updates_sink = crate::harness::tool::ToolUpdateSink::new({
        let updates = Arc::clone(&updates);
        move |result, _checkpoint| {
            if let Ok(mut values) = updates.lock() {
                values.push(result);
            }
        }
    });
    lane.emit(
        HarnessEventPayload::ToolStart {
            run_id: operation.meta.operation_id.clone(),
            turn_id: batch.turn_id.clone(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
        },
        cx,
    )
    .await;
    let (result, is_error) = if matches!(
        &operation.state.scope().control,
        Control::CancelRequested { .. }
    ) || controller.gate.token().is_cancelled()
    {
        (
            crate::tool::error_tool_result("tool execution aborted"),
            true,
        )
    } else if let Some(tool) = tool {
        let admitted_context = cx.with_cancellation(controller.gate.token().clone());
        let context = match &config.tool_context {
            Some(source) => Some(source(admitted_context.clone()).await?),
            None => None,
        };
        match admitted_context
            .race(tool.execute(
                &call.id,
                Value::Object(call.arguments.clone()),
                &updates_sink,
                context.as_ref(),
                &invocation,
                &admitted_context,
            ))
            .await
        {
            Ok(Ok(result)) => (result, false),
            Ok(Err(error)) => (crate::tool::error_tool_result(error.message()), true),
            Err(crate::context::Cancelled) => (
                crate::tool::error_tool_result("tool execution aborted"),
                true,
            ),
        }
    } else {
        (
            crate::tool::error_tool_result(format!("tool {} is not registered", call.name)),
            true,
        )
    };
    updates_sink.stop_accepting();
    let partials = updates
        .lock()
        .map(|values| values.clone())
        .unwrap_or_default();
    for partial in partials {
        lane.emit(
            HarnessEventPayload::ToolUpdate {
                run_id: operation.meta.operation_id.clone(),
                turn_id: batch.turn_id.clone(),
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                partial_result: partial,
            },
            cx,
        )
        .await;
    }
    let terminate = result.terminate.unwrap_or(false);
    let durable = AgentToolResult {
        content: result.content.clone(),
        details: result.details.clone(),
        added_tool_names: result.added_tool_names.clone(),
        terminate: Some(terminate),
    };
    let message = pi_ai::ToolResultMessage::new(
        call.id.clone(),
        call.name.clone(),
        result.content.clone(),
        is_error,
        crate::message::now_millis(),
    );
    lane.emit(
        HarnessEventPayload::ToolEnd {
            run_id: operation.meta.operation_id.clone(),
            turn_id: batch.turn_id.clone(),
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
            is_error,
            terminate,
        },
        cx,
    )
    .await;
    Ok((message, terminate, durable))
}

async fn staged_tool_result(
    lane: &LaneRuntime,
    operation_id: &OperationId,
    result_entry_id: &EntryId,
    cx: &Context,
) -> Result<Option<AgentToolResult>, HarnessError> {
    lane.owner
        .session
        .get_value(&pending_tool_output(operation_id, result_entry_id), cx)
        .await
        .map_err(map_session_error)
        .map(|stored| stored.map(|value| value.value))
}

fn native_tool_result(
    call: &pi_ai::ToolCall,
    result: &AgentToolResult,
    terminate: bool,
) -> pi_ai::ToolResultMessage {
    let mut message = pi_ai::ToolResultMessage::new(
        call.id.clone(),
        call.name.clone(),
        result.content.clone(),
        false,
        crate::message::now_millis(),
    );
    message.details = Some(result.details.clone());
    message
        .added_tool_names
        .clone_from(&result.added_tool_names);
    message.is_error = false;
    let _ = terminate;
    message
}

async fn commit_tool_results(
    lane: &LaneRuntime,
    operation: &Operation,
    batch: &ToolBatch,
    results: Vec<pi_ai::ToolResultMessage>,
    terminate: bool,
    cx: &Context,
) -> Result<OperationResultRecord, HarnessError> {
    let mut data = lane.data.lock().await;
    let mut parent = data.tip.clone();
    let mut writes = Vec::with_capacity(results.len().saturating_mul(2).saturating_add(2));
    let mut result_ids = Vec::with_capacity(results.len());
    for (index, result) in results.iter().enumerate() {
        let call = batch
            .calls
            .get(index)
            .ok_or_else(|| invariant("tool result order exceeds batch"))?;
        let message = super::support::tool_result_agent_message(result)?;
        writes.push(entry_write(
            call.result_entry_id.clone(),
            parent.clone(),
            message,
            result.is_error || terminate,
        ));
        writes.push(crate::session::delete_value(&pending_tool_output(
            &operation.meta.operation_id,
            &call.result_entry_id,
        )));
        result_ids.push(call.result_entry_id.clone());
        parent = Some(call.result_entry_id.clone());
    }
    let next = OperationState::Checkpoint {
        scope: batch_scope(operation, batch),
        data: CheckpointData {
            continuation: if terminate {
                Continuation::MayFinish {
                    include_final_assistant: false,
                }
            } else {
                Continuation::NeedAssistant {
                    overflow_recovery_used: false,
                }
            },
            trigger_entry_id: parent
                .clone()
                .ok_or_else(|| invariant("tool results have no tip"))?,
        },
    };
    writes.push(
        set_json(&operation_state(&operation.meta.operation_id), &next)
            .map_err(map_session_error)?,
    );
    writes.push(set_json(&branch_tip(lane.name.as_str()), &parent).map_err(map_session_error)?);
    lane.commit(writes, cx).await?;
    data.tip.clone_from(&parent);
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state: next,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    for result_id in result_ids {
        let entry = lane
            .owner
            .session
            .get_entry(&result_id, cx)
            .await
            .map_err(map_session_error)?
            .ok_or_else(|| invariant("tool result entry was not committed"))?;
        lane.emit(HarnessEventPayload::EntryAdded { entry }, cx)
            .await;
    }
    Ok(OperationResultRecord {
        operation_id: operation.meta.operation_id.clone(),
        kind: OperationKind::Run,
        status: TerminalStatus::Completed,
        error: None,
        from_tip_id: operation.meta.source_tip_id.clone(),
        tip_id: parent,
        started_at: operation.meta.started_at,
        ended_at: crate::message::now_millis(),
    })
}

fn batch_scope(operation: &Operation, batch: &ToolBatch) -> OperationScope {
    let mut scope = operation.state.scope().clone();
    scope.latest_assistant_entry_id = Some(batch.assistant_entry_id.clone());
    scope
}

async fn navigation(
    lane: &LaneRuntime,
    operation: &Operation,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let (target, label) = match &operation.state {
        OperationState::NavigationReadyToCommit {
            target_id, label, ..
        } => (target_id.clone(), label.clone()),
        _ => return Err(invariant("navigation dispatcher received another state")),
    };
    if let Some(target) = target.as_ref()
        && lane
            .owner
            .session
            .get_entry(target, cx)
            .await
            .map_err(map_session_error)?
            .is_none()
    {
        return Err(HarnessError::UnknownTarget {
            target_id: target.clone(),
            message: "navigation target disappeared".to_owned(),
        });
    }
    let mut data = lane.data.lock().await;
    let from = data.tip.clone();
    let mut writes =
        vec![set_json(&branch_tip(lane.name.as_str()), &target).map_err(map_session_error)?];
    if let Some(label) = label.as_ref() {
        writes.push(
            set_json(
                &super::support::label_address(target.as_ref().ok_or_else(|| {
                    HarnessError::InvalidNavigation {
                        lane: lane.name.clone(),
                        reason: "root_label".to_owned(),
                        message: "a root navigation cannot carry a label".to_owned(),
                    }
                })?),
                label,
            )
            .map_err(map_session_error)?,
        );
    }
    let record = OperationResultRecord {
        operation_id: operation.meta.operation_id.clone(),
        kind: OperationKind::Navigation,
        status: TerminalStatus::Completed,
        error: None,
        from_tip_id: from,
        tip_id: target.clone(),
        started_at: operation.meta.started_at,
        ended_at: crate::message::now_millis(),
    };
    let mut state = data.state.clone();
    state.current_operation_id = None;
    state.last_operation_id = Some(operation.meta.operation_id.clone());
    writes.push(
        set_json(&super::support::lane_state_address(&lane.name), &state)
            .map_err(map_session_error)?,
    );
    writes.push(
        set_json(
            &super::support::result_address(&operation.meta.operation_id),
            &record,
        )
        .map_err(map_session_error)?,
    );
    writes.extend(op_cleanup_writes(&operation.meta.operation_id, None));
    lane.commit(writes, cx).await?;
    data.tip.clone_from(&target);
    data.state = state;
    data.operation = None;
    data.last_result = Some(record.clone());
    drop(data);
    lane.emit(
        HarnessEventPayload::NavigationEnd {
            run_id: operation.meta.operation_id.clone(),
            status: TerminalStatus::Completed,
            from_tip_id: record.from_tip_id.clone(),
            tip_id: record.tip_id.clone(),
            error: None,
            ended_at: record.ended_at,
        },
        cx,
    )
    .await;
    lane.state_changed.notify_waiters();
    Ok(DriveStep::Settled(record))
}

enum StructuralSummary {
    Compaction(crate::harness::compaction::CompactResult),
    Branch(crate::harness::compaction::BranchSummaryResult),
}

async fn structural(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    _wait: bool,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let current = CurrentOperation {
        lane,
        operation,
        controller,
        cx,
    };
    let (generation, attempt) = match &operation.state {
        OperationState::SummaryDeciding { task, .. } => {
            return start_summary_generation(current, task).await;
        }
        OperationState::SummaryReady {
            generation,
            next_attempt,
            ..
        } => (generation.clone(), *next_attempt),
        OperationState::SummaryEffectPending {
            generation,
            attempt,
            ..
        } => (generation.clone(), *attempt),
        _ => return Err(invariant("structural dispatcher received another state")),
    };
    let durable =
        load_structural_preparation(lane, operation, &generation.task.task_id, cx).await?;
    let request_ref = ensure_summary_request(current, &generation, attempt).await?;
    let model = summary_model(lane, &generation)?;
    let summary_step = match &durable {
        crate::session::DurableStructuralPreparation::Compaction { .. } => {
            BeforeRequestStep::Compaction
        }
        crate::session::DurableStructuralPreparation::BranchSummary { .. } => {
            BeforeRequestStep::BranchSummary
        }
    };
    let request = summary_request(
        lane,
        &model,
        operation,
        controller,
        &generation.summary_context.stream_options,
        attempt,
        summary_step,
    );
    let result = match execute_summary(durable, &generation, model, &request, cx).await? {
        Ok(result) => result,
        Err((code, message)) => {
            return summary_failure(current, generation, attempt, code, message).await;
        }
    };
    match result {
        StructuralSummary::Compaction(result) => {
            commit_compaction(lane, operation, &generation, &request_ref, result, cx).await
        }
        StructuralSummary::Branch(result) => {
            commit_branch_summary(lane, operation, &generation, &request_ref, result, cx).await
        }
    }
}

async fn start_summary_generation(
    current: CurrentOperation<'_>,
    task: &crate::session::SummaryTask,
) -> Result<DriveStep, HarnessError> {
    let durable =
        load_structural_preparation(current.lane, current.operation, &task.task_id, current.cx)
            .await?;
    let navigation = matches!(
        &current.operation.meta.intent,
        OperationIntent::Navigation {
            summarize: true,
            ..
        }
    );
    match (&durable, navigation) {
        (crate::session::DurableStructuralPreparation::BranchSummary { .. }, true)
        | (crate::session::DurableStructuralPreparation::Compaction { .. }, false) => {}
        _ => {
            return Err(invariant(
                "summary preparation does not match operation intent",
            ));
        }
    }
    let config = current.lane.owner.config_snapshot().await;
    let Some(_model) = current
        .lane
        .owner
        .models
        .get_model(&config.model.provider, &config.model.id)
    else {
        let record = settle(
            current.lane,
            current.operation,
            TerminalStatus::Failed,
            Some(OperationError {
                code: "model_unavailable".to_owned(),
                message: format!(
                    "configured model {}/{} is unavailable",
                    config.model.provider, config.model.id
                ),
                details: None,
            }),
            current.cx,
        )
        .await?;
        return Ok(DriveStep::Settled(record));
    };
    let result_entry_id =
        new_entry_id(current.lane.owner.session.as_ref()).map_err(map_session_error)?;
    let generation = SummaryGenerationScope {
        task: task.clone(),
        summary_context: SummaryContext {
            result_entry_id,
            configuration: captured_configuration(&config),
            stream_options: config.stream_options.clone(),
            retry_policy: super::support::normalized_retry(config.retry)?,
        },
    };
    let next = OperationState::SummaryReady {
        scope: current.operation.state.scope().clone(),
        generation,
        next_attempt: 1,
    };
    transition(current.lane, current.operation, next, current.cx).await?;
    Ok(DriveStep::Continue)
}

async fn ensure_summary_request(
    current: CurrentOperation<'_>,
    generation: &SummaryGenerationScope,
    attempt: u64,
) -> Result<SummaryRequestRef, HarnessError> {
    if let OperationState::SummaryEffectPending {
        request: Some(request),
        ..
    } = &current.operation.state
    {
        return Ok(request.clone());
    }
    let usage_id = new_usage_id(current.lane.owner.session.as_ref()).map_err(map_session_error)?;
    let index = u32::try_from(attempt.saturating_sub(1)).map_err(|_| HarnessError::Closed {
        message: "summary request index overflow".to_owned(),
    })?;
    let request_ref = SummaryRequestRef { index, usage_id };
    let usage_ids = match &current.operation.state {
        OperationState::SummaryEffectPending { usage_ids, .. } => usage_ids.clone(),
        _ => Vec::new(),
    };
    let next = OperationState::SummaryEffectPending {
        scope: current.operation.state.scope().clone(),
        generation: generation.clone(),
        attempt,
        request: Some(request_ref.clone()),
        usage_ids,
    };
    transition(current.lane, current.operation, next, current.cx).await?;
    Ok(request_ref)
}

fn summary_model(
    lane: &LaneRuntime,
    generation: &SummaryGenerationScope,
) -> Result<pi_ai::Model, HarnessError> {
    lane.owner
        .models
        .get_model(
            &generation.summary_context.configuration.model.provider,
            &generation.summary_context.configuration.model.model_id,
        )
        .ok_or_else(|| HarnessError::Closed {
            message: format!(
                "summary model {}/{} is unavailable",
                generation.summary_context.configuration.model.provider,
                generation.summary_context.configuration.model.model_id
            ),
        })
}

async fn execute_summary(
    durable: crate::session::DurableStructuralPreparation,
    generation: &SummaryGenerationScope,
    model: pi_ai::Model,
    request: &crate::harness::compaction::SummaryRequest,
    cx: &Context,
) -> Result<Result<StructuralSummary, (String, String)>, HarnessError> {
    match durable {
        crate::session::DurableStructuralPreparation::Compaction { .. } => {
            let preparation =
                crate::harness::compaction::CompactionPreparation::from_durable(durable)
                    .ok_or_else(|| invariant("compaction preparation changed during drive"))?;
            let options = crate::harness::compaction::CompactGenerationOptions {
                model,
                custom_instructions: generation.task.custom_instructions.clone(),
                thinking_level: Some(generation.summary_context.configuration.thinking_level),
            };
            Ok(crate::harness::compaction::compact_with_request(
                &preparation,
                &options,
                request,
                cx,
            )
            .await
            .map(StructuralSummary::Compaction)
            .map_err(|error| (error.code().to_owned(), error.to_string())))
        }
        crate::session::DurableStructuralPreparation::BranchSummary { .. } => {
            let preparation = crate::harness::compaction::BranchPreparation::from_durable(durable)
                .ok_or_else(|| invariant("branch preparation changed during drive"))?;
            Ok(
                crate::harness::compaction::generate_branch_summary_with_request(
                    &preparation,
                    &crate::harness::compaction::PreparedBranchSummaryOptions {
                        custom_instructions: generation.task.custom_instructions.clone(),
                        replace_instructions: false,
                    },
                    request,
                    cx,
                )
                .await
                .map(StructuralSummary::Branch)
                .map_err(|error| (error.code().to_owned(), error.to_string())),
            )
        }
    }
}

async fn summary_failure(
    current: CurrentOperation<'_>,
    generation: SummaryGenerationScope,
    attempt: u64,
    code: String,
    message: String,
) -> Result<DriveStep, HarnessError> {
    if boundary_cancelled(current.lane, current.controller).await {
        return reconcile_abort(
            current.lane,
            current.operation,
            current.controller,
            current.cx,
        )
        .await;
    }
    let retry_policy = &generation.summary_context.retry_policy;
    if attempt < retry_policy.max_attempts {
        let delay = retry_delay(retry_policy.base_delay_ms, attempt)?;
        let not_before = crate::message::now_millis()
            .checked_add(i64::try_from(delay).map_err(|_| HarnessError::Closed {
                message: "summary retry delay exceeds timestamp range".to_owned(),
            })?)
            .ok_or_else(|| HarnessError::Closed {
                message: "summary retry timestamp overflow".to_owned(),
            })?;
        let next = OperationState::SummaryRetryWait {
            scope: current.operation.state.scope().clone(),
            generation,
            retry: RetryWait {
                next_attempt: attempt.checked_add(1).ok_or_else(|| HarnessError::Closed {
                    message: "summary retry attempt overflow".to_owned(),
                })?,
                not_before,
                error_message: message,
            },
        };
        transition(current.lane, current.operation, next, current.cx).await?;
        return Ok(DriveStep::WaitingRetry { not_before });
    }
    let record = settle(
        current.lane,
        current.operation,
        TerminalStatus::Failed,
        Some(OperationError {
            code,
            message,
            details: None,
        }),
        current.cx,
    )
    .await?;
    Ok(DriveStep::Settled(record))
}

async fn load_structural_preparation(
    lane: &LaneRuntime,
    operation: &Operation,
    task_id: &str,
    cx: &Context,
) -> Result<crate::session::DurableStructuralPreparation, HarnessError> {
    lane.owner
        .session
        .get_value(
            &super::support::preparation_address(&operation.meta.operation_id, task_id),
            cx,
        )
        .await
        .map_err(map_session_error)?
        .map(|stored| stored.value)
        .ok_or_else(|| invariant("summary preparation is missing"))
}

fn summary_request(
    lane: &LaneRuntime,
    model: &pi_ai::Model,
    operation: &Operation,
    controller: &DriveController,
    stream_options: &HarnessStreamOptions,
    attempt: u64,
    step: BeforeRequestStep,
) -> crate::harness::compaction::SummaryRequest {
    let models = Arc::clone(&lane.owner.models);
    let hooks = lane.owner.hooks.clone();
    let lane_name = lane.name.clone();
    let run_id = operation.meta.operation_id.to_string();
    let gate = controller.gate.clone();
    let model = model.clone();
    let mut base_options = stream_options.clone();
    base_options.deferred = Some(crate::session::configuration::DeferredRequest::Flag(false));
    Arc::new(move |context, mut options, request_cx| {
        let models = Arc::clone(&models);
        let hooks = hooks.clone();
        let lane_name = lane_name.clone();
        let run_id = run_id.clone();
        let gate = gate.clone();
        let model = model.clone();
        let base_options = base_options.clone();
        Box::pin(async move {
            let event = BeforeRequestEvent {
                lane: lane_name.clone(),
                run_id: run_id.clone(),
                model: model.clone(),
                step,
                attempt,
                stream_options: base_options.clone(),
            };
            let effective = match hooks
                .run_with_gate::<crate::harness::hooks::BeforeRequest>(event, &gate, &request_cx)
                .await
            {
                Ok(Some(result)) => match result.stream_options {
                    Some(patch) => apply_stream_options_patch(&base_options, &patch),
                    None => base_options,
                },
                Ok(None) => base_options,
                Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
                    abort.wait().await;
                    return Err(pi_ai::ProviderError::new("summary request aborted"));
                }
                Err(HookRunError::Gate(GateRejection::Closed(fault))) => {
                    return Err(pi_ai::ProviderError::new(fault.message.clone()));
                }
                Err(HookRunError::Handler(error)) => {
                    return Err(pi_ai::ProviderError::new(error.message));
                }
            };
            options.transport = effective.transport;
            options.cache_retention = effective.cache_retention;
            options.timeout_ms = effective.timeout_ms;
            options.max_retries = effective.max_retries;
            options.max_retry_delay_ms = effective.max_retry_delay_ms;
            options.headers = effective.headers.as_ref().map(|headers| {
                headers
                    .iter()
                    .map(|(key, value)| (key.clone(), Some(value.clone())))
                    .collect()
            });
            options.metadata = effective.metadata.as_ref().map(|metadata| {
                metadata
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            });
            options.on_payload =
                before_payload_callback_owned(hooks, lane_name, run_id, gate, request_cx.clone());
            crate::harness::compaction::complete_simple_with_retries(
                &models,
                &model,
                context,
                options,
                None,
                None,
                &request_cx,
            )
            .await
        })
    })
}

async fn commit_compaction(
    lane: &LaneRuntime,
    operation: &Operation,
    generation: &SummaryGenerationScope,
    request_ref: &SummaryRequestRef,
    result: crate::harness::hooks::CompactResult,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let summary_id = generation.summary_context.result_entry_id.clone();
    let mut data = lane.data.lock().await;
    let (writes, next_checkpoint) = compaction_writes(
        lane,
        operation,
        generation,
        request_ref,
        result,
        data.tip.clone(),
        &summary_id,
    )?;
    lane.commit(writes, cx).await?;
    data.tip = Some(summary_id.clone());
    if let Some(next) = next_checkpoint {
        data.operation = Some(Operation {
            meta: operation.meta.clone(),
            state: next,
        });
    }
    drop(data);
    lane.state_changed.notify_waiters();
    let entry = lane
        .owner
        .session
        .get_entry(&summary_id, cx)
        .await
        .map_err(map_session_error)?
        .ok_or_else(|| invariant("compaction entry was not committed"))?;
    lane.emit(HarnessEventPayload::EntryAdded { entry }, cx)
        .await;
    if matches!(
        &generation.task.boundary,
        crate::session::ResultBoundary::ResumeCheckpoint { .. }
    ) {
        lane.emit(
            HarnessEventPayload::CompactionEnd {
                run_id: operation.meta.operation_id.clone(),
                reason: generation
                    .task
                    .reason
                    .unwrap_or(crate::session::CompactionReason::Manual),
                status: TerminalStatus::Completed,
                entry_id: Some(summary_id),
                error: None,
                ended_at: crate::message::now_millis(),
            },
            cx,
        )
        .await;
        return Ok(DriveStep::Continue);
    }
    let record = settle(lane, operation, TerminalStatus::Completed, None, cx).await?;
    Ok(DriveStep::Settled(record))
}

fn compaction_writes(
    lane: &LaneRuntime,
    operation: &Operation,
    generation: &SummaryGenerationScope,
    request_ref: &SummaryRequestRef,
    result: crate::harness::hooks::CompactResult,
    parent: Option<EntryId>,
    summary_id: &EntryId,
) -> Result<(Vec<Write>, Option<OperationState>), HarnessError> {
    let usage = result.usage.clone();
    let next_checkpoint = match &generation.task.boundary {
        crate::session::ResultBoundary::ResumeCheckpoint { resume_after } => {
            let mut resume_after = resume_after.clone();
            if matches!(
                &resume_after.continuation,
                Continuation::NeedAssistant {
                    overflow_recovery_used: true
                }
            ) {
                resume_after.trigger_entry_id = summary_id.clone();
            }
            Some(OperationState::Checkpoint {
                scope: operation.state.scope().clone(),
                data: resume_after,
            })
        }
        _ => None,
    };
    let entry = Write::Entry {
        entry: crate::session::NewEntry {
            id: summary_id.clone(),
            parent_id: parent,
            body: crate::session::NewEntryBody::Compaction {
                summary: result.summary,
                retained_tail: result.retained_tail,
                tokens_before: result.tokens_before,
                details: result.details,
                usage: usage.clone(),
                from_hook: false,
            },
        },
    };
    let mut writes = vec![
        entry,
        set_json(&branch_tip(lane.name.as_str()), &Some(summary_id.clone()))
            .map_err(map_session_error)?,
    ];
    if let Some(usage) = usage {
        writes.push(Write::Usage {
            row: NewUsageRow {
                id: request_ref.usage_id.clone(),
                usage,
                entry_id: Some(summary_id.clone()),
                adjustment: false,
                details: None,
            },
        });
    }
    if let Some(next) = next_checkpoint.as_ref() {
        writes.push(
            set_json(&operation_state(&operation.meta.operation_id), next)
                .map_err(map_session_error)?,
        );
    }
    Ok((writes, next_checkpoint))
}

async fn commit_branch_summary(
    lane: &LaneRuntime,
    operation: &Operation,
    generation: &SummaryGenerationScope,
    request_ref: &SummaryRequestRef,
    result: crate::harness::compaction::BranchSummaryResult,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    let (target_id, label) = match &operation.meta.intent {
        OperationIntent::Navigation {
            target_id,
            label,
            summarize: true,
            ..
        } => (target_id.clone(), label.clone()),
        _ => return Err(invariant("branch summary belongs to navigation")),
    };
    let old_tip = lane.data.lock().await.tip.clone();
    let from_id = match (old_tip.as_ref(), target_id.as_ref()) {
        (Some(old_tip), Some(target_id)) => {
            crate::harness::compaction::collect_entries_for_branch_summary(
                lane.branch(cx).await?.as_ref(),
                lane.owner.session.as_ref(),
                Some(old_tip),
                target_id,
                cx,
            )
            .await
            .map_err(map_session_error)?
            .common_ancestor_id
        }
        _ => None,
    };
    let summary_id = generation.summary_context.result_entry_id.clone();
    let usage = result.usage.clone();
    let details = serde_json::json!({
        "readFiles": result.read_files,
        "modifiedFiles": result.modified_files,
    });
    let entry = Write::Entry {
        entry: crate::session::NewEntry {
            id: summary_id.clone(),
            parent_id: old_tip,
            body: crate::session::NewEntryBody::BranchSummary {
                from_id,
                summary: result.summary,
                details: Some(details),
                usage: usage.clone(),
                from_hook: false,
            },
        },
    };
    let next_state = OperationState::NavigationReadyToCommit {
        scope: operation.state.scope().clone(),
        target_id,
        label,
    };
    let mut writes = vec![
        entry,
        set_json(&branch_tip(lane.name.as_str()), &Some(summary_id.clone()))
            .map_err(map_session_error)?,
        set_json(&operation_state(&operation.meta.operation_id), &next_state)
            .map_err(map_session_error)?,
    ];
    if let Some(usage) = usage {
        writes.push(Write::Usage {
            row: NewUsageRow {
                id: request_ref.usage_id.clone(),
                usage,
                entry_id: Some(summary_id.clone()),
                adjustment: false,
                details: None,
            },
        });
    }
    lane.commit(writes, cx).await?;
    let mut data = lane.data.lock().await;
    data.tip = Some(summary_id.clone());
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state: next_state,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    let entry = lane
        .owner
        .session
        .get_entry(&summary_id, cx)
        .await
        .map_err(map_session_error)?
        .ok_or_else(|| invariant("branch summary entry was not committed"))?;
    lane.emit(HarnessEventPayload::EntryAdded { entry }, cx)
        .await;
    Ok(DriveStep::Continue)
}

async fn cancel_deferred_best_effort(
    lane: &LaneRuntime,
    deferred: &DeferredScope,
    controller: &DriveController,
    cx: &Context,
) -> Result<(), HarnessError> {
    let handle = deferred_handle(lane, deferred, cx).await?;
    let Some(model) = lane.owner.models.get_model(
        &deferred.configuration.model.provider,
        &deferred.configuration.model.model_id,
    ) else {
        return Ok(());
    };
    let session_id = format!(
        "{}:{}",
        lane.owner.session.metadata().id,
        lane.name.as_str()
    );
    let on_response: pi_ai::provider::OnResponseFn =
        Arc::new(|_response, _model| Box::pin(std::future::ready(Ok(()))));
    let options = native_stream_options(
        &deferred.stream_options,
        deferred.configuration.thinking_level,
        session_id,
        Some(controller.close_signal.clone()),
        None,
        on_response,
    );
    let _ = lane
        .owner
        .models
        .cancel_deferred(&model, handle, options)
        .await;
    Ok(())
}

async fn reconcile_abort(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    controller
        .control
        .begin_abort(futures::future::ready(()).boxed().shared());
    controller.control.signal_abort();
    let deferred = match &operation.state {
        OperationState::DeferredSuspended { deferred }
        | OperationState::DeferredEffectPending { deferred, .. } => Some(deferred.clone()),
        _ => None,
    };
    if let Some(deferred) = deferred {
        let _ = cancel_deferred_best_effort(lane, &deferred, controller, cx).await;
    }
    if matches!(
        &operation.state,
        OperationState::AssistantEffectPending { .. }
    ) {
        return recover_assistant_effect(lane, operation, controller, cx).await;
    }
    let record = settle(
        lane,
        operation,
        TerminalStatus::Aborted,
        Some(OperationError {
            code: "aborted".to_owned(),
            message: "operation aborted".to_owned(),
            details: None,
        }),
        cx,
    )
    .await?;
    Ok(DriveStep::Settled(record))
}

async fn publish_interrupted(
    lane: &LaneRuntime,
    operation: &Operation,
    response_id: &EntryId,
    cx: &Context,
) -> Result<OperationResultRecord, HarnessError> {
    let message = lane
        .owner
        .session
        .get_entry(response_id, cx)
        .await
        .map_err(map_session_error)?;
    let _ = message;
    settle(
        lane,
        operation,
        TerminalStatus::Aborted,
        Some(OperationError {
            code: "aborted".to_owned(),
            message: "assistant request aborted".to_owned(),
            details: None,
        }),
        cx,
    )
    .await
}

async fn operation_cleanup_writes(
    lane: &LaneRuntime,
    operation_id: &OperationId,
    response_id: Option<&EntryId>,
    cx: &Context,
) -> Result<Vec<Write>, HarnessError> {
    let mut writes = op_cleanup_writes(operation_id, response_id);
    let preparations = lane
        .owner
        .session
        .scan_values(&operation_preparation_prefix(operation_id), cx)
        .await
        .map_err(map_session_error)?;
    writes.extend(
        preparations
            .into_iter()
            .map(|value| crate::session::delete_value(&value.address)),
    );
    let tool_args = lane
        .owner
        .session
        .scan_values(&operation_tool_args_prefix(operation_id, None), cx)
        .await
        .map_err(map_session_error)?;
    writes.extend(
        tool_args
            .into_iter()
            .map(|value| crate::session::delete_value(&value.address)),
    );
    let tool_memos = lane
        .owner
        .session
        .scan_values(&operation_tool_memo_prefix(operation_id, None), cx)
        .await
        .map_err(map_session_error)?;
    writes.extend(
        tool_memos
            .into_iter()
            .map(|value| crate::session::delete_value(&value.address)),
    );
    let pending_outputs = lane
        .owner
        .session
        .scan_values(&pending_tool_output_prefix(operation_id), cx)
        .await
        .map_err(map_session_error)?;
    writes.extend(
        pending_outputs
            .into_iter()
            .map(|value| crate::session::delete_value(&value.address)),
    );
    Ok(writes)
}

async fn settle(
    lane: &LaneRuntime,
    operation: &Operation,
    status: TerminalStatus,
    error: Option<OperationError>,
    cx: &Context,
) -> Result<OperationResultRecord, HarnessError> {
    let response_id = response_id_of(&operation.state);
    let cleanup =
        operation_cleanup_writes(lane, &operation.meta.operation_id, response_id.as_ref(), cx)
            .await?;
    let mut data = lane.data.lock().await;
    let record = OperationResultRecord {
        operation_id: operation.meta.operation_id.clone(),
        kind: super::support::operation_kind(&operation.meta.intent),
        status,
        error: error.clone(),
        from_tip_id: operation.meta.source_tip_id.clone(),
        tip_id: data.tip.clone(),
        started_at: operation.meta.started_at,
        ended_at: crate::message::now_millis(),
    };
    let mut state = data.state.clone();
    state.current_operation_id = None;
    state.last_operation_id = Some(operation.meta.operation_id.clone());
    let mut writes = vec![
        set_json(&super::support::lane_state_address(&lane.name), &state)
            .map_err(map_session_error)?,
        set_json(
            &super::support::result_address(&operation.meta.operation_id),
            &record,
        )
        .map_err(map_session_error)?,
    ];
    writes.extend(cleanup);
    lane.commit(writes, cx).await?;
    data.operation = None;
    data.last_result = Some(record.clone());
    let payload = match record.kind {
        OperationKind::Run => HarnessEventPayload::RunEnd {
            run_id: record.operation_id.clone(),
            status: record.status,
            from_tip_id: record.from_tip_id.clone(),
            tip_id: record.tip_id.clone(),
            ended_at: record.ended_at,
            error: record.error.clone(),
        },
        OperationKind::Compaction => {
            let (reason, entry_id) = match &operation.state {
                OperationState::SummaryEffectPending { generation, .. }
                | OperationState::SummaryReady { generation, .. }
                | OperationState::SummaryRetryWait { generation, .. } => (
                    generation
                        .task
                        .reason
                        .unwrap_or(crate::session::CompactionReason::Manual),
                    Some(generation.summary_context.result_entry_id.clone()),
                ),
                _ => (crate::session::CompactionReason::Manual, None),
            };
            HarnessEventPayload::CompactionEnd {
                run_id: record.operation_id.clone(),
                reason,
                status: record.status,
                entry_id,
                error: record.error.clone(),
                ended_at: record.ended_at,
            }
        }
        OperationKind::Navigation => HarnessEventPayload::NavigationEnd {
            run_id: record.operation_id.clone(),
            status: record.status,
            from_tip_id: record.from_tip_id.clone(),
            tip_id: record.tip_id.clone(),
            error: record.error.clone(),
            ended_at: record.ended_at,
        },
    };
    lane.emit(payload, cx).await;
    lane.state_changed.notify_waiters();
    lane.idle.notify_waiters();
    Ok(record)
}

fn response_id_of(state: &OperationState) -> Option<EntryId> {
    match state {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => Some(response_entry_id.clone()),
        _ => None,
    }
}

async fn transition(
    lane: &LaneRuntime,
    operation: &Operation,
    state: OperationState,
    cx: &Context,
) -> Result<(), HarnessError> {
    let mut data = lane.data.lock().await;
    let current = data
        .operation
        .as_ref()
        .ok_or_else(|| invariant("operation disappeared during transition"))?;
    if current.meta.operation_id != operation.meta.operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: operation.meta.operation_id.clone(),
            current_operation_id: Some(current.meta.operation_id.clone()),
            last_operation_id: data.state.last_operation_id.clone(),
            message: "operation changed during transition".to_owned(),
        });
    }
    let write = set_json(&operation_state(&operation.meta.operation_id), &state)
        .map_err(map_session_error)?;
    lane.commit(vec![write], cx).await?;
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    Ok(())
}
async fn transition_with_writes(
    lane: &LaneRuntime,
    operation: &Operation,
    state: OperationState,
    mut writes: Vec<Write>,
    cx: &Context,
) -> Result<(), HarnessError> {
    let mut data = lane.data.lock().await;
    let current = data
        .operation
        .as_ref()
        .ok_or_else(|| invariant("operation disappeared during transition"))?;
    if current.meta.operation_id != operation.meta.operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: operation.meta.operation_id.clone(),
            current_operation_id: Some(current.meta.operation_id.clone()),
            last_operation_id: data.state.last_operation_id.clone(),
            message: "operation changed during transition".to_owned(),
        });
    }
    writes.push(
        set_json(&operation_state(&operation.meta.operation_id), &state)
            .map_err(map_session_error)?,
    );
    lane.commit(writes, cx).await?;
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    Ok(())
}

async fn transition_with_preparation(
    lane: &LaneRuntime,
    operation: &Operation,
    task_id: &str,
    preparation: crate::session::DurableStructuralPreparation,
    state: OperationState,
    cx: &Context,
) -> Result<(), HarnessError> {
    let mut data = lane.data.lock().await;
    let current = data
        .operation
        .as_ref()
        .ok_or_else(|| invariant("operation disappeared during preparation"))?;
    if current.meta.operation_id != operation.meta.operation_id {
        return Err(HarnessError::OperationMismatch {
            lane: lane.name.clone(),
            expected_operation_id: operation.meta.operation_id.clone(),
            current_operation_id: Some(current.meta.operation_id.clone()),
            last_operation_id: data.state.last_operation_id.clone(),
            message: "operation changed during preparation".to_owned(),
        });
    }
    let writes = vec![
        set_json(
            &super::support::preparation_address(&operation.meta.operation_id, task_id),
            &preparation,
        )
        .map_err(map_session_error)?,
        set_json(&operation_state(&operation.meta.operation_id), &state)
            .map_err(map_session_error)?,
    ];
    lane.commit(writes, cx).await?;
    data.operation = Some(Operation {
        meta: operation.meta.clone(),
        state,
    });
    drop(data);
    lane.state_changed.notify_waiters();
    Ok(())
}

/// Inbox entries admitted at an operation boundary.
struct InboxDrain {
    /// Message payloads admitted from the inbox, in commit order.
    messages: Vec<AgentMessage>,
    /// Last admitted entry able to trigger a new assistant step: a message or
    /// a custom entry whose type has a registered projector.
    trigger: Option<EntryId>,
    /// Whether the drain committed any entries.
    committed: bool,
}

async fn drain_inbox(
    lane: &LaneRuntime,
    operation: &Operation,
    cx: &Context,
) -> Result<InboxDrain, HarnessError> {
    let mut data = lane.data.lock().await;
    let selected = select_inbox(
        &data.state.inbox,
        operation.state.scope().settings.steering_mode,
        operation.state.scope().settings.follow_up_mode,
    );
    if selected.is_empty() {
        return Ok(InboxDrain {
            messages: Vec::new(),
            trigger: None,
            committed: false,
        });
    }
    let entry_projectors = lane.owner.config_snapshot().await.entry_projectors;
    let mutator = lane
        .owner
        .session
        .begin_mutation(cx)
        .await
        .map_err(map_session_error)?;
    let pending = super::support::read_pending(&*mutator, &selected, cx)
        .await
        .map_err(map_session_error)?;
    let mut parent = data.tip.clone();
    let mut drain = InboxDrain {
        messages: Vec::new(),
        trigger: None,
        committed: true,
    };
    let mut writes = Vec::new();
    for (item, value) in pending {
        match value {
            PendingEntry::Message { payload } => {
                drain.trigger = Some(item.entry_id.clone());
                drain.messages.push(payload.clone());
                writes.push(entry_write(
                    item.entry_id.clone(),
                    parent.clone(),
                    payload,
                    false,
                ));
            }
            PendingEntry::Custom {
                custom_type,
                payload,
            } => {
                if entry_projectors.contains_key(&custom_type) {
                    drain.trigger = Some(item.entry_id.clone());
                }
                writes.push(super::support::custom_entry_write(
                    item.entry_id.clone(),
                    parent.clone(),
                    custom_type,
                    payload,
                ));
            }
        }
        parent = Some(item.entry_id.clone());
        writes.push(crate::session::delete_value(
            &crate::session::address::pending_entry(&item.entry_id),
        ));
    }
    let next_inbox = data
        .state
        .inbox
        .iter()
        .filter(|item| {
            !selected
                .iter()
                .any(|chosen| chosen.entry_id == item.entry_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let next_state = LaneState {
        current_operation_id: Some(operation.meta.operation_id.clone()),
        last_operation_id: data.state.last_operation_id.clone(),
        inbox: next_inbox,
    };
    writes.push(
        set_json(&super::support::lane_state_address(&lane.name), &next_state)
            .map_err(map_session_error)?,
    );
    writes.push(set_json(&branch_tip(lane.name.as_str()), &parent).map_err(map_session_error)?);
    mutator
        .commit(writes, cx)
        .await
        .map_err(map_session_error)?;
    data.tip = parent;
    data.state = next_state;
    drop(data);
    lane.state_changed.notify_waiters();
    Ok(drain)
}

fn select_inbox(
    inbox: &[crate::session::InboxItem],
    steering: crate::queue::QueueMode,
    follow_up: crate::queue::QueueMode,
) -> Vec<crate::session::InboxItem> {
    let mut selected = Vec::new();
    let mut steer = false;
    let mut follow = false;
    for item in inbox {
        let allowed = match item.kind {
            crate::session::InboxItemKind::Write | crate::session::InboxItemKind::NextRun => true,
            crate::session::InboxItemKind::Steer => {
                steering == crate::queue::QueueMode::All || !steer
            }
            crate::session::InboxItemKind::FollowUp => {
                follow_up == crate::queue::QueueMode::All || !follow
            }
        };
        if allowed {
            match item.kind {
                crate::session::InboxItemKind::Steer => steer = true,
                crate::session::InboxItemKind::FollowUp => follow = true,
                crate::session::InboxItemKind::Write | crate::session::InboxItemKind::NextRun => {}
            }
            selected.push(item.clone());
        }
    }
    selected
}

async fn read_entries_by_id(
    lane: &LaneRuntime,
    ids: &[EntryId],
    cx: &Context,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let entries = lane
        .owner
        .session
        .get_entries(ids, cx)
        .await
        .map_err(map_session_error)?;
    Ok(ids
        .iter()
        .filter_map(|id| entries.get(id).and_then(|entry| entry.message().cloned()))
        .collect())
}

async fn append_injected_messages(
    lane: &LaneRuntime,
    messages: Vec<AgentMessage>,
    cx: &Context,
) -> Result<(), HarnessError> {
    let mut data = lane.data.lock().await;
    let mut parent = data.tip.clone();
    let mut writes = Vec::new();
    for message in messages {
        let id = new_entry_id(lane.owner.session.as_ref()).map_err(map_session_error)?;
        writes.push(entry_write(id.clone(), parent.clone(), message, false));
        parent = Some(id);
    }
    writes.push(set_json(&branch_tip(lane.name.as_str()), &parent).map_err(map_session_error)?);
    lane.commit(writes, cx).await?;
    data.tip = parent;
    drop(data);
    Ok(())
}

async fn configured_tools(
    lane: &LaneRuntime,
    names: &[String],
) -> Result<Vec<pi_ai::Tool>, HarnessError> {
    let config = lane.owner.config_snapshot().await;
    let mut tools = Vec::new();
    for name in names {
        let tool = config
            .tools
            .iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| HarnessError::Closed {
                message: format!("configured tool {name} is unavailable"),
            })?;
        tools.push(crate::harness::tool::to_provider_tool(tool.as_ref()));
    }
    Ok(tools)
}

async fn resolve_system_prompt(
    lane: &LaneRuntime,
    controller: &DriveController,
    cx: &Context,
) -> Result<String, HarnessError> {
    let config = lane.owner.config_snapshot().await;
    let Some(source) = config.system_prompt else {
        return Ok(String::new());
    };
    let context = match &config.tool_context {
        Some(tool_context) => Some(tool_context(cx.clone()).await?),
        None => None,
    };
    let future = controller
        .gate
        .admit(|| {
            source(
                context,
                cx.with_cancellation(controller.gate.token().clone()),
            )
        })
        .map_err(|error| match error {
            GateRejection::Closed(fault) => HarnessError::Closed {
                message: fault.message.clone(),
            },
            GateRejection::Aborted(_) => HarnessError::Closed {
                message: "system prompt was aborted".to_owned(),
            },
        })?;
    future.await
}

async fn before_request_options(
    current: CurrentOperation<'_>,
    model: &pi_ai::Model,
    step: BeforeRequestStep,
    attempt: u64,
    stream_options: HarnessStreamOptions,
) -> Result<HarnessStreamOptions, HookRunError> {
    if !current
        .lane
        .owner
        .hooks
        .has::<crate::harness::hooks::BeforeRequest>()
    {
        return Ok(stream_options);
    }
    let event = BeforeRequestEvent {
        lane: current.lane.name.clone(),
        run_id: current.operation.meta.operation_id.to_string(),
        model: model.clone(),
        step,
        attempt,
        stream_options: stream_options.clone(),
    };
    let result = current
        .lane
        .owner
        .hooks
        .run_with_gate::<crate::harness::hooks::BeforeRequest>(
            event,
            &current.controller.gate,
            current.cx,
        )
        .await?;
    Ok(match result.and_then(|result| result.stream_options) {
        Some(patch) => apply_stream_options_patch(&stream_options, &patch),
        None => stream_options,
    })
}

fn before_payload_callback(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    cx: &Context,
) -> Option<pi_ai::provider::OnPayloadFn> {
    before_payload_callback_owned(
        lane.owner.hooks.clone(),
        lane.name.clone(),
        operation.meta.operation_id.to_string(),
        controller.gate.clone(),
        cx.clone(),
    )
}

fn before_payload_callback_owned(
    hooks: crate::harness::hooks::HookRegistry,
    lane_name: LaneName,
    run_id: String,
    gate: Gate,
    hook_context: Context,
) -> Option<pi_ai::provider::OnPayloadFn> {
    if !hooks.has::<crate::harness::hooks::BeforePayload>() {
        return None;
    }
    Some(Arc::new(move |payload, model| {
        let hooks = hooks.clone();
        let lane_name = lane_name.clone();
        let run_id = run_id.clone();
        let gate = gate.clone();
        let hook_context = hook_context.clone();
        let value = payload.clone();
        Box::pin(async move {
            let event = BeforePayloadEvent {
                lane: lane_name,
                run_id,
                model: model.clone(),
                payload: value,
            };
            match hooks
                .run_with_gate::<crate::harness::hooks::BeforePayload>(event, &gate, &hook_context)
                .await
            {
                Ok(Some(result)) => {
                    *payload = result.payload;
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(HookRunError::Gate(GateRejection::Aborted(abort))) => {
                    abort.wait().await;
                    Err(pi_ai::ProviderError::new("assistant request aborted"))
                }
                Err(HookRunError::Gate(GateRejection::Closed(fault))) => {
                    Err(pi_ai::ProviderError::new(fault.message.clone()))
                }
                Err(HookRunError::Handler(error)) => Err(pi_ai::ProviderError::new(error.message)),
            }
        })
    }))
}

fn after_response_callback(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
) -> Option<HarnessAfterResponse> {
    if !lane
        .owner
        .hooks
        .has::<crate::harness::hooks::AfterResponse>()
    {
        return None;
    }
    let hooks = lane.owner.hooks.clone();
    let lane_name = lane.name.clone();
    let run_id = operation.meta.operation_id.to_string();
    let gate = controller.gate.clone();
    Some(Arc::new(move |message, metadata, cx| {
        let hooks = hooks.clone();
        let lane_name = lane_name.clone();
        let run_id = run_id.clone();
        let gate = gate.clone();
        Box::pin(async move {
            let event = AfterResponseEvent {
                lane: lane_name,
                run_id,
                status: metadata.status,
                headers: metadata.headers,
                message: message.clone(),
            };
            let result = hooks
                .run_with_gate::<crate::harness::hooks::AfterResponse>(event, &gate, &cx)
                .await;
            match result {
                Ok(Some(result)) => Ok(result.message.unwrap_or(message)),
                Ok(None) => Ok(message),
                Err(HookRunError::Gate(rejection)) => Err(rejection),
                Err(HookRunError::Handler(error)) => Err(GateRejection::Closed(Arc::new(error))),
            }
        })
    }))
}

fn transform_context(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
) -> Option<TransformRequestContext> {
    if !lane
        .owner
        .hooks
        .has::<crate::harness::hooks::TransformContext>()
    {
        return None;
    }
    let hooks = lane.owner.hooks.clone();
    let lane_name = lane.name.clone();
    let run_id = operation.meta.operation_id.to_string();
    let gate = controller.gate.clone();
    Some(Arc::new(
        move |request: HarnessRequestContext, cx: Context| {
            let hooks = hooks.clone();
            let lane_name = lane_name.clone();
            let run_id = run_id.clone();
            let gate = gate.clone();
            async move {
                let event = crate::harness::hooks::TransformContextEvent {
                    lane: lane_name,
                    run_id,
                    messages: request.messages.clone(),
                    system_prompt: request.system_prompt.clone(),
                };
                let result = hooks
                    .run_with_gate::<crate::harness::hooks::TransformContext>(event, &gate, &cx)
                    .await
                    .map_err(|error| match error {
                        crate::harness::hooks::HookRunError::Gate(rejection) => rejection,
                        crate::harness::hooks::HookRunError::Handler(fault) => {
                            GateRejection::Closed(Arc::new(fault))
                        }
                    })?;
                let Some(result) = result else {
                    return Ok(request);
                };
                Ok(HarnessRequestContext {
                    messages: result.messages.unwrap_or(request.messages),
                    system_prompt: result.system_prompt.unwrap_or(request.system_prompt),
                })
            }
            .boxed()
        },
    ))
}

fn retry_delay(base: u64, attempt: u64) -> Result<u64, HarnessError> {
    let shift = attempt.saturating_sub(1).min(63);
    base.checked_shl(u32::try_from(shift).map_err(|_| HarnessError::Closed {
        message: "retry shift overflow".to_owned(),
    })?)
    .ok_or_else(|| HarnessError::Closed {
        message: "retry delay overflow".to_owned(),
    })
}

async fn deferred_stream_failure(
    lane: &LaneRuntime,
    operation: &Operation,
    controller: &DriveController,
    error: crate::harness::stream::HarnessStreamError,
    cx: &Context,
) -> Result<DriveStep, HarnessError> {
    match error {
        crate::harness::stream::HarnessStreamError::Gate(GateRejection::Aborted(abort)) => {
            abort.wait().await;
            reconcile_abort(lane, operation, controller, cx).await
        }
        crate::harness::stream::HarnessStreamError::Gate(GateRejection::Closed(fault)) => {
            Err(HarnessError::Closed {
                message: fault.message.clone(),
            })
        }
        crate::harness::stream::HarnessStreamError::Fault(fault) => {
            lane.owner.fault(fault, cx).await;
            Err(lane.owner.closed_error())
        }
        crate::harness::stream::HarnessStreamError::Preparation(error) => Err(error),
        crate::harness::stream::HarnessStreamError::Provider(error) => {
            let message = error.to_string();
            lane.owner
                .fault(
                    HarnessFault {
                        message: format!("deferred provider poll failed: {message}"),
                        cause: Box::new(error),
                    },
                    cx,
                )
                .await;
            Err(lane.owner.closed_error())
        }
    }
}

fn hook_error(error: crate::harness::hooks::HookRunError) -> HarnessError {
    match error {
        crate::harness::hooks::HookRunError::Gate(_) => HarnessError::Closed {
            message: "hook was aborted".to_owned(),
        },
        crate::harness::hooks::HookRunError::Handler(error) => HarnessError::Closed {
            message: error.message,
        },
    }
}

fn invariant(message: &str) -> HarnessError {
    HarnessError::Closed {
        message: format!("harness invariant failed: {message}"),
    }
}

struct ResponseObserver {
    owner: Arc<super::harness::HarnessRuntime>,
    lane: LaneName,
    operation_id: OperationId,
    response_id: EntryId,
    encoder: Mutex<pi_ai::AssistantMessageFrameEncoder>,
}

impl ResponseObserver {
    fn new(lane: &LaneRuntime, operation_id: OperationId, response_id: EntryId) -> Self {
        Self {
            owner: Arc::clone(&lane.owner),
            lane: lane.name.clone(),
            operation_id,
            response_id,
            encoder: Mutex::new(pi_ai::AssistantMessageFrameEncoder::new()),
        }
    }

    fn encode(
        &self,
        event: &pi_ai::AssistantMessageEvent,
    ) -> Result<Option<pi_ai::AssistantMessageFrame>, HarnessFault> {
        let mut encoder = self.encoder.lock().map_err(|_| HarnessFault {
            message: "assistant frame encoder lock poisoned".to_owned(),
            cause: Box::new(EncoderPoisoned),
        })?;
        encoder.encode(event).map_err(|error| HarnessFault {
            message: error.to_string(),
            cause: Box::new(FrameEncodingError(error.to_string())),
        })
    }

    async fn commit(&self, writes: Vec<Write>, cx: &Context) -> Result<(), HarnessFault> {
        let mutator = self
            .owner
            .session
            .begin_mutation(cx)
            .await
            .map_err(|error| HarnessFault {
                message: error.to_string(),
                cause: Box::new(RuntimeObserverError(error.to_string())),
            })?;
        mutator
            .commit(writes, cx)
            .await
            .map_err(|error| HarnessFault {
                message: error.to_string(),
                cause: Box::new(RuntimeObserverError(error.to_string())),
            })?;
        Ok(())
    }

    async fn emit(&self, payload: HarnessEventPayload, cx: &Context) {
        self.owner
            .emit(
                crate::harness::event::HarnessEvent::lane(self.lane.clone(), payload),
                cx,
            )
            .await;
    }
}

impl AssistantStreamObserver for ResponseObserver {
    fn start<'a>(
        &'a self,
        message: &'a pi_ai::AssistantMessage,
        event: &'a pi_ai::AssistantMessageEvent,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>> {
        Box::pin(async move {
            if let Some(frame) = self.encode(event)? {
                let write = crate::session::append_list(
                    &pending_assistant_frames(&self.operation_id, &self.response_id),
                    &frame,
                )
                .map_err(|error| HarnessFault {
                    message: error.to_string(),
                    cause: Box::new(FrameEncodingError(error.to_string())),
                })?;
                self.commit(vec![write], cx).await?;
            }
            let message =
                assistant_agent_message(message.clone()).map_err(|error| HarnessFault {
                    message: error.to_string(),
                    cause: Box::new(RuntimeObserverError(error.to_string())),
                })?;
            self.emit(
                HarnessEventPayload::MessageStart {
                    run_id: Some(self.operation_id.clone()),
                    message,
                },
                cx,
            )
            .await;
            Ok(())
        })
    }

    fn update<'a>(
        &'a self,
        message: &'a pi_ai::AssistantMessage,
        event: &'a pi_ai::AssistantMessageEvent,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>> {
        Box::pin(async move {
            let frame = self.encode(event)?;
            if let Some(frame) = frame.as_ref() {
                let write = crate::session::append_list(
                    &pending_assistant_frames(&self.operation_id, &self.response_id),
                    frame,
                )
                .map_err(|error| HarnessFault {
                    message: error.to_string(),
                    cause: Box::new(FrameEncodingError(error.to_string())),
                })?;
                self.commit(vec![write], cx).await?;
            }
            let message =
                assistant_agent_message(message.clone()).map_err(|error| HarnessFault {
                    message: error.to_string(),
                    cause: Box::new(RuntimeObserverError(error.to_string())),
                })?;
            self.emit(
                HarnessEventPayload::MessageUpdate {
                    run_id: self.operation_id.clone(),
                    message,
                    event: Box::new(event.clone()),
                    frame,
                },
                cx,
            )
            .await;
            Ok(())
        })
    }

    fn end<'a>(
        &'a self,
        message: &'a SettledAssistantMessage,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>> {
        Box::pin(async move {
            let message =
                assistant_agent_message(message.get().clone()).map_err(|error| HarnessFault {
                    message: error.to_string(),
                    cause: Box::new(RuntimeObserverError(error.to_string())),
                })?;
            self.emit(
                HarnessEventPayload::MessageEnd {
                    run_id: Some(self.operation_id.clone()),
                    message,
                    entry_id: None,
                },
                cx,
            )
            .await;
            Ok(())
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("assistant frame encoder lock poisoned")]
struct EncoderPoisoned;
#[derive(Debug, thiserror::Error)]
#[error("frame encoding failed: {0}")]
struct FrameEncodingError(String);
#[derive(Debug, thiserror::Error)]
#[error("runtime observer failed: {0}")]
struct RuntimeObserverError(String);

struct Invocation<'a> {
    lane: &'a LaneRuntime,
    operation_id: OperationId,
    turn_id: String,
    entry_id: EntryId,
}
impl ToolInvocation for Invocation<'_> {
    fn invocation_id(&self) -> &EntryId {
        &self.entry_id
    }
    fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }
    fn turn_id(&self) -> &str {
        &self.turn_id
    }
    fn get_memo<'b>(
        &'b self,
        name: &'b str,
        cx: &'b Context,
    ) -> BoxFuture<'b, Result<Option<Value>, crate::session::SessionError>> {
        Box::pin(async move {
            self.lane
                .owner
                .session
                .get_value(
                    &operation_tool_memo(&self.operation_id, &self.entry_id, name),
                    cx,
                )
                .await
                .map(|value| value.map(|stored| stored.value))
        })
    }
    fn set_memo<'b>(
        &'b self,
        name: &'b str,
        value: Option<Value>,
        cx: &'b Context,
    ) -> BoxFuture<'b, Result<(), crate::session::SessionError>> {
        Box::pin(async move {
            let address = operation_tool_memo(&self.operation_id, &self.entry_id, name);
            match value {
                Some(value) => {
                    self.lane
                        .owner
                        .session
                        .set_value_json(&address.erase(), value, cx)
                        .await
                }
                None => {
                    self.lane
                        .owner
                        .session
                        .delete_value_json(&address.erase(), cx)
                        .await
                }
            }
        })
    }
}
