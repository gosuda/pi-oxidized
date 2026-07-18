//! Tool-call scheduling for one assistant message.
//!
//! Ports `executeToolCalls` and helpers from the TypeScript agent loop:
//! sequential force for any sequential tool, source-order preflights, parallel
//! completion-order ends, and source-order tool-result messages.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use pi_ai::{AssistantContent, AssistantMessage, Message, ToolCall, ToolResultMessage};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::{
    AfterToolCall, AfterToolCallContext, AgentContext, AgentLoopConfig, BeforeToolCallContext,
};
use crate::error::AgentLoopError;
use crate::event::AgentEvent;
use crate::message::{AgentMessage, now_millis};
use crate::tool::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdates, error_tool_result};

/// Maximum number of tool calls executing concurrently in a parallel batch.
pub const MAX_PARALLEL_TOOL_CALLS: usize = 8;

/// Maximum number of queued parallel tool progress updates.
pub const PARALLEL_TOOL_UPDATE_CAPACITY: usize = 64;

/// Outcome of scheduling every tool call from one assistant message.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutedToolCallBatch {
    /// Tool-result messages in assistant source order.
    pub messages: Vec<ToolResultMessage>,
    /// When true, the outer loop must not request another tool turn.
    pub terminate: bool,
}

struct PreparedToolCall {
    tool_call: ToolCall,
    tool: Arc<dyn AgentTool>,
    args: Map<String, Value>,
}

struct ImmediateOutcome {
    result: AgentToolResult,
    is_error: bool,
}

struct ExecutedOutcome {
    result: AgentToolResult,
    is_error: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct FinalizedOutcome {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

enum Preparation {
    Prepared(PreparedToolCall),
    Immediate(ImmediateOutcome),
}

struct ParallelUpdate {
    index: usize,
    partial: AgentToolResult,
}

struct ParallelWorkerResult {
    index: usize,
    finalized: Result<FinalizedOutcome, ()>,
}

/// Synchronous, non-blocking event fan-out used by the scheduler.
pub trait EmitAgentEvent {
    /// Emits one agent event.
    fn emit(&self, event: AgentEvent);
}

impl<F> EmitAgentEvent for F
where
    F: Fn(AgentEvent),
{
    fn emit(&self, event: AgentEvent) {
        self(event);
    }
}

/// Executes every tool call on `assistant_message` under `config` scheduling rules.
///
/// Any tool with [`ToolExecutionMode::Sequential`], or a global sequential mode,
/// forces the whole batch to run sequentially. Parallel mode preflights in source
/// order, executes concurrently, emits ends in completion order, and emits
/// tool-result messages in source order.
///
/// # Errors
///
/// Returns [`AgentLoopError`] only for unrecoverable loop infrastructure failures.
/// Prepare/validate/execute and hook-block failures are converted into error tool
/// results and returned inside [`ExecutedToolCallBatch`].
pub async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    emit: &impl EmitAgentEvent,
) -> Result<ExecutedToolCallBatch, AgentLoopError> {
    let tool_calls = tool_calls_from_message(assistant_message);
    let has_sequential = tool_calls.iter().any(|tool_call| {
        current_context
            .tools
            .iter()
            .find(|tool| tool.name() == tool_call.name)
            .and_then(|tool| tool.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            emit,
        )
        .await
    }
}

/// Fails every tool call without executing because the assistant hit `length`.
#[must_use]
pub fn fail_tool_calls_from_truncated_message(
    assistant_message: &AssistantMessage,
    emit: &impl EmitAgentEvent,
) -> ExecutedToolCallBatch {
    let tool_calls = tool_calls_from_message(assistant_message);
    let mut messages = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_tool_execution_start(&tool_call, &tool_call.arguments, emit);
        let finalized = FinalizedOutcome {
            tool_call: tool_call.clone(),
            result: error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, emit);
        let message = tool_result_message(&finalized);
        emit_tool_result_message(&message, emit);
        messages.push(message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// Returns true when a non-empty batch asks the loop to stop after tools.
#[must_use]
pub fn should_terminate_tool_batch(results: &[AgentToolResult]) -> bool {
    !results.is_empty() && results.iter().all(|result| result.terminate == Some(true))
}

fn should_terminate_finalized(finalized: &[FinalizedOutcome]) -> bool {
    !finalized.is_empty()
        && finalized
            .iter()
            .all(|entry| entry.result.terminate == Some(true))
}

fn tool_calls_from_message(assistant_message: &AssistantMessage) -> Vec<ToolCall> {
    assistant_message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
            AssistantContent::Text(_) | AssistantContent::Thinking(_) => None,
        })
        .collect()
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    emit: &impl EmitAgentEvent,
) -> Result<ExecutedToolCallBatch, AgentLoopError> {
    let mut finalized_calls = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, &tool_call.arguments, emit);

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await;
        let finalized = match preparation {
            Preparation::Immediate(immediate) => FinalizedOutcome {
                tool_call: tool_call.clone(),
                result: immediate.result,
                is_error: immediate.is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call(&prepared, cancel, emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    prepared,
                    executed,
                    config.after_tool_call.clone(),
                    cancel.clone(),
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit);
        let message = tool_result_message(&finalized);
        emit_tool_result_message(&message, emit);
        finalized_calls.push(finalized);
        messages.push(message);
    }

    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_finalized(&finalized_calls),
    })
}

async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    emit: &impl EmitAgentEvent,
) -> Result<ExecutedToolCallBatch, AgentLoopError> {
    let mut slots = Vec::with_capacity(tool_calls.len());
    let mut jobs = Vec::with_capacity(tool_calls.len());
    preflight_parallel_tools(ParallelPreflight {
        current_context,
        assistant_message,
        tool_calls,
        config,
        cancel,
        emit,
        slots: &mut slots,
        jobs: &mut jobs,
    })
    .await;

    let context = Arc::new(current_context.clone());
    let assistant = Arc::new(assistant_message.clone());
    let (update_tx, mut update_rx) = mpsc::channel(PARALLEL_TOOL_UPDATE_CAPACITY);
    let mut workers = JoinSet::new();
    let mut jobs = jobs.into_iter();
    while workers.len() < MAX_PARALLEL_TOOL_CALLS
        && spawn_next_parallel_worker(
            &mut workers,
            &mut jobs,
            &context,
            &assistant,
            config.after_tool_call.as_ref(),
            cancel,
            &update_tx,
        )
    {}

    collect_parallel_completions(
        &mut workers,
        &mut jobs,
        &context,
        &assistant,
        config.after_tool_call.as_ref(),
        cancel,
        &update_tx,
        &mut update_rx,
        &mut slots,
        emit,
    )
    .await;
    Ok(emit_source_ordered_results(slots, emit))
}

enum ParallelSlot {
    Ready(FinalizedOutcome),
    Pending(ToolCall),
}

struct ParallelPreflight<'a, E: EmitAgentEvent> {
    current_context: &'a AgentContext,
    assistant_message: &'a AssistantMessage,
    tool_calls: &'a [ToolCall],
    config: &'a AgentLoopConfig,
    cancel: &'a CancellationToken,
    emit: &'a E,
    slots: &'a mut Vec<ParallelSlot>,
    jobs: &'a mut Vec<(usize, PreparedToolCall)>,
}

async fn preflight_parallel_tools<E: EmitAgentEvent>(preflight: ParallelPreflight<'_, E>) {
    let ParallelPreflight {
        current_context,
        assistant_message,
        tool_calls,
        config,
        cancel,
        emit,
        slots,
        jobs,
    } = preflight;

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, &tool_call.arguments, emit);

        match prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await
        {
            Preparation::Immediate(immediate) => {
                let finalized = FinalizedOutcome {
                    tool_call: tool_call.clone(),
                    result: immediate.result,
                    is_error: immediate.is_error,
                };
                emit_tool_execution_end(&finalized, emit);
                slots.push(ParallelSlot::Ready(finalized));
            }
            Preparation::Prepared(prepared) => {
                let index = slots.len();
                slots.push(ParallelSlot::Pending(prepared.tool_call.clone()));
                jobs.push((index, prepared));
            }
        }
    }
}

fn spawn_next_parallel_worker(
    workers: &mut JoinSet<ParallelWorkerResult>,
    jobs: &mut std::vec::IntoIter<(usize, PreparedToolCall)>,
    context: &Arc<AgentContext>,
    assistant: &Arc<AssistantMessage>,
    after_tool_call: Option<&AfterToolCall>,
    cancel: &CancellationToken,
    update_tx: &mpsc::Sender<ParallelUpdate>,
) -> bool {
    let Some((index, prepared)) = jobs.next() else {
        return false;
    };
    spawn_parallel_tool(
        workers,
        index,
        prepared,
        Arc::clone(context),
        Arc::clone(assistant),
        after_tool_call.cloned(),
        cancel.clone(),
        update_tx.clone(),
    );
    true
}

fn spawn_parallel_tool(
    workers: &mut JoinSet<ParallelWorkerResult>,
    index: usize,
    prepared: PreparedToolCall,
    context: Arc<AgentContext>,
    assistant: Arc<AssistantMessage>,
    after_tool_call: Option<AfterToolCall>,
    cancel: CancellationToken,
    update_tx: mpsc::Sender<ParallelUpdate>,
) {
    workers.spawn(async move {
        let worker = async move {
            let updates = ToolUpdates::new(move |partial| {
                let _ = update_tx.try_send(ParallelUpdate { index, partial });
            });

            let executed = match prepared
                .tool
                .execute(
                    &prepared.tool_call.id,
                    prepared.args.clone(),
                    cancel.clone(),
                    updates.clone(),
                )
                .await
            {
                Ok(result) => {
                    updates.stop_accepting();
                    ExecutedOutcome {
                        result,
                        is_error: false,
                    }
                }
                Err(error) => {
                    updates.stop_accepting();
                    ExecutedOutcome {
                        result: AgentToolResult::from(error),
                        is_error: true,
                    }
                }
            };

            finalize_executed_tool_call(
                context.as_ref(),
                assistant.as_ref(),
                prepared,
                executed,
                after_tool_call,
                cancel,
            )
            .await
        };
        ParallelWorkerResult {
            index,
            finalized: AssertUnwindSafe(worker)
                .catch_unwind()
                .await
                .map_err(|_| ()),
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn collect_parallel_completions(
    workers: &mut JoinSet<ParallelWorkerResult>,
    jobs: &mut std::vec::IntoIter<(usize, PreparedToolCall)>,
    context: &Arc<AgentContext>,
    assistant: &Arc<AssistantMessage>,
    after_tool_call: Option<&AfterToolCall>,
    cancel: &CancellationToken,
    update_tx: &mpsc::Sender<ParallelUpdate>,
    update_rx: &mut mpsc::Receiver<ParallelUpdate>,
    slots: &mut [ParallelSlot],
    emit: &impl EmitAgentEvent,
) {
    while !workers.is_empty() {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                workers.abort_all();
                while workers.join_next().await.is_some() {}
                settle_all_pending(slots, "Operation aborted", emit);
                return;
            }
            update = update_rx.recv() => {
                if let Some(update) = update {
                    emit_parallel_update(slots, update, emit);
                }
            }
            joined = workers.join_next() => {
                drain_parallel_updates(update_rx, slots, emit);
                match joined {
                    Some(Ok(worker)) => settle_worker(slots, worker, emit),
                    Some(Err(_)) | None => {}
                }
                let _ = spawn_next_parallel_worker(
                    workers,
                    jobs,
                    context,
                    assistant,
                    after_tool_call,
                    cancel,
                    update_tx,
                );
            }
        }
    }

    drain_parallel_updates(update_rx, slots, emit);
    settle_all_pending(slots, "Tool execution ended without a result", emit);
}

fn drain_parallel_updates(
    update_rx: &mut mpsc::Receiver<ParallelUpdate>,
    slots: &[ParallelSlot],
    emit: &impl EmitAgentEvent,
) {
    while let Ok(update) = update_rx.try_recv() {
        emit_parallel_update(slots, update, emit);
    }
}

fn emit_parallel_update(
    slots: &[ParallelSlot],
    update: ParallelUpdate,
    emit: &impl EmitAgentEvent,
) {
    let Some(tool_call) = slots.get(update.index).map(|slot| match slot {
        ParallelSlot::Ready(finalized) => &finalized.tool_call,
        ParallelSlot::Pending(tool_call) => tool_call,
    }) else {
        return;
    };
    emit_tool_execution_update(tool_call, update.partial, emit);
}

fn settle_worker(
    slots: &mut [ParallelSlot],
    worker: ParallelWorkerResult,
    emit: &impl EmitAgentEvent,
) {
    match worker.finalized {
        Ok(finalized) => {
            emit_tool_execution_end(&finalized, emit);
            if let Some(slot) = slots.get_mut(worker.index) {
                *slot = ParallelSlot::Ready(finalized);
            }
        }
        Err(()) => settle_pending(slots, worker.index, "Tool execution panicked", emit),
    }
}

fn settle_pending(
    slots: &mut [ParallelSlot],
    index: usize,
    message: &str,
    emit: &impl EmitAgentEvent,
) {
    let Some(slot) = slots.get_mut(index) else {
        return;
    };
    let ParallelSlot::Pending(tool_call) = slot else {
        return;
    };
    let finalized = FinalizedOutcome {
        tool_call: tool_call.clone(),
        result: error_tool_result(message),
        is_error: true,
    };
    emit_tool_execution_end(&finalized, emit);
    *slot = ParallelSlot::Ready(finalized);
}

fn settle_all_pending(slots: &mut [ParallelSlot], message: &str, emit: &impl EmitAgentEvent) {
    for index in 0..slots.len() {
        settle_pending(slots, index, message, emit);
    }
}

fn emit_source_ordered_results(
    slots: Vec<ParallelSlot>,
    emit: &impl EmitAgentEvent,
) -> ExecutedToolCallBatch {
    let mut finalized_calls = Vec::with_capacity(slots.len());
    let mut messages = Vec::with_capacity(slots.len());
    for slot in slots {
        let finalized = match slot {
            ParallelSlot::Ready(finalized) => finalized,
            ParallelSlot::Pending(tool_call) => {
                let finalized = FinalizedOutcome {
                    tool_call,
                    result: error_tool_result("Tool execution ended without a result"),
                    is_error: true,
                };
                emit_tool_execution_end(&finalized, emit);
                finalized
            }
        };
        let message = tool_result_message(&finalized);
        emit_tool_result_message(&message, emit);
        messages.push(message);
        finalized_calls.push(finalized);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_finalized(&finalized_calls),
    }
}

async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> Preparation {
    let Some(tool) = current_context
        .tools
        .iter()
        .find(|tool| tool.name() == tool_call.name)
        .cloned()
    else {
        return Preparation::Immediate(ImmediateOutcome {
            result: error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        });
    };

    let validated_args = match tool
        .prepare_and_validate_arguments(tool_call.arguments.clone())
        .await
    {
        Ok(args) => args,
        Err(error) => {
            return Preparation::Immediate(ImmediateOutcome {
                result: AgentToolResult::from(error),
                is_error: true,
            });
        }
    };

    if let Some(before_tool_call) = config.before_tool_call.as_ref() {
        match before_tool_call(
            BeforeToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: validated_args.clone(),
                context: current_context.clone(),
            },
            cancel.clone(),
        )
        .await
        {
            Ok(Some(result)) if result.block => {
                return Preparation::Immediate(ImmediateOutcome {
                    result: error_tool_result(
                        result
                            .reason
                            .unwrap_or_else(|| "Tool execution was blocked".to_owned()),
                    ),
                    is_error: true,
                });
            }
            Ok(_) => {}
            Err(error) => {
                return Preparation::Immediate(ImmediateOutcome {
                    result: error_tool_result(error.to_string()),
                    is_error: true,
                });
            }
        }

        if cancel.is_cancelled() {
            return Preparation::Immediate(ImmediateOutcome {
                result: error_tool_result("Operation aborted"),
                is_error: true,
            });
        }
    }

    if cancel.is_cancelled() {
        return Preparation::Immediate(ImmediateOutcome {
            result: error_tool_result("Operation aborted"),
            is_error: true,
        });
    }

    Preparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool,
        args: validated_args,
    })
}

async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    cancel: &CancellationToken,
    emit: &impl EmitAgentEvent,
) -> ExecutedOutcome {
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<AgentToolResult>();
    let tool_call = prepared.tool_call.clone();
    let tool = Arc::clone(&prepared.tool);
    let args = prepared.args.clone();
    let tool_call_id = prepared.tool_call.id.clone();
    let cancel = cancel.clone();

    let mut execute = Box::pin(async move {
        let updates = ToolUpdates::new(move |partial| {
            let _ = update_tx.send(partial);
        });
        match tool
            .execute(&tool_call_id, args, cancel, updates.clone())
            .await
        {
            Ok(result) => {
                updates.stop_accepting();
                ExecutedOutcome {
                    result,
                    is_error: false,
                }
            }
            Err(error) => {
                updates.stop_accepting();
                ExecutedOutcome {
                    result: AgentToolResult::from(error),
                    is_error: true,
                }
            }
        }
    });

    loop {
        tokio::select! {
            biased;
            outcome = &mut execute => {
                while let Ok(partial) = update_rx.try_recv() {
                    emit_tool_execution_update(&tool_call, partial, emit);
                }
                return outcome;
            }
            partial = update_rx.recv() => {
                if let Some(partial) = partial {
                    emit_tool_execution_update(&tool_call, partial, emit);
                } else {
                    let outcome = execute.await;
                    while let Ok(partial) = update_rx.try_recv() {
                        emit_tool_execution_update(&tool_call, partial, emit);
                    }
                    return outcome;
                }
            }
        }
    }
}

async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: PreparedToolCall,
    executed: ExecutedOutcome,
    after_tool_call: Option<AfterToolCall>,
    cancel: CancellationToken,
) -> FinalizedOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after_tool_call) = after_tool_call {
        match after_tool_call(
            AfterToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: prepared.tool_call.clone(),
                args: prepared.args.clone(),
                result: result.clone(),
                is_error,
                context: current_context.clone(),
            },
            cancel,
        )
        .await
        {
            Ok(Some(after)) => {
                if let Some(content) = after.content {
                    result.content = content;
                }
                if let Some(details) = after.details {
                    result.details = details;
                }
                if let Some(terminate) = after.terminate {
                    result.terminate = Some(terminate);
                }
                if let Some(flag) = after.is_error {
                    is_error = flag;
                }
            }
            Ok(None) => {}
            Err(error) => {
                result = error_tool_result(error.to_string());
                is_error = true;
            }
        }
    }

    FinalizedOutcome {
        tool_call: prepared.tool_call,
        result,
        is_error,
    }
}

fn emit_tool_execution_start(
    tool_call: &ToolCall,
    args: &Map<String, Value>,
    emit: &impl EmitAgentEvent,
) {
    emit.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        args: args.clone(),
    });
}

fn emit_tool_execution_update(
    tool_call: &ToolCall,
    partial: AgentToolResult,
    emit: &impl EmitAgentEvent,
) {
    emit.emit(AgentEvent::ToolExecutionUpdate {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        args: tool_call.arguments.clone(),
        partial_result: partial,
    });
}

fn emit_tool_execution_end(finalized: &FinalizedOutcome, emit: &impl EmitAgentEvent) {
    emit.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    });
}

fn emit_tool_result_message(message: &ToolResultMessage, emit: &impl EmitAgentEvent) {
    let agent_message = AgentMessage::Llm(Box::new(Message::ToolResult(message.clone())));
    emit.emit(AgentEvent::MessageStart {
        message: agent_message.clone(),
    });
    emit.emit(AgentEvent::MessageEnd {
        message: agent_message,
    });
}

fn tool_result_message(finalized: &FinalizedOutcome) -> ToolResultMessage {
    let mut message = ToolResultMessage::new(
        finalized.tool_call.id.clone(),
        finalized.tool_call.name.clone(),
        finalized.result.content.clone(),
        finalized.is_error,
        now_millis(),
    );
    message.details = Some(finalized.result.details.clone());
    if let Some(names) = finalized.result.added_tool_names.as_ref()
        && !names.is_empty()
    {
        message.added_tool_names = Some(names.clone());
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use pi_ai::{ImageContent, Model, ModelCost, ModelInput, TextContent, ToolResultContent};
    use serde_json::json;
    use tokio::time::{sleep, timeout};

    use crate::config::{AfterToolCallResult, BeforeToolCallResult, default_convert_to_llm_hook};
    use crate::error::ToolError;

    type TestResult = Result<(), String>;

    fn sample_model() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "openai-completions".to_owned(),
            provider: "openai".to_owned(),
            base_url: "https://example.test".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn sample_config(mode: ToolExecutionMode) -> AgentLoopConfig {
        AgentLoopConfig {
            model: sample_model(),
            reasoning: None,
            temperature: None,
            max_tokens: None,
            session_id: None,
            transport: None,
            cache_retention: None,
            thinking_budgets: None,
            max_retry_delay_ms: None,
            metadata: None,
            headers: None,
            env: None,
            stream_extra: Map::new(),
            tool_execution: mode,
            convert_to_llm: default_convert_to_llm_hook(),
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            on_payload: None,
            on_response: None,
        }
    }

    fn assistant_with_calls(calls: Vec<ToolCall>) -> AssistantMessage {
        let mut message = AssistantMessage::new("openai-completions", "openai", "m", 1);
        message.content = calls.into_iter().map(AssistantContent::ToolCall).collect();
        message.stop_reason = pi_ai::StopReason::ToolUse;
        message
    }

    fn collect_emit() -> (Arc<Mutex<Vec<AgentEvent>>>, impl EmitAgentEvent) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_cb = Arc::clone(&events);
        let emit = move |event: AgentEvent| {
            if let Ok(mut guard) = events_cb.lock() {
                guard.push(event);
            }
        };
        (events, emit)
    }

    fn snapshot_events(events: &Arc<Mutex<Vec<AgentEvent>>>) -> Result<Vec<AgentEvent>, String> {
        events
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| "event mutex poisoned".to_owned())
    }

    fn context_with(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext {
        AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools,
        }
    }

    fn shared_tool(
        name: &str,
        mode: Option<ToolExecutionMode>,
        delay: Duration,
        active: &Arc<AtomicUsize>,
        max_active: &Arc<AtomicUsize>,
    ) -> RecordingTool {
        RecordingTool {
            name: name.to_owned(),
            mode,
            delay,
            active: Arc::clone(active),
            max_active: Arc::clone(max_active),
            executed: Arc::new(AtomicBool::new(false)),
            fail_validate: false,
            send_late_update: false,
            panic_on_execute: false,
            ignore_cancel: false,
            progress_updates: 0,
            cancel_seen: Arc::new(AtomicBool::new(false)),
            result_text: format!("{name}-ok"),
            image: false,
            terminate: None,
            parameters: json!({"type":"object"}),
        }
    }

    struct RecordingTool {
        name: String,
        mode: Option<ToolExecutionMode>,
        delay: Duration,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        executed: Arc<AtomicBool>,
        fail_validate: bool,
        send_late_update: bool,
        panic_on_execute: bool,
        ignore_cancel: bool,
        progress_updates: usize,
        cancel_seen: Arc<AtomicBool>,
        result_text: String,
        image: bool,
        terminate: Option<bool>,
        parameters: Value,
    }

    impl RecordingTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                mode: None,
                delay: Duration::from_millis(0),
                active: Arc::new(AtomicUsize::new(0)),
                max_active: Arc::new(AtomicUsize::new(0)),
                executed: Arc::new(AtomicBool::new(false)),
                fail_validate: false,
                send_late_update: false,
                panic_on_execute: false,
                ignore_cancel: false,
                progress_updates: 0,
                cancel_seen: Arc::new(AtomicBool::new(false)),
                result_text: format!("{name}-ok"),
                image: false,
                terminate: None,
                parameters: json!({"type":"object","properties":{}}),
            }
        }
    }

    impl AgentTool for RecordingTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn label(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &'static str {
            "recording"
        }

        fn parameters(&self) -> &Value {
            &self.parameters
        }

        fn execution_mode(&self) -> Option<ToolExecutionMode> {
            self.mode
        }

        fn validate_arguments(
            &self,
            args: &Map<String, Value>,
        ) -> Result<Map<String, Value>, ToolError> {
            if self.fail_validate {
                return Err(ToolError::new("bad args"));
            }
            Ok(args.clone())
        }

        fn execute(
            &self,
            _tool_call_id: &str,
            _args: Map<String, Value>,
            cancel: CancellationToken,
            updates: ToolUpdates,
        ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
            let delay = self.delay;
            let active = Arc::clone(&self.active);
            let max_active = Arc::clone(&self.max_active);
            let executed = Arc::clone(&self.executed);
            let cancel_seen = Arc::clone(&self.cancel_seen);
            let result_text = self.result_text.clone();
            let image = self.image;
            let terminate = self.terminate;
            let send_late_update = self.send_late_update;
            let panic_on_execute = self.panic_on_execute;
            let ignore_cancel = self.ignore_cancel;
            let progress_updates = self.progress_updates;

            Box::pin(async move {
                executed.store(true, Ordering::SeqCst);
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(current, Ordering::SeqCst);

                assert!(!panic_on_execute, "intentional tool panic");

                updates.send(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent::new("partial"))],
                    details: json!({"stage":"partial"}),
                    added_tool_names: None,
                    terminate: None,
                });
                for index in 0..progress_updates {
                    updates.send(AgentToolResult {
                        content: vec![ToolResultContent::Text(TextContent::new(format!(
                            "progress-{index}"
                        )))],
                        details: json!({"stage":"progress"}),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                if ignore_cancel {
                    std::future::pending::<()>().await;
                }

                if delay.is_zero() {
                    if cancel.is_cancelled() {
                        cancel_seen.store(true, Ordering::SeqCst);
                    }
                } else {
                    tokio::select! {
                        () = sleep(delay) => {}
                        () = cancel.cancelled() => {
                            cancel_seen.store(true, Ordering::SeqCst);
                        }
                    }
                }

                active.fetch_sub(1, Ordering::SeqCst);
                updates.stop_accepting();

                if send_late_update {
                    updates.send(AgentToolResult {
                        content: vec![ToolResultContent::Text(TextContent::new("late"))],
                        details: json!({"stage":"late"}),
                        added_tool_names: None,
                        terminate: None,
                    });
                }

                let mut content = vec![ToolResultContent::Text(TextContent::new(result_text))];
                if image {
                    content.push(ToolResultContent::Image(ImageContent::new(
                        "abc",
                        "image/png",
                    )));
                }

                Ok(AgentToolResult {
                    content,
                    details: json!({}),
                    added_tool_names: None,
                    terminate,
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_sequential_forces_no_overlap() -> TestResult {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let context = context_with(vec![
            Arc::new(shared_tool(
                "a",
                None,
                Duration::from_millis(50),
                &active,
                &max_active,
            )),
            Arc::new(shared_tool(
                "b",
                Some(ToolExecutionMode::Sequential),
                Duration::from_millis(50),
                &active,
                &max_active,
            )),
        ]);
        let assistant = assistant_with_calls(vec![
            ToolCall::new("c1", "a", Map::new()),
            ToolCall::new("c2", "b", Map::new()),
        ]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let (_events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        if batch.messages.len() != 2 {
            return Err(format!("expected 2 results, got {}", batch.messages.len()));
        }
        if max_active.load(Ordering::SeqCst) != 1 {
            return Err(format!(
                "expected no overlap, max_active={}",
                max_active.load(Ordering::SeqCst)
            ));
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_completion_order_differs_from_source_result_order() -> TestResult {
        let tool_one = RecordingTool {
            delay: Duration::from_millis(80),
            result_text: "one".to_owned(),
            ..RecordingTool::new("one")
        };
        let tool_two = RecordingTool {
            delay: Duration::from_millis(10),
            result_text: "two".to_owned(),
            ..RecordingTool::new("two")
        };
        let context = context_with(vec![Arc::new(tool_one), Arc::new(tool_two)]);
        let assistant = assistant_with_calls(vec![
            ToolCall::new("id-1", "one", Map::new()),
            ToolCall::new("id-2", "two", Map::new()),
        ]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let (events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        let source_ids: Vec<&str> = batch
            .messages
            .iter()
            .map(|message| message.tool_call_id.as_str())
            .collect();
        if source_ids != ["id-1", "id-2"] {
            return Err(format!("source result order wrong: {source_ids:?}"));
        }

        let end_ids: Vec<String> = snapshot_events(&events)?
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect();
        if end_ids != ["id-2".to_owned(), "id-1".to_owned()] {
            return Err(format!("completion end order wrong: {end_ids:?}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn length_fails_all_without_execute() -> TestResult {
        let executed = Arc::new(AtomicBool::new(false));
        let tool = RecordingTool {
            executed: Arc::clone(&executed),
            ..RecordingTool::new("t")
        };
        let mut assistant = assistant_with_calls(vec![
            ToolCall::new("a", "t", Map::new()),
            ToolCall::new("b", "t", Map::new()),
        ]);
        assistant.stop_reason = pi_ai::StopReason::Length;
        let (events, emit) = collect_emit();

        let batch = fail_tool_calls_from_truncated_message(&assistant, &emit);
        if executed.load(Ordering::SeqCst) {
            return Err("length path executed a tool".to_owned());
        }
        if batch.messages.len() != 2 || batch.terminate {
            return Err("length batch shape wrong".to_owned());
        }
        if !batch.messages.iter().all(|message| message.is_error) {
            return Err("length results must be errors".to_owned());
        }
        let ends = snapshot_events(&events)?
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { is_error: true, .. }))
            .count();
        if ends != 2 {
            return Err(format!("expected 2 error ends, got {ends}"));
        }
        let _ = tool;
        Ok(())
    }

    #[tokio::test]
    async fn before_hook_block_is_ordered_and_skips_execute() -> TestResult {
        let executed = Arc::new(AtomicBool::new(false));
        let tool = RecordingTool {
            executed: Arc::clone(&executed),
            ..RecordingTool::new("blocked")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("c1", "blocked", Map::new())]);
        let mut config = sample_config(ToolExecutionMode::Parallel);
        config.before_tool_call = Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async {
                Ok(Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("nope".to_owned()),
                }))
            })
        }));
        let (events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        if executed.load(Ordering::SeqCst) {
            return Err("blocked tool still executed".to_owned());
        }
        let first = batch
            .messages
            .first()
            .ok_or_else(|| "missing blocked result".to_owned())?;
        if !first.is_error {
            return Err("blocked result must be error".to_owned());
        }

        let kinds: Vec<&'static str> = snapshot_events(&events)?
            .into_iter()
            .map(|event| match event {
                AgentEvent::ToolExecutionStart { .. } => "start",
                AgentEvent::ToolExecutionEnd { .. } => "end",
                AgentEvent::MessageStart { .. } => "msg_start",
                AgentEvent::MessageEnd { .. } => "msg_end",
                _ => "other",
            })
            .collect();
        if kinds != ["start", "end", "msg_start", "msg_end"] {
            return Err(format!("preflight block order wrong: {kinds:?}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn late_update_after_execute_is_ignored() -> TestResult {
        let tool = RecordingTool {
            send_late_update: true,
            ..RecordingTool::new("late")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("c1", "late", Map::new())]);
        let config = sample_config(ToolExecutionMode::Sequential);
        let (events, emit) = collect_emit();

        execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        let updates: Vec<String> = snapshot_events(&events)?
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionUpdate { partial_result, .. } => partial_result
                    .content
                    .first()
                    .and_then(|content| match content {
                        ToolResultContent::Text(text) => Some(text.text.clone()),
                        ToolResultContent::Image(_) => None,
                    }),
                _ => None,
            })
            .collect();
        if updates != ["partial".to_owned()] {
            return Err(format!("late update not ignored: {updates:?}"));
        }
        Ok(())
    }

    async fn cancellation_preserves_all_tool_result_ids(mode: ToolExecutionMode) -> TestResult {
        let tool = RecordingTool::new("cancel-me");
        let executed = Arc::clone(&tool.executed);
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![
            ToolCall::new("c1", "cancel-me", Map::new()),
            ToolCall::new("c2", "cancel-me", Map::new()),
            ToolCall::new("c3", "cancel-me", Map::new()),
        ]);
        let mut config = sample_config(mode);
        config.before_tool_call = Some(Arc::new(|context, cancel| {
            Box::pin(async move {
                if context.tool_call.id == "c1" {
                    cancel.cancel();
                }
                Ok(None)
            })
        }));
        let (_events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        let ids: Vec<&str> = batch
            .messages
            .iter()
            .map(|message| message.tool_call_id.as_str())
            .collect();
        if ids != ["c1", "c2", "c3"] {
            return Err(format!("cancelled result ids were not preserved: {ids:?}"));
        }
        for message in &batch.messages {
            if !message.is_error {
                return Err(format!(
                    "cancelled result {} was not an error",
                    message.tool_call_id
                ));
            }
            let contains_aborted = message.content.iter().any(|content| {
                matches!(
                    content,
                    ToolResultContent::Text(text) if text.text.contains("Operation aborted")
                )
            });
            if !contains_aborted {
                return Err(format!(
                    "cancelled result {} lacked aborted content: {:?}",
                    message.tool_call_id, message.content
                ));
            }
        }
        if executed.load(Ordering::SeqCst) {
            return Err("pre-cancelled batch executed a tool".to_owned());
        }
        Ok(())
    }

    #[tokio::test]
    async fn sequential_cancellation_preserves_all_tool_result_ids() -> TestResult {
        cancellation_preserves_all_tool_result_ids(ToolExecutionMode::Sequential).await
    }

    #[tokio::test]
    async fn parallel_cancellation_preserves_all_tool_result_ids() -> TestResult {
        cancellation_preserves_all_tool_result_ids(ToolExecutionMode::Parallel).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_reaches_running_tool() -> TestResult {
        let cancel_seen = Arc::new(AtomicBool::new(false));
        let tool = RecordingTool {
            delay: Duration::from_millis(200),
            cancel_seen: Arc::clone(&cancel_seen),
            ..RecordingTool::new("cancel-me")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("c1", "cancel-me", Map::new())]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let (_events, emit) = collect_emit();

        let run = tokio::spawn(async move {
            execute_tool_calls(&context, &assistant, &config, &cancel_task, &emit).await
        });

        sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let batch = run
            .await
            .map_err(|error| format!("join failed: {error}"))?
            .map_err(|error| error.to_string())?;
        if batch.messages.len() != 1 {
            return Err(format!(
                "expected one cancelled result, got {}",
                batch.messages.len()
            ));
        }
        if !cancel_seen.load(Ordering::SeqCst) {
            return Err("tool did not observe cancellation".to_owned());
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_cooperative_parallel_tool_is_aborted_and_paired() -> TestResult {
        let tool = RecordingTool {
            ignore_cancel: true,
            ..RecordingTool::new("ignore-cancel")
        };
        let started = Arc::clone(&tool.executed);
        let context = context_with(vec![Arc::new(tool)]);
        let assistant =
            assistant_with_calls(vec![ToolCall::new("c1", "ignore-cancel", Map::new())]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let (events, emit) = collect_emit();

        let run = tokio::spawn(async move {
            execute_tool_calls(&context, &assistant, &config, &run_cancel, &emit).await
        });
        for _ in 0..100 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            sleep(Duration::from_millis(2)).await;
        }
        if !started.load(Ordering::SeqCst) {
            return Err("non-cooperative tool never started".to_owned());
        }

        cancel.cancel();
        let batch = timeout(Duration::from_secs(1), run)
            .await
            .map_err(|_| "cancelled batch did not settle".to_owned())?
            .map_err(|error| format!("join failed: {error}"))?
            .map_err(|error| error.to_string())?;
        if batch.messages.len() != 1 || !batch.messages[0].is_error {
            return Err(format!("unexpected cancelled batch: {:?}", batch.messages));
        }
        if !batch.messages[0].content.iter().any(|content| {
            matches!(
                content,
                ToolResultContent::Text(text) if text.text.contains("Operation aborted")
            )
        }) {
            return Err(format!(
                "missing aborted result content: {:?}",
                batch.messages[0].content
            ));
        }
        let end_count = snapshot_events(&events)?
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. }
                        if tool_call_id == "c1"
                )
            })
            .count();
        if end_count != 1 {
            return Err(format!("expected one paired error end, got {end_count}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn panicking_parallel_tool_emits_paired_error_result() -> TestResult {
        let tool = RecordingTool {
            panic_on_execute: true,
            ..RecordingTool::new("panic")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("panic-1", "panic", Map::new())]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let (events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        if batch.messages.len() != 1 || !batch.messages[0].is_error {
            return Err(format!("panic was not paired: {:?}", batch.messages));
        }
        if !batch.messages[0].content.iter().any(|content| {
            matches!(
                content,
                ToolResultContent::Text(text) if text.text.contains("Tool execution panicked")
            )
        }) {
            return Err(format!(
                "panic result lacked diagnostic: {:?}",
                batch.messages[0].content
            ));
        }
        let events = snapshot_events(&events)?;
        let ends = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        let result_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message }
                        if matches!(message.as_llm(), Some(Message::ToolResult(_)))
                )
            })
            .count();
        if ends != 1 || result_ends != 1 {
            return Err(format!(
                "panic lifecycle was not paired: ends={ends}, result_ends={result_ends}"
            ));
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_tool_concurrency_is_capped() -> TestResult {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let tool = shared_tool(
            "bounded",
            None,
            Duration::from_millis(20),
            &active,
            &max_active,
        );
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(
            (0..(MAX_PARALLEL_TOOL_CALLS * 3))
                .map(|index| ToolCall::new(format!("c{index}"), "bounded", Map::new()))
                .collect(),
        );
        let config = sample_config(ToolExecutionMode::Parallel);
        let (_events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        if batch.messages.len() != MAX_PARALLEL_TOOL_CALLS * 3 {
            return Err(format!("missing bounded results: {}", batch.messages.len()));
        }
        let observed = max_active.load(Ordering::SeqCst);
        if observed == 0 || observed > MAX_PARALLEL_TOOL_CALLS {
            return Err(format!("parallel concurrency exceeded cap: {observed}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn parallel_progress_queue_is_bounded() -> TestResult {
        let tool = RecordingTool {
            progress_updates: 10_000,
            ..RecordingTool::new("progress")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant =
            assistant_with_calls(vec![ToolCall::new("progress-1", "progress", Map::new())]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let (events, emit) = collect_emit();

        execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        let updates = snapshot_events(&events)?
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionUpdate { .. }))
            .count();
        if updates == 0 || updates > PARALLEL_TOOL_UPDATE_CAPACITY {
            return Err(format!("progress queue was not bounded: {updates}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn preserves_tool_identity_and_image_content() -> TestResult {
        let tool = RecordingTool {
            image: true,
            result_text: "hello".to_owned(),
            ..RecordingTool::new("img")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let args = Map::from_iter([("path".to_owned(), json!("a"))]);
        let assistant = assistant_with_calls(vec![ToolCall::new("call-9", "img", args.clone())]);
        let config = sample_config(ToolExecutionMode::Sequential);
        let (events, emit) = collect_emit();

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        let first = batch
            .messages
            .first()
            .ok_or_else(|| "missing image result".to_owned())?;
        if first.tool_call_id != "call-9" || first.tool_name != "img" {
            return Err("tool identity not preserved".to_owned());
        }
        if first.content.len() != 2 {
            return Err(format!(
                "expected text+image content, got {}",
                first.content.len()
            ));
        }
        if !matches!(first.content.get(1), Some(ToolResultContent::Image(_))) {
            return Err("missing image content block".to_owned());
        }

        let start_args = snapshot_events(&events)?
            .into_iter()
            .find_map(|event| match event {
                AgentEvent::ToolExecutionStart { args, .. } => Some(args),
                _ => None,
            });
        if start_args.as_ref() != Some(&args) {
            return Err(format!("start args wrong: {start_args:?}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn validate_failure_becomes_error_result() -> TestResult {
        let tool = RecordingTool {
            fail_validate: true,
            ..RecordingTool::new("bad")
        };
        let executed = Arc::clone(&tool.executed);
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("c1", "bad", Map::new())]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let emit = |_event: AgentEvent| {};

        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;

        if executed.load(Ordering::SeqCst) {
            return Err("invalid tool executed".to_owned());
        }
        let first = batch
            .messages
            .first()
            .ok_or_else(|| "missing validate error result".to_owned())?;
        if !first.is_error {
            return Err("validate failure must be error".to_owned());
        }
        let text = match first.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.as_str(),
            _ => "",
        };
        if text != "bad args" {
            return Err(format!("unexpected validate error text: {text}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn after_hook_can_set_terminate_for_batch() -> TestResult {
        let tool = RecordingTool {
            terminate: Some(false),
            ..RecordingTool::new("term")
        };
        let context = context_with(vec![Arc::new(tool)]);
        let assistant = assistant_with_calls(vec![ToolCall::new("c1", "term", Map::new())]);
        let mut config = sample_config(ToolExecutionMode::Sequential);
        config.after_tool_call = Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async {
                Ok(Some(AfterToolCallResult {
                    content: None,
                    details: None,
                    is_error: None,
                    terminate: Some(true),
                }))
            })
        }));
        let emit = |_event: AgentEvent| {};
        let batch = execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        if !batch.terminate {
            return Err("after_tool_call terminate not applied".to_owned());
        }
        Ok(())
    }

    #[test]
    fn should_terminate_requires_all_true() {
        assert!(!should_terminate_tool_batch(&[]));
        assert!(!should_terminate_tool_batch(&[
            AgentToolResult {
                terminate: Some(true),
                ..AgentToolResult::default()
            },
            AgentToolResult {
                terminate: Some(false),
                ..AgentToolResult::default()
            }
        ]));
        assert!(should_terminate_tool_batch(&[
            AgentToolResult {
                terminate: Some(true),
                ..AgentToolResult::default()
            },
            AgentToolResult {
                terminate: Some(true),
                ..AgentToolResult::default()
            }
        ]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_task_leaks_after_parallel_batch() -> TestResult {
        let before = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        let context = context_with(vec![
            Arc::new(RecordingTool {
                delay: Duration::from_millis(5),
                ..RecordingTool::new("a")
            }),
            Arc::new(RecordingTool {
                delay: Duration::from_millis(5),
                ..RecordingTool::new("b")
            }),
        ]);
        let assistant = assistant_with_calls(vec![
            ToolCall::new("1", "a", Map::new()),
            ToolCall::new("2", "b", Map::new()),
        ]);
        let config = sample_config(ToolExecutionMode::Parallel);
        let emit = |_event: AgentEvent| {};
        execute_tool_calls(
            &context,
            &assistant,
            &config,
            &CancellationToken::new(),
            &emit,
        )
        .await
        .map_err(|error| error.to_string())?;
        sleep(Duration::from_millis(20)).await;
        let after = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        if after > before + 1 {
            return Err(format!("task leak detected: before={before} after={after}"));
        }
        Ok(())
    }
}
