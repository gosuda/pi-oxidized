//! Low-level agent turn loop.
//!
//! Ports `runAgentLoop` / `runAgentLoopContinue` / `runLoop` /
//! `streamAssistantResponse` from the TypeScript agent loop. Terminal
//! `message_end` and `agent_end` events are emitted only here; [`ProviderDrain`]
//! returns the final assistant message and never publishes agent lifecycle
//! events.

use std::sync::Arc;

use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message,
    ModelThinkingLevel, Provider, ProviderError, StopReason, ToolResultMessage,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::{
    AgentContext, AgentLoopConfig, AgentLoopTurnUpdate, PrepareNextTurnContext,
    ShouldStopAfterTurnContext,
};
use crate::drain::{DRAIN_EVENT_CAPACITY, DrainItem, ProviderDrain};
use crate::error::AgentLoopError;
use crate::event::AgentEvent;
use crate::message::{AgentMessage, now_millis};
use crate::schedule::{EmitAgentEvent, execute_tool_calls, fail_tool_calls_from_truncated_message};
use crate::tool::to_pi_tool;

/// I/O handles required by one low-level agent loop invocation.
pub struct RunIo<'a> {
    /// Synchronous non-blocking event sink (session / UI / extensions).
    pub sink: &'a dyn crate::bus::EventSink,
    /// Provider used for each assistant stream.
    pub provider: &'a dyn Provider,
    /// Per-run partial assistant watch. Cleared to `None` after every stream.
    pub partial: watch::Sender<Option<Arc<AssistantMessage>>>,
}

/// Runs a prompt turn: injects `prompts`, then enters the shared loop.
///
/// # Transcript contract
///
/// On success, the returned messages are exactly this invocation's ordered
/// [`AgentEvent::MessageEnd`] payloads. When `io.sink` is an
/// [`crate::bus::AgentEventSink`] and no external transcript mutation races the
/// run, `messages_after = messages_before ++ returned_messages`.
///
/// # Errors
///
/// Returns [`AgentLoopError`] for unrecoverable hook / conversion failures.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    io: RunIo<'_>,
    cancel: CancellationToken,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    let mut new_messages = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt,
        messages: {
            let mut messages = context.messages;
            messages.extend(prompts.iter().cloned());
            messages
        },
        tools: context.tools,
    };

    io.sink.emit(AgentEvent::AgentStart);
    io.sink.emit(AgentEvent::TurnStart);
    for prompt in &prompts {
        emit_message_pair(io.sink, prompt.clone());
    }

    run_loop(&mut current_context, &mut new_messages, config, &io, cancel).await?;
    Ok(new_messages)
}

/// Continues from an existing context without injecting a new prompt.
///
/// The last message must be a user or tool-result message after conversion
/// would run; empty context and assistant-tailed context are rejected here
/// exactly as the TypeScript reference does.
///
/// # Errors
///
/// Returns [`AgentLoopError`] when the context cannot be continued, or when a
/// hook / conversion failure escapes the no-throw contract.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    io: RunIo<'_>,
    cancel: CancellationToken,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    if context.messages.is_empty() {
        return Err(AgentLoopError::message(
            "Cannot continue: no messages in context",
        ));
    }

    if context
        .messages
        .last()
        .is_some_and(|message| message.role() == "assistant")
    {
        return Err(AgentLoopError::message(
            "Cannot continue from message role: assistant",
        ));
    }

    let mut new_messages = Vec::new();
    let mut current_context = context;

    io.sink.emit(AgentEvent::AgentStart);
    io.sink.emit(AgentEvent::TurnStart);

    run_loop(&mut current_context, &mut new_messages, config, &io, cancel).await?;
    Ok(new_messages)
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    io: &RunIo<'_>,
    cancel: CancellationToken,
) -> Result<(), AgentLoopError> {
    let mut last_completed_turn: Option<PrepareNextTurnContext> = None;
    let mut pending_messages = poll_messages(config.get_steering_messages.as_ref()).await?;

    // Outer loop: re-enters when follow-up messages arrive after tools finish.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: stream, tools, steering.
        while has_more_tool_calls || !pending_messages.is_empty() {
            // A saved completed turn means the caller-emitted first turn is
            // over and this iteration starts a real subsequent provider
            // request; decide continuation before touching provider state.
            if let Some(completed_turn) = last_completed_turn.take() {
                if cancel.is_cancelled() {
                    emit_agent_end(io.sink, new_messages);
                    return Ok(());
                }

                apply_prepare_next_turn(
                    current_context,
                    &mut config,
                    completed_turn,
                    cancel.clone(),
                )
                .await?;

                if cancel.is_cancelled() {
                    emit_agent_end(io.sink, new_messages);
                    return Ok(());
                }

                // Messages queued while preparation ran join this same next
                // request; a pending poll delivery keeps one-at-a-time
                // semantics and skips the extra poll.
                if pending_messages.is_empty() {
                    pending_messages = poll_messages(config.get_steering_messages.as_ref()).await?;
                    if cancel.is_cancelled() {
                        emit_agent_end(io.sink, new_messages);
                        return Ok(());
                    }
                }

                io.sink.emit(AgentEvent::TurnStart);
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit_message_pair(io.sink, message.clone());
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message =
                stream_assistant_response(current_context, &config, io, cancel.clone()).await?;
            new_messages.push(assistant_agent_message(message.clone()));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                io.sink.emit(AgentEvent::TurnEnd {
                    message: assistant_agent_message(message),
                    tool_results: Vec::new(),
                });
                emit_agent_end(io.sink, new_messages);
                return Ok(());
            }

            let tool_results;
            has_more_tool_calls = false;
            if message_has_tool_calls(&message) {
                let emit = SinkEmit(io.sink);
                let batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&message, &emit)
                } else {
                    execute_tool_calls(current_context, &message, &config, &cancel, &emit).await?
                };
                tool_results = batch.messages;
                has_more_tool_calls = !batch.terminate;

                for result in &tool_results {
                    let agent_result = tool_result_agent_message(result.clone());
                    current_context.messages.push(agent_result.clone());
                    new_messages.push(agent_result);
                }
            } else {
                tool_results = Vec::new();
            }

            io.sink.emit(AgentEvent::TurnEnd {
                message: assistant_agent_message(message.clone()),
                tool_results: tool_results.clone(),
            });

            if cancel.is_cancelled() {
                emit_agent_end(io.sink, new_messages);
                return Ok(());
            }

            let completed_turn = PrepareNextTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            };

            if should_stop_after_turn(&config, &completed_turn).await? {
                emit_agent_end(io.sink, new_messages);
                return Ok(());
            }

            pending_messages = poll_messages(config.get_steering_messages.as_ref()).await?;

            if cancel.is_cancelled() {
                emit_agent_end(io.sink, new_messages);
                return Ok(());
            }

            last_completed_turn = Some(completed_turn);
        }

        let follow_up = poll_messages(config.get_follow_up_messages.as_ref()).await?;
        if follow_up.is_empty() {
            break;
        }
        pending_messages = follow_up;
    }

    emit_agent_end(io.sink, new_messages);
    Ok(())
}

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    io: &RunIo<'_>,
    cancel: CancellationToken,
) -> Result<AssistantMessage, AgentLoopError> {
    let messages = prepare_llm_messages(context, config, cancel.clone()).await?;
    let llm_context = Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages,
        tools: context_tools(context),
    };

    let resolved_key = resolve_api_key(config).await?;
    let options = config.build_stream_options(resolved_key, Some(cancel.child_token()));
    let stream = io.provider.stream(&config.model, llm_context, options);

    let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
    let drain_handle =
        ProviderDrain::spawn(stream, io.partial.clone(), event_tx, cancel.child_token());

    let result = consume_drain_items(&mut event_rx, context, config, io, &cancel).await;

    // Drop the receiver so the drain task can exit if it is still blocked on send.
    drop(event_rx);
    // Best-effort join; the drain exits on cancel, channel close, or final item.
    let _ = drain_handle.await;
    clear_partial(io);

    result
}

async fn consume_drain_items(
    event_rx: &mut mpsc::Receiver<DrainItem>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    io: &RunIo<'_>,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, AgentLoopError> {
    let mut added_partial = false;

    loop {
        let item = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Cancellation while waiting for the next drain item: synthesize
                // a single aborted final. Closing the receiver after return
                // ensures a later drain terminal cannot double-finalize.
                let final_message =
                    synthesize_stream_failure(config, cancel, "stream cancelled");
                return Ok(finalize_assistant(
                    context,
                    io,
                    final_message,
                    added_partial,
                ));
            }
            item = event_rx.recv() => item,
        };
        // Cancellation owns the terminal outcome even when the provider item
        // became ready in the same scheduler turn after select chose it.
        if cancel.is_cancelled() {
            let final_message = synthesize_stream_failure(config, cancel, "stream cancelled");
            return Ok(finalize_assistant(
                context,
                io,
                final_message,
                added_partial,
            ));
        }

        let Some(item) = item else {
            // Early close / empty stream without a terminal event.
            let final_message = synthesize_stream_failure(
                config,
                cancel,
                "provider stream closed without a final message",
            );
            return Ok(finalize_assistant(
                context,
                io,
                final_message,
                added_partial,
            ));
        };

        match item {
            DrainItem::Event(event) => match *event {
                AssistantMessageEvent::Start { partial } => {
                    let message = assistant_agent_message(Arc::unwrap_or_clone(partial));
                    context.messages.push(message.clone());
                    added_partial = true;
                    io.sink.emit(AgentEvent::MessageStart { message });
                }
                AssistantMessageEvent::Done { message, .. } => {
                    return Ok(finalize_assistant(context, io, message, added_partial));
                }
                AssistantMessageEvent::Error { error, .. } => {
                    return Ok(finalize_assistant(context, io, error, added_partial));
                }
                event => {
                    let partial = match &event {
                        AssistantMessageEvent::TextStart { partial, .. }
                        | AssistantMessageEvent::TextDelta { partial, .. }
                        | AssistantMessageEvent::TextEnd { partial, .. }
                        | AssistantMessageEvent::ThinkingStart { partial, .. }
                        | AssistantMessageEvent::ThinkingDelta { partial, .. }
                        | AssistantMessageEvent::ThinkingEnd { partial, .. }
                        | AssistantMessageEvent::ToolCallStart { partial, .. }
                        | AssistantMessageEvent::ToolCallDelta { partial, .. }
                        | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
                        AssistantMessageEvent::Start { .. }
                        | AssistantMessageEvent::Done { .. }
                        | AssistantMessageEvent::Error { .. } => unreachable!(),
                    };
                    if added_partial {
                        // One owned assistant-message copy plus one Arc per
                        // frame: the partial Arc stays shared with the boxed
                        // provider event forwarded below.
                        let message = Arc::new(assistant_agent_message(partial.as_ref().clone()));
                        io.sink.emit(AgentEvent::MessageUpdate {
                            message,
                            assistant_message_event: Box::new(event),
                        });
                    }
                }
            },
            DrainItem::Infra(error) => {
                let final_message = synthesize_from_provider_error(config, cancel, &error);
                return Ok(finalize_assistant(
                    context,
                    io,
                    final_message,
                    added_partial,
                ));
            }
        }
    }
}

fn finalize_assistant(
    context: &mut AgentContext,
    io: &RunIo<'_>,
    final_message: AssistantMessage,
    added_partial: bool,
) -> AssistantMessage {
    if added_partial {
        replace_last_assistant(context, final_message.clone());
    } else {
        context
            .messages
            .push(assistant_agent_message(final_message.clone()));
        io.sink.emit(AgentEvent::MessageStart {
            message: assistant_agent_message(final_message.clone()),
        });
    }
    io.sink.emit(AgentEvent::MessageEnd {
        message: assistant_agent_message(final_message.clone()),
    });
    final_message
}

fn synthesize_from_provider_error(
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    error: &ProviderError,
) -> AssistantMessage {
    synthesize_stream_failure(config, cancel, error.message())
}

fn synthesize_stream_failure(
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    message: &str,
) -> AssistantMessage {
    let mut assistant = AssistantMessage::new(
        config.model.api.clone(),
        config.model.provider.clone(),
        config.model.id.clone(),
        now_millis(),
    );
    if cancel.is_cancelled() {
        assistant.stop_reason = StopReason::Aborted;
    } else {
        assistant.stop_reason = StopReason::Error;
    }
    assistant.error_message = Some(message.to_owned());
    assistant
}

async fn prepare_llm_messages(
    context: &AgentContext,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
) -> Result<Vec<Message>, AgentLoopError> {
    let mut messages = context.messages.clone();
    if let Some(transform) = config.transform_context.as_ref() {
        messages = transform(messages, cancel).await?;
    }
    (config.convert_to_llm)(messages).await
}

async fn resolve_api_key(config: &AgentLoopConfig) -> Result<Option<String>, AgentLoopError> {
    if let Some(get_api_key) = config.get_api_key.as_ref() {
        return get_api_key(config.model.provider.clone()).await;
    }
    Ok(None)
}

fn context_tools(context: &AgentContext) -> Option<Vec<pi_ai::Tool>> {
    if context.tools.is_empty() {
        None
    } else {
        Some(
            context
                .tools
                .iter()
                .map(|tool| to_pi_tool(tool.as_ref()))
                .collect(),
        )
    }
}

async fn poll_messages(
    hook: Option<&crate::config::GetMessages>,
) -> Result<Vec<AgentMessage>, AgentLoopError> {
    match hook {
        Some(get) => get().await,
        None => Ok(Vec::new()),
    }
}

async fn apply_prepare_next_turn(
    current_context: &mut AgentContext,
    config: &mut AgentLoopConfig,
    completed_turn: PrepareNextTurnContext,
    cancel: CancellationToken,
) -> Result<(), AgentLoopError> {
    let Some(prepare) = config.prepare_next_turn.clone() else {
        return Ok(());
    };
    let update = prepare(completed_turn, cancel).await?;
    apply_turn_update(current_context, config, update);
    Ok(())
}

fn apply_turn_update(
    current_context: &mut AgentContext,
    config: &mut AgentLoopConfig,
    update: Option<AgentLoopTurnUpdate>,
) {
    let Some(update) = update else {
        return;
    };
    if let Some(context) = update.context {
        *current_context = context;
    }
    if let Some(model) = update.model {
        config.model = model;
    }
    if let Some(level) = update.thinking_level {
        config.reasoning = if level == ModelThinkingLevel::Off {
            None
        } else {
            Some(level)
        };
    }
}

async fn should_stop_after_turn(
    config: &AgentLoopConfig,
    completed_turn: &ShouldStopAfterTurnContext,
) -> Result<bool, AgentLoopError> {
    let Some(should_stop) = config.should_stop_after_turn.as_ref() else {
        return Ok(false);
    };
    should_stop(completed_turn.clone()).await
}

fn message_has_tool_calls(message: &AssistantMessage) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)))
}

fn replace_last_assistant(context: &mut AgentContext, message: AssistantMessage) {
    if let Some(last) = context.messages.last_mut() {
        *last = assistant_agent_message(message);
    }
}

fn assistant_agent_message(message: AssistantMessage) -> AgentMessage {
    AgentMessage::Llm(Box::new(Message::Assistant(message)))
}

fn tool_result_agent_message(message: ToolResultMessage) -> AgentMessage {
    AgentMessage::Llm(Box::new(Message::ToolResult(message)))
}

fn emit_message_pair(sink: &dyn crate::bus::EventSink, message: AgentMessage) {
    sink.emit(AgentEvent::MessageStart {
        message: message.clone(),
    });
    sink.emit(AgentEvent::MessageEnd { message });
}

fn emit_agent_end(sink: &dyn crate::bus::EventSink, messages: &[AgentMessage]) {
    sink.emit(AgentEvent::AgentEnd {
        messages: messages.to_vec(),
    });
}

fn clear_partial(io: &RunIo<'_>) {
    let _ = io.partial.send(None);
}

/// Bridges [`crate::bus::EventSink`] into the scheduler's emit trait.
struct SinkEmit<'a>(&'a dyn crate::bus::EventSink);

impl EmitAgentEvent for SinkEmit<'_> {
    fn emit(&self, event: AgentEvent) {
        self.0.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::LazyLock;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantContent, DoneReason, ErrorReason, Model, ModelCost, ModelInput,
        ModelThinkingLevel, StreamOptionKey, TextContent, ToolCall, ToolResultContent,
    };
    use serde_json::{Map, Value, json};
    use tokio::sync::watch;
    use tokio::time::sleep;

    use crate::bus::{AgentEventSink, EventSink, ExtensionEvent};
    use crate::config::{AgentLoopTurnUpdate, default_convert_to_llm_hook, text_user_message};
    use crate::error::ToolError;
    use crate::message::{CustomAgentMessage, default_convert_to_llm};
    use crate::state::AgentState;
    use crate::tool::{AgentTool, AgentToolResult, ToolExecutionMode, ToolUpdates};

    type TestResult = Result<(), String>;

    static EMPTY_OBJECT_SCHEMA: LazyLock<Value> =
        LazyLock::new(|| json!({"type":"object","properties":{}}));

    type ScriptItem = Result<AssistantMessageEvent, ProviderError>;
    type Script = Vec<ScriptItem>;

    type OrderLog = Arc<Mutex<Vec<&'static str>>>;

    fn record_order(log: &OrderLog, entry: &'static str) {
        if let Ok(mut guard) = log.lock() {
            guard.push(entry);
        }
    }

    #[derive(Clone)]
    struct ScriptedProvider {
        scripts: Arc<Mutex<Vec<Script>>>,
        contexts: Arc<Mutex<Vec<Context>>>,
        models: Arc<Mutex<Vec<String>>>,
        reasoning: Arc<Mutex<Vec<Option<String>>>>,
        order_log: Option<OrderLog>,
        hang_after: Option<usize>,
        item_delay: Duration,
        delivered: Arc<AtomicUsize>,
        cancel_seen: Arc<AtomicBool>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Script>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                contexts: Arc::new(Mutex::new(Vec::new())),
                models: Arc::new(Mutex::new(Vec::new())),
                reasoning: Arc::new(Mutex::new(Vec::new())),
                order_log: None,
                hang_after: None,
                item_delay: Duration::ZERO,
                delivered: Arc::new(AtomicUsize::new(0)),
                cancel_seen: Arc::new(AtomicBool::new(false)),
            }
        }
        fn paced(scripts: Vec<Script>, item_delay: Duration) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                contexts: Arc::new(Mutex::new(Vec::new())),
                models: Arc::new(Mutex::new(Vec::new())),
                reasoning: Arc::new(Mutex::new(Vec::new())),
                order_log: None,
                hang_after: None,
                item_delay,
                delivered: Arc::new(AtomicUsize::new(0)),
                cancel_seen: Arc::new(AtomicBool::new(false)),
            }
        }

        fn hanging(scripts: Vec<Script>, hang_after: usize) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                contexts: Arc::new(Mutex::new(Vec::new())),
                models: Arc::new(Mutex::new(Vec::new())),
                reasoning: Arc::new(Mutex::new(Vec::new())),
                order_log: None,
                hang_after: Some(hang_after),
                item_delay: Duration::ZERO,
                delivered: Arc::new(AtomicUsize::new(0)),
                cancel_seen: Arc::new(AtomicBool::new(false)),
            }
        }

        fn call_count(&self) -> usize {
            self.contexts.lock().map_or(0, |guard| guard.len())
        }

        fn last_context(&self) -> Option<Context> {
            self.contexts
                .lock()
                .ok()
                .and_then(|guard| guard.last().cloned())
        }

        fn model_ids(&self) -> Vec<String> {
            self.models
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }

        fn reasoning_values(&self) -> Vec<Option<String>> {
            self.reasoning
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default()
        }

        fn with_order_log(mut self, log: OrderLog) -> Self {
            self.order_log = Some(log);
            self
        }
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            model: &Model,
            context: Context,
            options: pi_ai::StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            if let Some(log) = &self.order_log
                && let Ok(mut guard) = log.lock()
            {
                guard.push("stream");
            }
            if let Ok(mut guard) = self.models.lock() {
                guard.push(model.id.clone());
            }
            if let Ok(mut guard) = self.reasoning.lock() {
                guard.push(
                    options
                        .extra_value(StreamOptionKey::REASONING)
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
            }
            if let Ok(mut guard) = self.contexts.lock() {
                guard.push(context);
            }

            let script = self
                .scripts
                .lock()
                .ok()
                .and_then(|mut guard| {
                    if guard.is_empty() {
                        None
                    } else {
                        Some(guard.remove(0))
                    }
                })
                .unwrap_or_default();
            let items = Arc::new(script);
            let delivered = Arc::clone(&self.delivered);
            let hang_after = self.hang_after;
            let item_delay = self.item_delay;
            let cancel_seen = Arc::clone(&self.cancel_seen);
            let signal = options.signal.clone();

            stream::unfold(0usize, move |index| {
                let items = Arc::clone(&items);
                let delivered = Arc::clone(&delivered);
                let cancel_seen = Arc::clone(&cancel_seen);
                let signal = signal.clone();
                let item_delay = item_delay;
                async move {
                    if let Some(token) = signal.as_ref()
                        && token.is_cancelled()
                    {
                        cancel_seen.store(true, Ordering::SeqCst);
                    }
                    if hang_after.is_some_and(|limit| index >= limit) {
                        if let Some(token) = signal.as_ref() {
                            token.cancelled().await;
                            cancel_seen.store(true, Ordering::SeqCst);
                        } else {
                            std::future::pending::<()>().await;
                        }
                        return None;
                    }
                    if index >= items.len() {
                        return None;
                    }
                    if !item_delay.is_zero() {
                        sleep(item_delay).await;
                    }
                    delivered.fetch_add(1, Ordering::SeqCst);
                    Some((items[index].clone(), index + 1))
                }
            })
            .boxed()
        }
    }

    struct CollectSink {
        events: Arc<Mutex<Vec<AgentEvent>>>,
    }

    impl CollectSink {
        fn new() -> (Self, Arc<Mutex<Vec<AgentEvent>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: Arc::clone(&events),
                },
                events,
            )
        }
    }

    impl EventSink for CollectSink {
        fn emit(&self, event: AgentEvent) {
            if let Ok(mut guard) = self.events.lock() {
                guard.push(event);
            }
        }
    }

    struct RecordingTool {
        name: String,
        executed: Arc<AtomicUsize>,
        delay: Duration,
        result_text: String,
        terminate: bool,
    }

    impl RecordingTool {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                executed: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
                result_text: format!("{name}-ok"),
                terminate: false,
            }
        }

        fn terminating(name: &str) -> Self {
            Self {
                terminate: true,
                ..Self::new(name)
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
            &EMPTY_OBJECT_SCHEMA
        }

        fn validate_arguments(
            &self,
            args: &Map<String, Value>,
        ) -> Result<Map<String, Value>, ToolError> {
            Ok(args.clone())
        }

        fn execute(
            &self,
            _tool_call_id: &str,
            _args: Map<String, Value>,
            cancel: CancellationToken,
            _updates: ToolUpdates,
        ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
            let executed = Arc::clone(&self.executed);
            let delay = self.delay;
            let result_text = self.result_text.clone();
            let terminate = self.terminate;
            Box::pin(async move {
                executed.fetch_add(1, Ordering::SeqCst);
                if !delay.is_zero() {
                    tokio::select! {
                        () = sleep(delay) => {}
                        () = cancel.cancelled() => {}
                    }
                }
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent::new(result_text))],
                    details: json!({}),
                    added_tool_names: None,
                    terminate: terminate.then_some(true),
                })
            })
        }
    }

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

    fn sample_config() -> AgentLoopConfig {
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
            tool_execution: ToolExecutionMode::Parallel,
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
            telemetry: crate::telemetry::noop_context(),
        }
    }

    fn assistant(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::new("openai-completions", "openai", "m", 1);
        if !text.is_empty() {
            message
                .content
                .push(AssistantContent::Text(TextContent::new(text)));
        }
        message
    }

    fn start(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::Start {
            partial: Arc::new(assistant(text)),
        }
    }

    fn text_delta(text: &str, delta: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: delta.into(),
            partial: Arc::new(assistant(text)),
        }
    }

    fn done_text(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Stop;
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        }
    }

    fn done_tool(calls: Vec<ToolCall>) -> AssistantMessageEvent {
        let mut message = AssistantMessage::new("openai-completions", "openai", "m", 1);
        message.content = calls.into_iter().map(AssistantContent::ToolCall).collect();
        message.stop_reason = StopReason::ToolUse;
        AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            message,
        }
    }

    fn length_tool(calls: Vec<ToolCall>) -> AssistantMessageEvent {
        let mut message = AssistantMessage::new("openai-completions", "openai", "m", 1);
        message.content = calls.into_iter().map(AssistantContent::ToolCall).collect();
        message.stop_reason = StopReason::Length;
        AssistantMessageEvent::Done {
            reason: DoneReason::Length,
            message,
        }
    }

    fn error_event(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Error;
        message.error_message = Some(text.into());
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: message,
        }
    }

    fn aborted_event(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some(text.into());
        AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: message,
        }
    }

    fn text_script(text: &str) -> Vec<Result<AssistantMessageEvent, ProviderError>> {
        vec![
            Ok(start("")),
            Ok(text_delta(text, text)),
            Ok(done_text(text)),
        ]
    }

    fn tool_script(id: &str, name: &str) -> Vec<Result<AssistantMessageEvent, ProviderError>> {
        vec![
            Ok(start("")),
            Ok(done_tool(vec![ToolCall::new(id, name, Map::new())])),
        ]
    }

    fn event_types(events: &[AgentEvent]) -> Vec<&'static str> {
        events.iter().map(event_type).collect()
    }

    fn event_type(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }

    fn snapshot(events: &Arc<Mutex<Vec<AgentEvent>>>) -> Result<Vec<AgentEvent>, String> {
        events
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| "event mutex poisoned".to_owned())
    }

    fn count_type(events: &[AgentEvent], ty: &str) -> usize {
        events
            .iter()
            .filter(|event| event_type(event) == ty)
            .count()
    }

    fn count_assistant_message_end(events: &[AgentEvent]) -> usize {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd {
                        message: AgentMessage::Llm(inner)
                    } if matches!(inner.as_ref(), Message::Assistant(_))
                )
            })
            .count()
    }

    async fn run_prompt(
        prompts: Vec<AgentMessage>,
        context: AgentContext,
        config: AgentLoopConfig,
        provider: &ScriptedProvider,
        cancel: CancellationToken,
    ) -> Result<
        (
            Vec<AgentMessage>,
            Vec<AgentEvent>,
            watch::Receiver<Option<Arc<AssistantMessage>>>,
        ),
        String,
    > {
        let (sink, events) = CollectSink::new();
        let (partial_tx, partial_rx) = watch::channel(None);
        let io = RunIo {
            sink: &sink,
            provider,
            partial: partial_tx,
        };
        let messages = run_agent_loop(prompts, context, config, io, cancel)
            .await
            .map_err(|err| err.to_string())?;
        let events = snapshot(&events)?;
        Ok((messages, events, partial_rx))
    }

    fn base_context(tools: Vec<Arc<dyn AgentTool>>) -> AgentContext {
        AgentContext {
            system_prompt: "sys".to_owned(),
            messages: Vec::new(),
            tools,
        }
    }

    fn user_text_contains(message: &AgentMessage, needle: &str) -> bool {
        match message.as_llm() {
            Some(Message::User(user)) => match &user.content {
                pi_ai::UserMessageContent::Text(text) => text.contains(needle),
                pi_ai::UserMessageContent::Blocks(blocks) => {
                    blocks.iter().any(|block| match block {
                        pi_ai::UserContent::Text(text) => text.text.contains(needle),
                        pi_ai::UserContent::Image(_) => false,
                    })
                }
            },
            _ => false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn text_turn_event_order() -> TestResult {
        let provider = ScriptedProvider::new(vec![text_script("hello")]);
        let (_messages, events, partial_rx) = run_prompt(
            vec![text_user_message("hi")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(
            event_types(&events),
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_update",
                "message_end",
                "turn_end",
                "agent_end",
            ]
        );
        assert_eq!(count_type(&events, "agent_end"), 1);
        assert_eq!(count_assistant_message_end(&events), 1);
        assert!(partial_rx.borrow().is_none(), "partial must clear");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tool_turn_event_order() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider = ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("done")]);
        let mut config = sample_config();
        config.tool_execution = ToolExecutionMode::Sequential;

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("use tool")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(
            event_types(&events),
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_end",
                "tool_execution_start",
                "tool_execution_end",
                "message_start",
                "message_end",
                "turn_end",
                "turn_start",
                "message_start",
                "message_update",
                "message_end",
                "turn_end",
                "agent_end",
            ]
        );
        assert_eq!(count_type(&events, "agent_end"), 1);
        assert_eq!(provider.call_count(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_transcript_is_prior_plus_ordered_message_ends() -> TestResult {
        let prior = vec![text_user_message("prior")];
        let mut initial_state = AgentState::new();
        initial_state.messages = prior.clone();
        let state = Arc::new(Mutex::new(initial_state));
        let sink = AgentEventSink::new(Arc::clone(&state));
        let mut subscription = sink.subscribe();

        let tool = Arc::new(RecordingTool::new("read"));
        let provider = ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("done")]);
        let mut config = sample_config();
        config.tool_execution = ToolExecutionMode::Sequential;
        let mut context = base_context(vec![tool]);
        context.messages = prior.clone();
        let (partial_tx, _) = watch::channel(None);
        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx,
        };

        let new_messages = run_agent_loop(
            vec![text_user_message("use tool")],
            context,
            config,
            io,
            CancellationToken::new(),
        )
        .await
        .map_err(|error| error.to_string())?;

        let mut message_ends = Vec::new();
        while let Ok(event) = subscription.try_recv() {
            if let AgentEvent::MessageEnd { message } = event {
                message_ends.push(message);
            }
        }
        assert!(
            !subscription.is_lagged(),
            "the contract witness must observe every run event"
        );
        assert_eq!(
            new_messages, message_ends,
            "the returned run delta must equal ordered message_end payloads"
        );
        let message_roles: Vec<_> = message_ends.iter().map(AgentMessage::role).collect();
        assert_eq!(
            message_roles,
            ["user", "assistant", "toolResult", "assistant"],
            "the witness must exercise each run-owned message kind in order"
        );

        let mut expected_transcript = prior;
        expected_transcript.extend(message_ends);
        let transcript = state
            .lock()
            .map_err(|_| "agent state mutex poisoned".to_owned())?
            .messages
            .clone();
        assert_eq!(
            transcript, expected_transcript,
            "the reducer must append each message_end payload in order"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_after_tools_ends_without_another_turn_or_hooks() -> TestResult {
        let tool = Arc::new(RecordingTool::new("cancel-me"));
        let provider = ScriptedProvider::new(vec![
            vec![
                Ok(start("")),
                Ok(done_tool(vec![
                    ToolCall::new("c1", "cancel-me", Map::new()),
                    ToolCall::new("c2", "cancel-me", Map::new()),
                    ToolCall::new("c3", "cancel-me", Map::new()),
                ])),
            ],
            text_script("must-not-start"),
        ]);
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let stop_calls = Arc::new(AtomicUsize::new(0));
        let steering_polls = Arc::new(AtomicUsize::new(0));
        let follow_up_polls = Arc::new(AtomicUsize::new(0));
        let mut config = sample_config();
        config.tool_execution = ToolExecutionMode::Sequential;
        config.before_tool_call = Some(Arc::new(|context, cancel| {
            Box::pin(async move {
                if context.tool_call.id == "c1" {
                    cancel.cancel();
                }
                Ok(None)
            })
        }));
        let prepare_calls_hook = Arc::clone(&prepare_calls);
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let prepare_calls_hook = Arc::clone(&prepare_calls_hook);
            Box::pin(async move {
                prepare_calls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
        }));
        let stop_calls_hook = Arc::clone(&stop_calls);
        config.should_stop_after_turn = Some(Arc::new(move |_| {
            let stop_calls_hook = Arc::clone(&stop_calls_hook);
            Box::pin(async move {
                stop_calls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
        }));
        let steering_polls_hook = Arc::clone(&steering_polls);
        config.get_steering_messages = Some(Arc::new(move || {
            let steering_polls_hook = Arc::clone(&steering_polls_hook);
            Box::pin(async move {
                steering_polls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        }));
        let follow_up_polls_hook = Arc::clone(&follow_up_polls);
        config.get_follow_up_messages = Some(Arc::new(move || {
            let follow_up_polls_hook = Arc::clone(&follow_up_polls_hook);
            Box::pin(async move {
                follow_up_polls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("cancel tools")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        let result_ids = events.iter().find_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } if !tool_results.is_empty() => Some(
                tool_results
                    .iter()
                    .map(|result| result.tool_call_id.as_str())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        });
        assert_eq!(result_ids, Some(vec!["c1"]));
        assert_eq!(provider.call_count(), 1);
        assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
        assert_eq!(stop_calls.load(Ordering::SeqCst), 0);
        assert_eq!(steering_polls.load(Ordering::SeqCst), 1);
        assert_eq!(follow_up_polls.load(Ordering::SeqCst), 0);
        assert_eq!(
            &event_types(&events)[events.len() - 2..],
            ["turn_end", "agent_end"]
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steering_injects_after_tools() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let steer = Arc::new(Mutex::new(vec![text_user_message("steer-now")]));
        let steer_polls = Arc::new(AtomicUsize::new(0));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("after-steer")]);
        let mut config = sample_config();
        let steer_q = Arc::clone(&steer);
        let steer_polls_hook = Arc::clone(&steer_polls);
        config.get_steering_messages = Some(Arc::new(move || {
            let steer_q = Arc::clone(&steer_q);
            let steer_polls_hook = Arc::clone(&steer_polls_hook);
            Box::pin(async move {
                if steer_polls_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(Vec::new());
                }
                let mut guard = steer_q
                    .lock()
                    .map_err(|_| AgentLoopError::message("poisoned"))?;
                Ok(std::mem::take(&mut *guard))
            })
        }));

        let (messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        let types = event_types(&events);
        let first_tool_end = types
            .iter()
            .position(|ty| *ty == "tool_execution_end")
            .ok_or("missing tool end")?;
        let steer_start = events
            .iter()
            .enumerate()
            .find_map(|(idx, event)| match event {
                AgentEvent::MessageStart { message }
                    if idx > first_tool_end && user_text_contains(message, "steer-now") =>
                {
                    Some(idx)
                }
                _ => None,
            })
            .ok_or("missing steer message_start")?;
        if steer_start <= first_tool_end {
            return Err("steering appeared before tools finished".to_owned());
        }
        assert_eq!(count_type(&events, "agent_end"), 1);
        assert!(
            messages.iter().any(|m| user_text_contains(m, "steer-now")),
            "steering message missing from returned messages"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn follow_up_starts_new_outer_turn() -> TestResult {
        let follow = Arc::new(Mutex::new(vec![text_user_message("follow")]));
        let provider = ScriptedProvider::new(vec![text_script("first"), text_script("second")]);
        let mut config = sample_config();
        let follow_q = Arc::clone(&follow);
        config.get_follow_up_messages = Some(Arc::new(move || {
            let follow_q = Arc::clone(&follow_q);
            Box::pin(async move {
                let mut guard = follow_q
                    .lock()
                    .map_err(|_| AgentLoopError::message("poisoned"))?;
                Ok(std::mem::take(&mut *guard))
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(provider.call_count(), 2);
        assert_eq!(count_type(&events, "turn_start"), 2);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    fn context_has_user_text(context: &Context, needle: &str) -> bool {
        context.messages.iter().any(|message| {
            match message {
            Message::User(user) => match &user.content {
                pi_ai::UserMessageContent::Text(text) => text.contains(needle),
                pi_ai::UserMessageContent::Blocks(blocks) => blocks.iter().any(|block| {
                    matches!(block, pi_ai::UserContent::Text(text) if text.text.contains(needle))
                }),
            },
            _ => false,
        }
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_next_turn_updates_model_and_reasoning() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider = ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("next")]);
        let mut config = sample_config();
        let mut next_model = sample_model();
        next_model.id = "m2".to_owned();
        let next_model_for_hook = next_model.clone();
        let order: OrderLog = Arc::new(Mutex::new(Vec::new()));
        let order_for_hook = Arc::clone(&order);
        let provider = provider.with_order_log(Arc::clone(&order));
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let next_model = next_model_for_hook.clone();
            let order = Arc::clone(&order_for_hook);
            Box::pin(async move {
                record_order(&order, "prepare");
                Ok(Some(AgentLoopTurnUpdate {
                    context: None,
                    model: Some(next_model),
                    thinking_level: Some(ModelThinkingLevel::High),
                }))
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(provider.call_count(), 2);
        assert_eq!(count_type(&events, "agent_end"), 1);
        assert_eq!(
            provider.model_ids(),
            vec!["m".to_owned(), "m2".to_owned()],
            "request two must use the model prepared after turn one"
        );
        assert_eq!(
            provider.reasoning_values(),
            vec![None, Some("high".to_owned())],
            "request two must carry the prepared thinking level"
        );
        let order = order
            .lock()
            .map_err(|_| "order mutex poisoned".to_owned())?;
        let stream_positions: Vec<usize> = order
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (*entry == "stream").then_some(index))
            .collect();
        let prepare_position = order
            .iter()
            .position(|entry| *entry == "prepare")
            .ok_or("prepare never recorded")?;
        assert_eq!(stream_positions.len(), 2);
        assert!(
            stream_positions[0] < prepare_position && prepare_position < stream_positions[1],
            "prepare must run between the two provider requests"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_after_turn_precedes_prepare_next_turn() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("unused")]);
        let mut config = sample_config();
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let stop_calls = Arc::new(AtomicUsize::new(0));
        let order: OrderLog = Arc::new(Mutex::new(Vec::new()));
        let prepare_calls_hook = Arc::clone(&prepare_calls);
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let prepare_calls_hook = Arc::clone(&prepare_calls_hook);
            Box::pin(async move {
                prepare_calls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
        }));
        let stop_calls_hook = Arc::clone(&stop_calls);
        let order_for_stop = Arc::clone(&order);
        config.should_stop_after_turn = Some(Arc::new(move |_| {
            let stop_calls_hook = Arc::clone(&stop_calls_hook);
            let order = Arc::clone(&order_for_stop);
            Box::pin(async move {
                stop_calls_hook.fetch_add(1, Ordering::SeqCst);
                record_order(&order, "stop");
                Ok(true)
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            prepare_calls.load(Ordering::SeqCst),
            0,
            "a stopped turn must never reach prepare_next_turn"
        );
        assert_eq!(provider.call_count(), 1);
        assert_eq!(count_type(&events, "turn_start"), 1);
        assert_eq!(count_type(&events, "agent_end"), 1);
        let order = order
            .lock()
            .map_err(|_| "order mutex poisoned".to_owned())?;
        assert_eq!(order.len(), 1);
        assert_eq!(order[0], "stop");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminating_tool_result_skips_prepare_next_turn() -> TestResult {
        let tool = Arc::new(RecordingTool::terminating("finish"));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "finish"), text_script("unused")]);
        let mut config = sample_config();
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let prepare_calls_hook = Arc::clone(&prepare_calls);
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let prepare_calls_hook = Arc::clone(&prepare_calls_hook);
            Box::pin(async move {
                prepare_calls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(
            prepare_calls.load(Ordering::SeqCst),
            0,
            "a terminating batch without queued work must skip preparation"
        );
        assert_eq!(provider.call_count(), 1);
        assert_eq!(count_type(&events, "agent_start"), 1);
        assert_eq!(count_type(&events, "turn_start"), 1);
        assert_eq!(count_type(&events, "turn_end"), 1);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn steering_queued_during_prepare_reaches_same_next_turn() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("after-steer")]);
        let mut config = sample_config();
        let queue: Arc<Mutex<Vec<AgentMessage>>> = Arc::new(Mutex::new(Vec::new()));
        let steer_polls = Arc::new(AtomicUsize::new(0));
        let prepare_started = Arc::new(AtomicBool::new(false));
        let release_prepare = Arc::new(tokio::sync::Notify::new());

        let steer_polls_hook = Arc::clone(&steer_polls);
        let queue_for_hook = Arc::clone(&queue);
        config.get_steering_messages = Some(Arc::new(move || {
            let steer_polls_hook = Arc::clone(&steer_polls_hook);
            let queue = Arc::clone(&queue_for_hook);
            Box::pin(async move {
                steer_polls_hook.fetch_add(1, Ordering::SeqCst);
                let mut guard = queue
                    .lock()
                    .map_err(|_| AgentLoopError::message("poisoned"))?;
                if guard.is_empty() {
                    return Ok(Vec::new());
                }
                Ok(vec![guard.remove(0)])
            })
        }));
        let prepare_started_hook = Arc::clone(&prepare_started);
        let release_for_hook = Arc::clone(&release_prepare);
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let prepare_started_hook = Arc::clone(&prepare_started_hook);
            let release = Arc::clone(&release_for_hook);
            Box::pin(async move {
                prepare_started_hook.store(true, Ordering::SeqCst);
                release.notified().await;
                Ok(None)
            })
        }));

        let provider = Arc::new(provider);
        let task_provider = Arc::clone(&provider);
        let task_tool = Arc::clone(&tool);
        let run = tokio::spawn(async move {
            run_prompt(
                vec![text_user_message("prompt")],
                base_context(vec![task_tool]),
                config,
                &task_provider,
                CancellationToken::new(),
            )
            .await
        });

        let mut waited = 0usize;
        while !prepare_started.load(Ordering::SeqCst) {
            waited += 1;
            if waited > 5_000 {
                return Err("prepare never started".to_owned());
            }
            sleep(Duration::from_millis(1)).await;
        }
        queue
            .lock()
            .map_err(|_| "queue mutex poisoned".to_owned())?
            .push(text_user_message("steer-during-prepare"));
        release_prepare.notify_one();

        let (_messages, events, _) = run.await.map_err(|error| error.to_string())??;
        assert_eq!(
            steer_polls.load(Ordering::SeqCst),
            4,
            "polls: initial, bottom, guarded post-prepare, bottom after final turn"
        );
        assert_eq!(provider.call_count(), 2);
        let context = provider.last_context().ok_or("no provider context")?;
        assert!(
            context_has_user_text(&context, "steer-during-prepare"),
            "steering queued during prepare must reach the same next request"
        );
        assert_eq!(count_type(&events, "agent_start"), 1);
        assert_eq!(count_type(&events, "turn_start"), 2);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepare_skips_extra_poll_when_steering_already_delivered() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("after-steer")]);
        let mut config = sample_config();
        let steer_polls = Arc::new(AtomicUsize::new(0));
        let polls_at_prepare = Arc::new(AtomicUsize::new(0));

        let steer_polls_hook = Arc::clone(&steer_polls);
        config.get_steering_messages = Some(Arc::new(move || {
            let steer_polls_hook = Arc::clone(&steer_polls_hook);
            Box::pin(async move {
                let poll = steer_polls_hook.fetch_add(1, Ordering::SeqCst) + 1;
                if poll == 2 {
                    return Ok(vec![text_user_message("steer-early")]);
                }
                Ok(Vec::new())
            })
        }));
        let steer_polls_for_prepare = Arc::clone(&steer_polls);
        let polls_at_prepare_hook = Arc::clone(&polls_at_prepare);
        config.prepare_next_turn = Some(Arc::new(move |_, _| {
            let steer_polls = Arc::clone(&steer_polls_for_prepare);
            let polls_at_prepare = Arc::clone(&polls_at_prepare_hook);
            Box::pin(async move {
                polls_at_prepare.store(steer_polls.load(Ordering::SeqCst), Ordering::SeqCst);
                Ok(None)
            })
        }));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(
            polls_at_prepare.load(Ordering::SeqCst),
            2,
            "prepare must run right after the bottom poll delivered steering"
        );
        assert_eq!(
            steer_polls.load(Ordering::SeqCst),
            3,
            "no extra poll may run between the delivered poll and prepare"
        );
        assert_eq!(provider.call_count(), 2);
        let context = provider.last_context().ok_or("no provider context")?;
        assert!(
            context_has_user_text(&context, "steer-early"),
            "delivered steering must still be part of request two"
        );
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_during_prepare_stops_before_next_request() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let provider =
            ScriptedProvider::new(vec![tool_script("c1", "read"), text_script("unused")]);
        let mut config = sample_config();
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let prepare_started = Arc::new(AtomicBool::new(false));
        let hook_saw_cancel = Arc::new(AtomicBool::new(false));
        let prepare_calls_hook = Arc::clone(&prepare_calls);
        let prepare_started_hook = Arc::clone(&prepare_started);
        let hook_saw_cancel_hook = Arc::clone(&hook_saw_cancel);
        config.prepare_next_turn = Some(Arc::new(move |_ctx, cancel| {
            let prepare_calls = Arc::clone(&prepare_calls_hook);
            let prepare_started = Arc::clone(&prepare_started_hook);
            let hook_saw_cancel = Arc::clone(&hook_saw_cancel_hook);
            Box::pin(async move {
                prepare_calls.fetch_add(1, Ordering::SeqCst);
                prepare_started.store(true, Ordering::SeqCst);
                cancel.cancelled().await;
                hook_saw_cancel.store(true, Ordering::SeqCst);
                Ok(None)
            })
        }));

        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let provider = Arc::new(provider);
        let task_provider = Arc::clone(&provider);
        let task_tool = Arc::clone(&tool);
        let run = tokio::spawn(async move {
            run_prompt(
                vec![text_user_message("prompt")],
                base_context(vec![task_tool]),
                config,
                &task_provider,
                run_cancel,
            )
            .await
        });

        let mut waited = 0usize;
        while !prepare_started.load(Ordering::SeqCst) {
            waited += 1;
            if waited > 5_000 {
                return Err("prepare never started".to_owned());
            }
            sleep(Duration::from_millis(1)).await;
        }
        cancel.cancel();

        let (_messages, events, _) = run.await.map_err(|error| error.to_string())??;
        assert!(
            hook_saw_cancel.load(Ordering::SeqCst),
            "prepare must receive the active run cancellation token"
        );
        assert_eq!(prepare_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.call_count(), 1);
        assert_eq!(count_type(&events, "agent_start"), 1);
        assert_eq!(count_type(&events, "turn_start"), 1);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_after_turn_skips_steering_and_follow_up() -> TestResult {
        let steer_polls = Arc::new(AtomicUsize::new(0));
        let follow_polls = Arc::new(AtomicUsize::new(0));
        let provider = ScriptedProvider::new(vec![text_script("stop")]);
        let mut config = sample_config();
        let steer_polls_hook = Arc::clone(&steer_polls);
        config.get_steering_messages = Some(Arc::new(move || {
            let steer_polls_hook = Arc::clone(&steer_polls_hook);
            Box::pin(async move {
                steer_polls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        }));
        let follow_polls_hook = Arc::clone(&follow_polls);
        config.get_follow_up_messages = Some(Arc::new(move || {
            let follow_polls_hook = Arc::clone(&follow_polls_hook);
            Box::pin(async move {
                follow_polls_hook.fetch_add(1, Ordering::SeqCst);
                Ok(vec![text_user_message("should-not-run")])
            })
        }));
        config.should_stop_after_turn = Some(Arc::new(|_| Box::pin(async { Ok(true) })));

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(steer_polls.load(Ordering::SeqCst), 1);
        assert_eq!(follow_polls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.call_count(), 1);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn semantic_error_exits_with_single_agent_end() -> TestResult {
        let provider = ScriptedProvider::new(vec![vec![Ok(start("")), Ok(error_event("boom"))]]);
        let (_messages, events, partial_rx) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(count_type(&events, "agent_end"), 1);
        assert_eq!(count_assistant_message_end(&events), 1);
        assert!(partial_rx.borrow().is_none());
        assert!(matches!(
            events.iter().find(|e| matches!(e, AgentEvent::TurnEnd { .. })),
            Some(AgentEvent::TurnEnd { tool_results, .. }) if tool_results.is_empty()
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn early_stream_close_synthesizes_error_final() -> TestResult {
        let provider = ScriptedProvider::new(vec![vec![Ok(start("partial"))]]);
        let (messages, events, partial_rx) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(count_type(&events, "agent_end"), 1);
        assert_eq!(count_assistant_message_end(&events), 1);
        assert!(partial_rx.borrow().is_none());
        let last = messages
            .last()
            .and_then(AgentMessage::as_llm)
            .and_then(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .ok_or("missing assistant")?;
        assert_eq!(last.stop_reason, StopReason::Error);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infra_error_synthesizes_error() -> TestResult {
        let provider = ScriptedProvider::new(vec![vec![
            Ok(start("")),
            Err(ProviderError::new("transport down")),
        ]]);
        let (messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;
        assert_eq!(count_type(&events, "agent_end"), 1);
        let last = messages
            .last()
            .and_then(AgentMessage::as_llm)
            .and_then(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .ok_or("missing assistant")?;
        assert_eq!(last.stop_reason, StopReason::Error);
        assert_eq!(last.error_message.as_deref(), Some("transport down"));
        Ok(())
    }

    #[test]
    fn cancelled_provider_error_synthesizes_aborted() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let assistant = synthesize_from_provider_error(
            &sample_config(),
            &cancel,
            &ProviderError::new("transport down"),
        );

        assert_eq!(assistant.stop_reason, StopReason::Aborted);
        assert_eq!(assistant.error_message.as_deref(), Some("transport down"));
    }

    #[tokio::test]
    async fn ready_cancel_wins_over_ready_provider_terminal() -> TestResult {
        let (sink, events) = CollectSink::new();
        let provider = ScriptedProvider::new(Vec::new());
        let (partial_tx, _) = watch::channel(None);
        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx,
        };
        let mut context = base_context(Vec::new());
        let config = sample_config();
        let cancel = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .try_send(DrainItem::Event(Box::new(done_text("provider-final"))))
            .map_err(|error| error.to_string())?;
        cancel.cancel();

        let final_message = consume_drain_items(&mut event_rx, &mut context, &config, &io, &cancel)
            .await
            .map_err(|error| error.to_string())?;
        let events = snapshot(&events)?;

        assert_eq!(final_message.stop_reason, StopReason::Aborted);
        assert_eq!(count_assistant_message_end(&events), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(message)
            } if matches!(
                message.as_ref(),
                Message::Assistant(assistant)
                    if assistant.stop_reason == StopReason::Aborted
            )
        )));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn truncated_tools_do_not_execute() -> TestResult {
        let tool = Arc::new(RecordingTool::new("read"));
        let executed = Arc::clone(&tool.executed);
        let provider = ScriptedProvider::new(vec![
            vec![
                Ok(start("")),
                Ok(length_tool(vec![ToolCall::new("c1", "read", Map::new())])),
            ],
            text_script("recovered"),
        ]);

        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(vec![tool]),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;

        assert_eq!(executed.load(Ordering::SeqCst), 0);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolExecutionEnd { is_error: true, .. })),
            "expected truncated tool error end"
        );
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_rejects_empty_and_assistant_tail() -> TestResult {
        let provider = ScriptedProvider::new(Vec::new());
        let (sink, _) = CollectSink::new();
        let (partial_tx, _) = watch::channel(None);
        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx.clone(),
        };
        let err = run_agent_loop_continue(
            base_context(Vec::new()),
            sample_config(),
            io,
            CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("empty should error")?;
        assert!(err.to_string().contains("no messages"));

        let (sink, _) = CollectSink::new();
        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx,
        };
        let mut context = base_context(Vec::new());
        context
            .messages
            .push(assistant_agent_message(assistant("tail")));
        let err = run_agent_loop_continue(context, sample_config(), io, CancellationToken::new())
            .await
            .err()
            .ok_or("assistant tail should error")?;
        assert!(err.to_string().contains("assistant"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_from_user_or_tool_result_works() -> TestResult {
        let provider = ScriptedProvider::new(vec![text_script("continued")]);
        let (sink, events) = CollectSink::new();
        let (partial_tx, _) = watch::channel(None);
        let mut context = base_context(Vec::new());
        context.messages.push(text_user_message("again"));
        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx,
        };
        let messages =
            run_agent_loop_continue(context, sample_config(), io, CancellationToken::new())
                .await
                .map_err(|err| err.to_string())?;
        let events = snapshot(&events)?;
        assert_eq!(messages.len(), 1);
        assert_eq!(event_types(&events)[..2], ["agent_start", "turn_start"]);
        assert_eq!(count_type(&events, "message_start"), 1);
        assert_eq!(count_type(&events, "agent_end"), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn custom_messages_preserved_via_convert_hook() -> TestResult {
        let custom = AgentMessage::Custom(CustomAgentMessage::new(
            "branchSummary",
            Map::from_iter([("summary".to_owned(), json!("x"))]),
        ));
        let provider = ScriptedProvider::new(vec![text_script("ok")]);
        let mut config = sample_config();
        config.convert_to_llm = Arc::new(|messages| {
            Box::pin(async move {
                if !messages.iter().any(|m| m.role() == "branchSummary") {
                    return Err(AgentLoopError::message("custom missing in convert_to_llm"));
                }
                Ok(default_convert_to_llm(&messages))
            })
        });

        let mut context = base_context(Vec::new());
        context.messages.push(custom);
        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            context,
            config,
            &provider,
            CancellationToken::new(),
        )
        .await?;
        assert_eq!(count_type(&events, "agent_end"), 1);
        let ctx = provider.last_context().ok_or("no context")?;
        assert!(
            ctx.messages.iter().all(|message| {
                matches!(
                    message,
                    Message::User(_) | Message::Assistant(_) | Message::ToolResult(_)
                )
            }),
            "only LLM messages reach provider"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_plus_provider_terminal_single_message_end_and_agent_end() -> TestResult {
        let provider = ScriptedProvider::hanging(
            vec![vec![
                Ok(start("")),
                Ok(text_delta("x", "x")),
                Ok(done_text("x")),
            ]],
            1,
        );
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let (sink, events) = CollectSink::new();
        let (partial_tx, partial_rx) = watch::channel(None);
        let provider_ref = provider.clone();

        let join = tokio::spawn(async move {
            let io = RunIo {
                sink: &sink,
                provider: &provider_ref,
                partial: partial_tx,
            };
            run_agent_loop(
                vec![text_user_message("prompt")],
                base_context(Vec::new()),
                sample_config(),
                io,
                cancel_run,
            )
            .await
        });

        for _ in 0..100 {
            if provider.delivered.load(Ordering::SeqCst) >= 1 {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(
            provider.delivered.load(Ordering::SeqCst) >= 1,
            "provider did not deliver the event required to establish the cancellation race"
        );
        cancel.cancel();
        join.await
            .map_err(|err| err.to_string())?
            .map_err(|err| err.to_string())?;
        let events = snapshot(&events)?;

        assert_eq!(count_type(&events, "agent_end"), 1, "events={events:?}");
        assert_eq!(count_assistant_message_end(&events), 1, "events={events:?}");
        let final_assistant = events.iter().find_map(|event| match event {
            AgentEvent::MessageEnd { message } => match message.as_llm() {
                Some(Message::Assistant(assistant)) => Some(assistant),
                _ => None,
            },
            _ => None,
        });
        let final_assistant = final_assistant.ok_or("missing assistant terminal")?;
        assert_eq!(final_assistant.stop_reason, StopReason::Aborted);
        assert!(
            final_assistant
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("cancel")),
            "unexpected cancellation terminal: {final_assistant:?}"
        );
        assert!(partial_rx.borrow().is_none());
        Ok(())
    }

    fn note_assistant_final(event: &AgentEvent, saw_final: &mut bool) {
        if let AgentEvent::MessageEnd { message } = event
            && matches!(message.as_llm(), Some(Message::Assistant(_)))
        {
            *saw_final = true;
        }
    }

    fn note_partial_if_present(
        partial_rx: &mut watch::Receiver<Option<Arc<AssistantMessage>>>,
        partial_versions: &mut usize,
    ) {
        if partial_rx.borrow_and_update().is_some() {
            *partial_versions += 1;
        }
    }

    async fn drain_extension_lag(extension: &mut crate::bus::ExtensionSubscription) {
        for _ in 0..16 {
            match extension.try_recv() {
                Ok(ExtensionEvent::Lagged)
                | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return;
                }
                Ok(ExtensionEvent::Event(_)) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hung_extension_cannot_stall_final_or_partial() -> TestResult {
        let state = Arc::new(std::sync::Mutex::new(AgentState::default()));
        let sink = AgentEventSink::new(Arc::clone(&state));
        let mut extension = sink.subscribe_extension_with_capacity(1);
        let mut lossless = sink.subscribe();

        let mut long_stream = vec![Ok(start(""))];
        for i in 0..40 {
            let text = format!("p{i}");
            long_stream.push(Ok(text_delta(&text, &text)));
        }
        long_stream.push(Ok(done_text("final")));
        let provider = ScriptedProvider::paced(vec![long_stream], Duration::from_millis(2));

        let (partial_tx, mut partial_rx) = watch::channel(None);
        partial_rx.borrow_and_update();
        let provider_ref = provider.clone();
        let mut join = tokio::spawn(async move {
            let io = RunIo {
                sink: &sink,
                provider: &provider_ref,
                partial: partial_tx,
            };
            run_agent_loop(
                vec![text_user_message("prompt")],
                base_context(Vec::new()),
                sample_config(),
                io,
                CancellationToken::new(),
            )
            .await
        });

        let mut partial_versions = 0usize;
        let mut saw_final = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                result = &mut join => {
                    result.map_err(|err| err.to_string())?.map_err(|err| err.to_string())?;
                    break;
                }
                changed = partial_rx.changed() => {
                    if changed.is_ok() || partial_rx.has_changed().unwrap_or(false) {
                        note_partial_if_present(&mut partial_rx, &mut partial_versions);
                    }
                    if changed.is_err() {
                        sleep(Duration::from_millis(1)).await;
                    }
                }
                event = lossless.recv() => {
                    if let Some(event) = event.as_ref() {
                        note_assistant_final(event, &mut saw_final);
                    }
                }
                () = sleep(Duration::from_millis(10)) => {
                    if tokio::time::Instant::now() > deadline {
                        return Err("run stalled under hung extension".to_owned());
                    }
                }
            }
        }

        while let Ok(event) = lossless.try_recv() {
            note_assistant_final(&event, &mut saw_final);
        }
        if !saw_final {
            return Err("lossless sink missed assistant final".to_owned());
        }
        if partial_versions < 2 {
            return Err(format!(
                "partial watch did not advance enough: {partial_versions}"
            ));
        }
        drain_extension_lag(&mut extension).await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_start_event_still_emits_message_start_before_end() -> TestResult {
        let provider = ScriptedProvider::new(vec![vec![Ok(done_text("only-done"))]]);
        let (_messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;
        let types = event_types(&events);
        assert!(types.iter().filter(|ty| **ty == "message_start").count() >= 2);
        assert_eq!(count_assistant_message_end(&events), 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_semantic_path() -> TestResult {
        let provider =
            ScriptedProvider::new(vec![vec![Ok(start("")), Ok(aborted_event("aborted"))]]);
        let (messages, events, _) = run_prompt(
            vec![text_user_message("prompt")],
            base_context(Vec::new()),
            sample_config(),
            &provider,
            CancellationToken::new(),
        )
        .await?;
        assert_eq!(count_type(&events, "agent_end"), 1);
        let last = messages
            .last()
            .and_then(AgentMessage::as_llm)
            .and_then(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .ok_or("missing assistant")?;
        assert_eq!(last.stop_reason, StopReason::Aborted);
        Ok(())
    }
}
