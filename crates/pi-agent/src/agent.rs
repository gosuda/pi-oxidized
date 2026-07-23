//! Stateful agent wrapper over the green low-level run loop.
//!
//! Owns shared state, steering/follow-up queues, provider wiring, partial
//! watch, event sink, and idle notification. Exposes one active run at a time
//! with prompt/continue/abort/wait semantics, and guarantees exactly one
//! `agent_end` per run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, Message, Model, ModelThinkingLevel, Provider, StopReason};
use serde_json::Map;
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use crate::bus::{AgentEventSink, AgentEventSubscription, EventSink, ExtensionSubscription};
use crate::config::{AgentContext, AgentLoopConfig, GetMessages};
use crate::error::AgentLoopError;
use crate::event::AgentEvent;
use crate::message::{AgentMessage, now_millis};
use crate::queue::{PendingMessageQueue, QueueMode};
use crate::run::{RunIo, run_agent_loop, run_agent_loop_continue};
use crate::state::{AgentState, AgentStateSnapshot};
use crate::tool::{AgentTool, ToolExecutionMode};

/// Configuration for a new [`Agent`].
#[derive(Clone)]
pub struct AgentOptions {
    /// System prompt used for every provider request.
    pub system_prompt: String,
    /// Active model.
    pub model: Model,
    /// Reasoning level.
    pub thinking_level: pi_ai::ModelThinkingLevel,
    /// Available tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Initial transcript messages.
    pub messages: Vec<AgentMessage>,
    /// Base loop configuration; model and reasoning are refreshed from state.
    pub config: AgentLoopConfig,
    /// Provider used for assistant streams.
    pub provider: Arc<dyn Provider>,
}

impl AgentOptions {
    /// Creates minimal options using `provider` and sensible defaults.
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self {
            system_prompt: String::new(),
            model: crate::state::default_model(),
            thinking_level: pi_ai::ModelThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            config: default_base_config(),
            provider,
        }
    }
}

fn default_base_config() -> AgentLoopConfig {
    AgentLoopConfig {
        model: crate::state::default_model(),
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
        convert_to_llm: crate::config::default_convert_to_llm_hook(),
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

/// Combined runtime state that owns the active run token and streaming flag.
///
/// Holding both under one mutex makes `wait_for_idle` observe a single,
/// atomic idle condition and prevents a run from being marked idle while the
/// active token is still held.
struct RunState {
    /// True while the agent is processing a prompt or continuation.
    is_streaming: bool,
    /// Cancellation token for the currently active run, if any.
    active: Option<CancellationToken>,
    /// Human-readable reason supplied by the caller that aborted this run.
    abort_reason: Option<String>,
}

struct AgentInner {
    state: Arc<Mutex<AgentState>>,
    sink: AgentEventSink,
    steering: Arc<Mutex<PendingMessageQueue>>,
    follow_up: Arc<Mutex<PendingMessageQueue>>,
    provider: Arc<dyn Provider>,
    base_config: AgentLoopConfig,
    partial_tx: watch::Sender<Option<Arc<AssistantMessage>>>,
    idle: Notify,
    run: Arc<Mutex<RunState>>,
}

/// Stateful agent exposing prompt/continue/abort lifecycle.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

impl Agent {
    /// Creates a new agent from options.
    #[must_use]
    pub fn new(options: AgentOptions) -> Self {
        let state = Arc::new(Mutex::new(AgentState::with_initial(
            options.system_prompt,
            options.model,
            options.thinking_level,
            options.tools,
            options.messages,
        )));
        let sink = AgentEventSink::new(Arc::clone(&state));
        let (partial_tx, _) = watch::channel(None);
        let inner = Arc::new(AgentInner {
            state,
            sink,
            steering: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            follow_up: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            provider: options.provider,
            base_config: options.config,
            partial_tx,
            idle: Notify::new(),
            run: Arc::new(Mutex::new(RunState {
                is_streaming: false,
                active: None,
                abort_reason: None,
            })),
        });
        Self { inner }
    }

    /// Returns a bounded event subscription.
    #[must_use]
    pub fn subscribe(&self) -> AgentEventSubscription {
        self.inner.sink.subscribe()
    }

    /// Returns a bounded extension event subscription.
    #[must_use]
    pub fn subscribe_extension(&self) -> ExtensionSubscription {
        self.inner.sink.subscribe_extension()
    }

    /// Returns a receiver for the latest partial assistant message.
    #[must_use]
    pub fn partial(&self) -> watch::Receiver<Option<Arc<AssistantMessage>>> {
        self.inner.partial_tx.subscribe()
    }

    /// Returns an immutable snapshot of the agent state.
    #[must_use]
    pub fn state(&self) -> AgentStateSnapshot {
        let mut snapshot = lock(&self.inner.state).snapshot();
        let run = lock(&self.inner.run);
        snapshot.is_streaming = run.is_streaming;
        snapshot
    }

    /// Replaces the active model for future config snapshots.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. The change is
    /// visible to the next run-start snapshot and does not alter active-run
    /// ownership or queues.
    pub fn set_model(&self, model: Model) {
        lock(&self.inner.state).model = model;
    }

    /// Replaces the thinking level for future config snapshots.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. The change is
    /// visible to the next run-start snapshot and does not alter active-run
    /// ownership or queues.
    pub fn set_thinking_level(&self, level: ModelThinkingLevel) {
        lock(&self.inner.state).thinking_level = level;
    }

    /// Replaces the tool set for future context snapshots.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. The change is
    /// visible to the next run-start snapshot and does not alter active-run
    /// ownership or queues.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        lock(&self.inner.state).tools = tools;
    }

    /// Replaces the system prompt for future context snapshots.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. The change is
    /// visible to the next run-start snapshot and does not alter active-run
    /// ownership or queues.
    pub fn set_system_prompt(&self, prompt: String) {
        lock(&self.inner.state).system_prompt = prompt;
    }

    /// Replaces the full transcript for future context snapshots.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. Used by
    /// compaction, fork, and tree rebuild paths. Does not alter active-run
    /// ownership or queues.
    pub fn replace_messages(&self, messages: Vec<AgentMessage>) {
        lock(&self.inner.state).messages = messages;
    }

    /// Appends one message to the transcript.
    ///
    /// Callers must serialize mutations with the prompt lifecycle. Does not
    /// alter active-run ownership or queues.
    pub fn push_message(&self, message: AgentMessage) {
        lock(&self.inner.state).messages.push(message);
    }

    /// Pops the transcript tail when it is an assistant message.
    ///
    /// Returns `true` when a message was removed. Non-assistant tails are left
    /// unchanged. Does not alter active-run ownership or queues.
    #[must_use]
    pub fn pop_last_if_assistant(&self) -> bool {
        let mut state = lock(&self.inner.state);
        match state.messages.last() {
            Some(message) if message.role() == "assistant" => {
                state.messages.pop();
                true
            }
            _ => false,
        }
    }

    /// Returns a clone of the current transcript.
    #[must_use]
    pub fn transcript(&self) -> Vec<AgentMessage> {
        lock(&self.inner.state).messages.clone()
    }

    /// Returns a clone of the most recent assistant message, if any.
    ///
    /// Only the matching message is cloned; the rest of the transcript is not.
    #[must_use]
    pub fn last_assistant(&self) -> Option<AssistantMessage> {
        let state = lock(&self.inner.state);
        state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message.as_llm() {
                Some(Message::Assistant(assistant)) => Some(assistant.clone()),
                _ => None,
            })
    }

    /// Replaces the transcript tail when it is an assistant message.
    ///
    /// Succeeds only when the last message is an assistant; otherwise returns
    /// `false` without scanning earlier entries. Callers must serialize
    /// mutations with the prompt lifecycle. Does not alter active-run ownership
    /// or queues.
    #[must_use]
    pub fn replace_last_assistant(&self, message: AssistantMessage) -> bool {
        let mut state = lock(&self.inner.state);
        match state.messages.last_mut() {
            Some(tail) if tail.role() == "assistant" => {
                *tail = AgentMessage::Llm(Box::new(Message::Assistant(message)));
                true
            }
            _ => false,
        }
    }

    /// Enqueues a steering message.
    pub fn steer(&self, message: AgentMessage) {
        lock(&self.inner.steering).enqueue(message);
    }

    /// Enqueues a follow-up message.
    pub fn follow_up(&self, message: AgentMessage) {
        lock(&self.inner.follow_up).enqueue(message);
    }

    /// Clears both steering and follow-up queues.
    pub fn clear_queues(&self) {
        lock(&self.inner.steering).clear();
        lock(&self.inner.follow_up).clear();
    }

    /// Returns true when either queue still contains pending messages.
    #[must_use]
    pub fn has_queued_messages(&self) -> bool {
        !lock(&self.inner.steering).is_empty() || !lock(&self.inner.follow_up).is_empty()
    }

    /// Returns the current steering queue drain mode.
    #[must_use]
    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.inner.steering).mode()
    }

    /// Sets the steering queue drain mode.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.inner.steering).set_mode(mode);
    }

    /// Returns the current follow-up queue drain mode.
    #[must_use]
    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.inner.follow_up).mode()
    }

    /// Sets the follow-up queue drain mode.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.inner.follow_up).set_mode(mode);
    }

    /// Starts a new prompt run and awaits its completion.
    ///
    /// # Errors
    ///
    /// Returns an error if another run is already active.
    pub async fn prompt(&self, prompts: Vec<AgentMessage>) -> Result<(), AgentLoopError> {
        let cancel = start_run(&self.inner)?;
        run_lifecycle(
            &self.inner,
            prompts,
            RunMode::Prompt {
                skip_initial_steering: false,
            },
            cancel,
        )
        .await
    }

    /// Continues from the current transcript tail and awaits its completion.
    ///
    /// Mirrors `Agent.prototype.continue` in `agent.ts`: when the tail is an
    /// assistant message, queued steering (then follow-up) messages are run as a
    /// new prompt; otherwise the low-level continuation loop runs from the tail.
    ///
    /// # Errors
    ///
    /// Returns an error if another run is already active, the transcript is
    /// empty, or the tail is an assistant message with nothing queued.
    pub async fn continue_run(&self) -> Result<(), AgentLoopError> {
        let (prompts, mode, cancel) = begin_continue(&self.inner)?;
        run_lifecycle(&self.inner, prompts, mode, cancel).await
    }

    /// Cancels the active run, if any.
    ///
    /// Queues are not cleared by abort.
    pub fn abort(&self) {
        if let Some(token) = lock(&self.inner.run).active.as_ref() {
            token.cancel();
        }
    }

    /// Cancels the active run and surfaces `reason` on its aborted assistant.
    ///
    /// Queues are not cleared by abort.
    pub fn abort_with_reason(&self, reason: impl Into<String>) {
        let mut run = lock(&self.inner.run);
        let Some(token) = run.active.clone() else {
            return;
        };
        run.abort_reason = Some(reason.into());
        token.cancel();
    }

    /// Waits until no run is active.
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            {
                let run = lock(&self.inner.run);
                if !run.is_streaming && run.active.is_none() {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Aborts any active run, waits for it to finish, and clears transcript and
    /// queues.
    pub async fn reset(&self) {
        self.abort();
        self.wait_for_idle().await;
        lock(&self.inner.state).reset_transcript();
        lock(&self.inner.steering).clear();
        lock(&self.inner.follow_up).clear();
        let _ = self.inner.partial_tx.send(None);
        let mut run = lock(&self.inner.run);
        run.is_streaming = false;
        run.active = None;
        run.abort_reason = None;
    }
}

enum RunMode {
    Prompt { skip_initial_steering: bool },
    Continue,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

fn start_run(inner: &AgentInner) -> Result<CancellationToken, AgentLoopError> {
    let cancel = CancellationToken::new();
    {
        let mut run = lock(&inner.run);
        if run.active.is_some() {
            return Err(AgentLoopError::message("agent is already running"));
        }
        run.is_streaming = true;
        run.active = Some(cancel.clone());
        run.abort_reason = None;
    }
    {
        let mut state = lock(&inner.state);
        state.is_streaming = true;
        state.streaming_message = None;
        state.error_message = None;
    }
    Ok(cancel)
}

/// Acquires the active run token and plans a continuation atomically.
///
/// Queue draining happens only after the active token is secured, so a
/// concurrent `prompt` cannot win the token after we have drained and silently
/// lose the queued messages. The active mutex is held across validation and
/// draining; no other path takes a queue/state lock before the active lock, so
/// this ordering cannot deadlock.
fn begin_continue(
    inner: &AgentInner,
) -> Result<(Vec<AgentMessage>, RunMode, CancellationToken), AgentLoopError> {
    let cancel = CancellationToken::new();
    let prompts;
    let mode;
    {
        let mut run = lock(&inner.run);
        if run.active.is_some() {
            return Err(AgentLoopError::message("agent is already running"));
        }

        let tail_is_assistant = {
            let state = lock(&inner.state);
            if state.messages.is_empty() {
                return Err(AgentLoopError::message("No messages to continue from"));
            }
            state
                .messages
                .last()
                .is_some_and(|message| message.role() == "assistant")
        };

        let plan = plan_assistant_tail_continue(inner, tail_is_assistant)?;
        prompts = plan.0;
        mode = plan.1;

        run.is_streaming = true;
        run.active = Some(cancel.clone());
        run.abort_reason = None;
        {
            let mut state = lock(&inner.state);
            state.is_streaming = true;
            state.streaming_message = None;
            state.error_message = None;
        }
    }
    Ok((prompts, mode, cancel))
}

/// Picks the prompts and run mode for a continuation, draining queued
/// steering/follow-up messages only when the transcript tail is an assistant.
fn plan_assistant_tail_continue(
    inner: &AgentInner,
    tail_is_assistant: bool,
) -> Result<(Vec<AgentMessage>, RunMode), AgentLoopError> {
    if tail_is_assistant {
        let steering = lock(&inner.steering).drain();
        if steering.is_empty() {
            let follow_ups = lock(&inner.follow_up).drain();
            return if follow_ups.is_empty() {
                Err(AgentLoopError::message(
                    "Cannot continue from message role: assistant",
                ))
            } else {
                Ok((
                    follow_ups,
                    RunMode::Prompt {
                        skip_initial_steering: false,
                    },
                ))
            };
        }
        return Ok((
            steering,
            RunMode::Prompt {
                skip_initial_steering: true,
            },
        ));
    }
    Ok((Vec::new(), RunMode::Continue))
}

fn snapshot_context(inner: &AgentInner) -> AgentContext {
    let state = lock(&inner.state);
    AgentContext {
        system_prompt: state.system_prompt.clone(),
        messages: state.messages.clone(),
        tools: state.tools.clone(),
    }
}

fn snapshot_config(inner: &AgentInner) -> AgentLoopConfig {
    let state = lock(&inner.state);
    let mut config = inner.base_config.clone();
    config.model = state.model.clone();
    config.reasoning = if state.thinking_level == ModelThinkingLevel::Off {
        None
    } else {
        Some(state.thinking_level)
    };
    config
}

fn make_steering_hook(queue: Arc<Mutex<PendingMessageQueue>>, skip_initial: bool) -> GetMessages {
    let skipped = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if skip_initial && !skipped.swap(true, Ordering::SeqCst) {
            return Box::pin(async move { Ok(Vec::new()) });
        }
        let messages = lock(&queue).drain();
        Box::pin(async move { Ok(messages) })
    })
}

fn make_follow_up_hook(queue: Arc<Mutex<PendingMessageQueue>>) -> GetMessages {
    Arc::new(move || {
        let messages = lock(&queue).drain();
        Box::pin(async move { Ok(messages) })
    })
}

struct TrackingSink<'a> {
    inner: &'a dyn EventSink,
    terminal: Arc<AtomicBool>,
    new_messages: Arc<Mutex<Vec<AgentMessage>>>,
    run: Arc<Mutex<RunState>>,
}

impl EventSink for TrackingSink<'_> {
    fn emit(&self, mut event: AgentEvent) {
        // Substitute a caller-supplied abort reason onto every aborted terminal
        // surface that carries an assistant message. MessageEnd alone is not
        // enough: TurnEnd (and therefore AgentStateSnapshot.error_message via
        // reduce) still sees the synthesized "stream cancelled" text unless we
        // rewrite it here too.
        match &mut event {
            AgentEvent::MessageEnd { message } | AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Llm(inner) = message
                    && let Message::Assistant(assistant) = inner.as_mut()
                    && assistant.stop_reason == StopReason::Aborted
                    && let Some(reason) = lock(&self.run).abort_reason.clone()
                {
                    assistant.error_message = Some(reason);
                }
            }
            _ => {}
        }
        if let AgentEvent::MessageEnd { message } = &event {
            if message.role() == "assistant" {
                self.terminal.store(true, Ordering::SeqCst);
            }
            lock(&self.new_messages).push(message.clone());
        }
        self.inner.emit(event);
    }
}

async fn run_lifecycle(
    inner: &Arc<AgentInner>,
    prompts: Vec<AgentMessage>,
    mode: RunMode,
    cancel: CancellationToken,
) -> Result<(), AgentLoopError> {
    let context = snapshot_context(inner);
    let mut config = snapshot_config(inner);
    let skip_initial = match mode {
        RunMode::Prompt {
            skip_initial_steering,
        } => skip_initial_steering,
        RunMode::Continue => false,
    };
    config.get_steering_messages = Some(make_steering_hook(
        Arc::clone(&inner.steering),
        skip_initial,
    ));
    config.get_follow_up_messages = Some(make_follow_up_hook(Arc::clone(&inner.follow_up)));

    let terminal = Arc::new(AtomicBool::new(false));
    let new_messages = Arc::new(Mutex::new(Vec::new()));
    let tracking = TrackingSink {
        inner: &inner.sink,
        terminal: Arc::clone(&terminal),
        new_messages: Arc::clone(&new_messages),
        run: Arc::clone(&inner.run),
    };

    let _ = inner.partial_tx.send(None);
    let io = RunIo {
        sink: &tracking,
        provider: inner.provider.as_ref(),
        partial: inner.partial_tx.clone(),
    };

    let result = match mode {
        RunMode::Prompt { .. } => {
            run_agent_loop(prompts, context, config, io, cancel.clone()).await
        }
        RunMode::Continue => run_agent_loop_continue(context, config, io, cancel.clone()).await,
    };
    finish_run(inner, &terminal, &new_messages, result, &cancel)
}

fn finish_run(
    inner: &Arc<AgentInner>,
    terminal: &AtomicBool,
    new_messages: &Mutex<Vec<AgentMessage>>,
    result: Result<Vec<AgentMessage>, AgentLoopError>,
    cancel: &CancellationToken,
) -> Result<(), AgentLoopError> {
    let _ = inner.partial_tx.send(None);

    let outcome = match result {
        Ok(_) => Ok(()),
        Err(error) => {
            // Make the error observable on every failure. Set it before
            // `finish_run()` (which clears streaming/pending but preserves
            // `error_message`), so callers and snapshot readers see it.
            {
                let mut state = lock(&inner.state);
                state.error_message = Some(error.to_string());
            }
            let mut produced = lock(new_messages).clone();
            // Only synthesize an assistant terminal when the loop never emitted
            // one. If `terminal` is already set, the loop produced the single
            // allowed assistant message end + turn end, so we emit exactly one
            // `agent_end` and no duplicate message/turn events.
            if !terminal.load(Ordering::SeqCst) {
                let assistant = synthesize_error_assistant(&snapshot_config(inner), cancel, &error);
                let message = AgentMessage::Llm(Box::new(Message::Assistant(assistant)));
                emit_message_pair(&inner.sink, message.clone());
                inner.sink.emit(AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: Vec::new(),
                });
                produced.push(message);
            }
            inner.sink.emit(AgentEvent::AgentEnd { messages: produced });
            Err(error)
        }
    };

    {
        let mut state = lock(&inner.state);
        state.finish_run();
    }
    {
        let mut run = lock(&inner.run);
        run.is_streaming = false;
        run.active = None;
        run.abort_reason = None;
    }
    inner.idle.notify_waiters();
    outcome
}

fn emit_message_pair(sink: &dyn EventSink, message: AgentMessage) {
    sink.emit(AgentEvent::MessageStart {
        message: message.clone(),
    });
    sink.emit(AgentEvent::MessageEnd { message });
}

fn synthesize_error_assistant(
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    error: &AgentLoopError,
) -> AssistantMessage {
    let mut message = AssistantMessage::new(
        config.model.api.clone(),
        config.model.provider.clone(),
        config.model.id.clone(),
        now_millis(),
    );
    message.stop_reason = if cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    message.error_message = Some(error.to_string());
    message
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::message::user_text;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantContent, AssistantMessageEvent, Context, DoneReason, Message, Model, ModelCost,
        ModelInput, Provider, ProviderError, StopReason, StreamOptions, TextContent,
        UserMessageContent,
    };
    use tokio::time::{sleep, timeout};

    use super::*;

    fn test_model() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn base_test_config() -> AgentLoopConfig {
        let mut config = default_base_config();
        config.model = test_model();
        config
    }

    fn agent_options(provider: Arc<dyn Provider>) -> AgentOptions {
        AgentOptions {
            system_prompt: "sys".to_owned(),
            model: test_model(),
            thinking_level: pi_ai::ModelThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            config: base_test_config(),
            provider,
        }
    }

    fn assistant(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::new("test-api", "test-provider", "m", now_millis());
        message
            .content
            .push(AssistantContent::Text(TextContent::new(text)));
        message
    }

    fn start_event() -> AssistantMessageEvent {
        AssistantMessageEvent::Start {
            partial: AssistantMessage::new("test-api", "test-provider", "m", now_millis()),
        }
    }

    fn text_delta_event(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text.to_owned(),
            partial: assistant(text),
        }
    }

    fn text_end_event(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text.to_owned(),
            partial: assistant(text),
        }
    }

    fn done_event(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Stop;
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        }
    }

    #[derive(Clone)]
    struct MockProvider(Vec<Result<AssistantMessageEvent, ProviderError>>);

    impl Provider for MockProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            stream::iter(self.0.clone()).boxed()
        }
    }

    #[derive(Clone, Default)]
    struct HangingProvider {
        prefix: Vec<Result<AssistantMessageEvent, ProviderError>>,
    }

    impl Provider for HangingProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            let prefix = self.prefix.clone();
            stream::iter(prefix).chain(stream::pending()).boxed()
        }
    }

    impl HangingProvider {
        fn after_start() -> Self {
            Self {
                prefix: vec![Ok(start_event())],
            }
        }
    }

    async fn drain_events(rx: &mut AgentEventSubscription) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(Some(event)) = timeout(Duration::from_millis(50), rx.recv()).await {
            events.push(event);
        }
        events
    }

    fn count_agent_end(events: &[AgentEvent]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
            .count()
    }

    fn user_text_of(message: &AgentMessage) -> Option<&str> {
        match message {
            AgentMessage::Llm(inner) => match inner.as_ref() {
                Message::User(user) => match &user.content {
                    UserMessageContent::Text(text) => Some(text.as_str()),
                    UserMessageContent::Blocks(_) => None,
                },
                _ => None,
            },
            AgentMessage::Custom(_) => None,
        }
    }

    fn assistant_error_message(message: &AgentMessage) -> Option<&str> {
        match message.as_llm() {
            Some(Message::Assistant(assistant)) => assistant.error_message.as_deref(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn prompt_produces_lifecycle_events_and_one_agent_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(text_delta_event("hello")),
            Ok(text_end_event("hello")),
            Ok(done_event("hello")),
        ]));
        let agent = Agent::new(agent_options(provider));
        let mut rx = agent.subscribe();

        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        assert_eq!(count_agent_end(&events), 1);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::MessageEnd { message } if message.role() == "assistant"
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnEnd { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn exclusivity_rejects_second_run() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));

        let _first = tokio::spawn({
            let agent = agent.clone();
            async move {
                let _ = agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await;
            }
        });

        // Wait until the spawned run has acquired the active token.
        let mut waited = 0;
        while !agent.state().is_streaming && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(agent.state().is_streaming, "first run never started");

        assert!(
            agent
                .prompt(vec![user_text("again", std::iter::empty())])
                .await
                .is_err()
        );
        assert!(agent.continue_run().await.is_err());

        agent.abort();
        agent.wait_for_idle().await;
        Ok(())
    }

    #[tokio::test]
    async fn continue_run_skips_initial_steering_poll() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("continue")),
        ]));
        let agent = Agent::new(agent_options(provider));

        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        agent.steer(user_text("steer", std::iter::empty()));
        let mut rx = agent.subscribe();

        agent.continue_run().await?;
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        let user_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "user"
                )
            })
            .count();
        assert_eq!(user_ends, 1, "queued steering is injected as the prompt");
        let assistant_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "assistant"
                )
            })
            .count();
        assert_eq!(assistant_ends, 1);
        assert_eq!(count_agent_end(&events), 1);
        Ok(())
    }

    #[tokio::test]
    async fn steering_one_at_a_time_drains_oldest_only() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("ok")),
            Ok(start_event()),
            Ok(done_event("ok2")),
        ]));
        let agent = Agent::new(agent_options(provider));
        agent.set_steering_mode(QueueMode::OneAtATime);
        agent.steer(user_text("first", std::iter::empty()));
        agent.steer(user_text("second", std::iter::empty()));

        let mut rx = agent.subscribe();
        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        let user_texts: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageEnd { message } if message.role() == "user" => {
                    user_text_of(message).map(str::to_owned)
                }
                _ => None,
            })
            .collect();
        // One message is drained per steering poll, and the loop polls after
        // every turn, so both queued messages are consumed within this run.
        assert_eq!(
            user_texts,
            vec!["hi".to_owned(), "first".to_owned(), "second".to_owned()],
            "prompt + both steering messages in source order"
        );
        assert!(!agent.has_queued_messages(), "steering queue is drained");
        Ok(())
    }

    #[tokio::test]
    async fn follow_up_queue_triggers_second_turn() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("first")),
            Ok(start_event()),
            Ok(done_event("second")),
        ]));
        let agent = Agent::new(agent_options(provider));
        agent.follow_up(user_text("follow", std::iter::empty()));

        let mut rx = agent.subscribe();
        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        let assistant_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "assistant"
                )
            })
            .count();
        assert_eq!(assistant_ends, 2);
        let turn_starts = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnStart))
            .count();
        assert_eq!(turn_starts, 2);
        assert_eq!(count_agent_end(&events), 1);
        Ok(())
    }

    #[tokio::test]
    async fn abort_emits_single_aborted_terminal() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));
        let mut rx = agent.subscribe();

        let run = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await
            }
        });

        let mut waited = 0;
        while !agent.state().is_streaming && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(agent.state().is_streaming, "run never started");
        agent.abort();
        let prompt_result = run.await?;
        assert!(
            prompt_result.is_ok(),
            "normal abort must resolve the prompt successfully: {prompt_result:?}"
        );
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        assert_eq!(count_agent_end(&events), 1);

        let final_assistant = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageEnd { message } if message.role() == "assistant" => {
                    Some(message)
                }
                _ => None,
            })
            .next_back();
        let aborted = match final_assistant {
            Some(AgentMessage::Llm(message))
                if matches!(message.as_ref(), Message::Assistant(_)) =>
            {
                if let Message::Assistant(assistant) = message.as_ref() {
                    assistant.stop_reason == StopReason::Aborted
                } else {
                    false
                }
            }
            _ => false,
        };
        assert!(aborted, "expected an aborted assistant terminal");
        Ok(())
    }

    #[tokio::test]
    async fn abort_with_reason_surfaces_reason_on_message_turn_and_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));
        let mut rx = agent.subscribe();

        let run = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await
            }
        });

        let mut waited = 0;
        while agent.state().streaming_message.is_none() && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(
            agent.state().streaming_message.is_some(),
            "run never reached mid-turn streaming"
        );

        let reason = "extension cancelled by user";
        agent.abort_with_reason(reason);
        let prompt_result = run.await?;
        assert!(
            prompt_result.is_ok(),
            "abort_with_reason must resolve the prompt successfully: {prompt_result:?}"
        );
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        let message_end_error = events.iter().rev().find_map(|event| match event {
            AgentEvent::MessageEnd { message } if message.role() == "assistant" => {
                assistant_error_message(message)
            }
            _ => None,
        });
        let turn_end_error = events.iter().rev().find_map(|event| match event {
            AgentEvent::TurnEnd { message, .. } => assistant_error_message(message),
            _ => None,
        });

        assert_eq!(message_end_error, Some(reason));
        assert_eq!(turn_end_error, Some(reason));
        assert_eq!(agent.state().error_message.as_deref(), Some(reason));
        Ok(())
    }

    #[tokio::test]
    async fn abort_keeps_generic_stream_cancelled_on_message_turn_and_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));
        let mut rx = agent.subscribe();

        let run = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await
            }
        });

        let mut waited = 0;
        while agent.state().streaming_message.is_none() && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(
            agent.state().streaming_message.is_some(),
            "run never reached mid-turn streaming"
        );

        agent.abort();
        let prompt_result = run.await?;
        assert!(
            prompt_result.is_ok(),
            "plain abort must resolve the prompt successfully: {prompt_result:?}"
        );
        agent.wait_for_idle().await;

        let events = drain_events(&mut rx).await;
        let expected = "stream cancelled";
        let message_end_error = events.iter().rev().find_map(|event| match event {
            AgentEvent::MessageEnd { message } if message.role() == "assistant" => {
                assistant_error_message(message)
            }
            _ => None,
        });
        let turn_end_error = events.iter().rev().find_map(|event| match event {
            AgentEvent::TurnEnd { message, .. } => assistant_error_message(message),
            _ => None,
        });

        assert_eq!(message_end_error, Some(expected));
        assert_eq!(turn_end_error, Some(expected));
        assert_eq!(agent.state().error_message.as_deref(), Some(expected));
        Ok(())
    }

    #[tokio::test]
    async fn abort_preserves_steering_and_follow_up_queues()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));
        let run = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await
            }
        });

        let mut waited = 0;
        while agent.state().streaming_message.is_none() && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(
            agent.state().streaming_message.is_some(),
            "provider start was never reduced"
        );

        agent.steer(user_text("steer-after-abort", std::iter::empty()));
        agent.follow_up(user_text("follow-after-abort", std::iter::empty()));
        agent.abort();
        let prompt_result = run.await?;
        assert!(prompt_result.is_ok(), "abort returned {prompt_result:?}");
        agent.wait_for_idle().await;

        let steering = lock(&agent.inner.steering).drain();
        let follow_up = lock(&agent.inner.follow_up).drain();
        assert_eq!(steering.len(), 1, "abort cleared the steering queue");
        assert_eq!(follow_up.len(), 1, "abort cleared the follow-up queue");
        assert_eq!(user_text_of(&steering[0]), Some("steer-after-abort"));
        assert_eq!(user_text_of(&follow_up[0]), Some("follow-after-abort"));
        Ok(())
    }

    #[tokio::test]
    async fn reset_clears_transcript_and_queues() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));
        agent.steer(user_text("steer", std::iter::empty()));
        agent.follow_up(user_text("follow", std::iter::empty()));

        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        agent.reset().await;
        let state = agent.state();
        assert!(state.messages.is_empty());
        assert!(!state.is_streaming);

        // A fresh prompt should not inject any queued messages.
        let mut rx = agent.subscribe();
        agent
            .prompt(vec![user_text("again", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;
        let events = drain_events(&mut rx).await;
        let user_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "user"
                )
            })
            .count();
        assert_eq!(user_ends, 1);
        Ok(())
    }

    #[tokio::test]
    async fn subscriber_does_not_block_run() -> Result<(), Box<dyn std::error::Error>> {
        let items: Vec<_> = (0..200)
            .map(|index| Ok(text_delta_event(&format!("chunk-{index}"))))
            .chain(std::iter::once(Ok(done_event("end"))))
            .collect();
        let provider = Arc::new(MockProvider(items));
        let agent = Agent::new(agent_options(provider));
        let _rx = agent.subscribe();

        agent
            .prompt(vec![user_text("go", std::iter::empty())])
            .await?;
        timeout(Duration::from_secs(2), agent.wait_for_idle())
            .await
            .map_err(|_| "run did not finish in time")?;
        Ok(())
    }

    #[tokio::test]
    async fn no_duplicate_final_when_loop_errors_before_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(Vec::new()));
        let mut options = agent_options(provider);
        options.config.convert_to_llm = Arc::new(|_messages| {
            Box::pin(async { Err(AgentLoopError::message("convert failed")) })
        });
        let agent = Agent::new(options);
        let mut rx = agent.subscribe();

        let result = agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await;
        agent.wait_for_idle().await;
        assert!(result.is_err(), "loop failure must propagate to the caller");
        assert_eq!(
            agent.state().error_message.as_deref(),
            Some("convert failed"),
            "error must be observable on state"
        );

        let events = drain_events(&mut rx).await;
        assert_eq!(count_agent_end(&events), 1);
        let assistant_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "assistant"
                )
            })
            .count();
        assert_eq!(assistant_ends, 1);
        Ok(())
    }

    #[tokio::test]
    async fn no_duplicate_final_when_loop_errors_after_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let mut options = agent_options(provider);
        options.config.prepare_next_turn = Some(Arc::new(|_ctx| {
            Box::pin(async { Err(AgentLoopError::message("turn hook failed")) })
        }));
        let agent = Agent::new(options);
        let mut rx = agent.subscribe();

        let result = agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await;
        agent.wait_for_idle().await;
        assert!(result.is_err(), "post-terminal hook failure must propagate");
        assert_eq!(
            agent.state().error_message.as_deref(),
            Some("turn hook failed"),
            "post-terminal error must be observable without a duplicate terminal"
        );

        let events = drain_events(&mut rx).await;
        assert_eq!(count_agent_end(&events), 1);
        let assistant_ends = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::MessageEnd { message } if message.role() == "assistant"
                )
            })
            .count();
        assert_eq!(assistant_ends, 1);
        Ok(())
    }

    #[tokio::test]
    async fn new_run_clears_prior_error_message() -> Result<(), Box<dyn std::error::Error>> {
        // convert_to_llm fails on the first run, then succeeds so the second
        let first_failed = Arc::new(AtomicBool::new(false));
        let mut options = agent_options(Arc::new(HangingProvider::after_start()));
        options.config.convert_to_llm = Arc::new(move |_messages| {
            let first_failed = Arc::clone(&first_failed);
            Box::pin(async move {
                if !first_failed.load(Ordering::SeqCst) {
                    first_failed.store(true, Ordering::SeqCst);
                    return Err(AgentLoopError::message("convert failed"));
                }
                Ok(Vec::new())
            })
        });
        let agent = Agent::new(options);
        let mut rx = agent.subscribe();

        // First run fails before any terminal assistant message.
        let first = agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await;
        agent.wait_for_idle().await;
        assert!(first.is_err(), "first run must fail and propagate");
        let _ = drain_events(&mut rx).await;
        assert_eq!(
            agent.state().error_message.as_deref(),
            Some("convert failed")
        );

        // Second run clears the prior error and stays active on the hanging
        // provider, so we can observe the cleared state mid-run.
        let _second = tokio::spawn({
            let agent = agent.clone();
            async move {
                let _ = agent
                    .prompt(vec![user_text("go", std::iter::empty())])
                    .await;
            }
        });
        let mut waited = 0;
        while !agent.state().is_streaming && waited < 200 {
            sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert!(agent.state().is_streaming, "second run never started");
        assert_eq!(
            agent.state().error_message,
            None,
            "prior error cleared when the second run starts"
        );

        agent.abort();
        agent.wait_for_idle().await;
        Ok(())
    }
    #[tokio::test]
    async fn concurrent_prompt_and_continue_never_lose_queued_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        // Seed an assistant tail so `continue_run` has a valid continuation
        // point and a steering message to drain.
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));
        agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        agent.steer(user_text("raced", std::iter::empty()));
        let mut rx = agent.subscribe();

        // Race a prompt against a continuation. Only one may win the active
        // token, but the loser must not have drained the steering message.
        let prompt_task = tokio::spawn({
            let agent = agent.clone();
            async move { agent.prompt(vec![user_text("p", std::iter::empty())]).await }
        });
        let continue_task = tokio::spawn({
            let agent = agent.clone();
            async move { agent.continue_run().await }
        });
        let _ = prompt_task.await;
        let _ = continue_task.await;
        agent.wait_for_idle().await;

        // If the prompt won, the steering message is still queued; flush it with
        // another continuation. If continue won, it is already consumed. Either
        // way the message is never lost.
        if agent.has_queued_messages() {
            agent.continue_run().await?;
            agent.wait_for_idle().await;
        }

        let events = drain_events(&mut rx).await;
        let raced_count = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageEnd { message } if message.role() == "user" => {
                    user_text_of(message).filter(|text| *text == "raced")
                }
                _ => None,
            })
            .count();
        assert_eq!(raced_count, 1, "queued steering message is never lost");
        assert!(!agent.has_queued_messages(), "queue drained at end");
        Ok(())
    }

    #[tokio::test]
    async fn wait_for_idle_returns_when_already_idle() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));
        timeout(Duration::from_millis(50), agent.wait_for_idle())
            .await
            .map_err(|_| "wait_for_idle did not return when idle")?;
        Ok(())
    }

    #[tokio::test]
    async fn prompt_accepted_immediately_after_idle() -> Result<(), Box<dyn std::error::Error>> {
        // Once wait_for_idle returns the active token must be released, so an
        // immediate prompt is accepted rather than rejected as "already running".
        let provider = Arc::new(MockProvider(vec![Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));

        agent
            .prompt(vec![user_text("first", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;

        // No yield/sleep: the second prompt must win the token at once.
        agent
            .prompt(vec![user_text("second", std::iter::empty())])
            .await?;
        agent.wait_for_idle().await;
        Ok(())
    }

    #[tokio::test]
    async fn start_run_marks_streaming_atomically() -> Result<(), Box<dyn std::error::Error>> {
        // After start_run returns there must be no window where the active token
        // is held but the snapshot reports non-streaming.
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));

        let cancel = start_run(&agent.inner)?;
        assert!(
            agent.state().is_streaming,
            "streaming must be visible immediately after the active token is acquired"
        );
        drop(cancel);
        Ok(())
    }

    struct NamedTool {
        name: String,
    }

    impl AgentTool for NamedTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn label(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &'static str {
            "named"
        }

        fn parameters(&self) -> &serde_json::Value {
            static PARAMS: std::sync::LazyLock<serde_json::Value> =
                std::sync::LazyLock::new(|| serde_json::json!({ "type": "object" }));
            &PARAMS
        }

        fn validate_arguments(
            &self,
            args: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Map<String, serde_json::Value>, crate::error::ToolError> {
            Ok(args.clone())
        }

        fn execute(
            &self,
            _tool_call_id: &str,
            _args: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _updates: crate::tool::ToolUpdates,
        ) -> futures::future::BoxFuture<
            'static,
            Result<crate::tool::AgentToolResult, crate::error::ToolError>,
        > {
            Box::pin(async { Ok(crate::tool::AgentToolResult::default()) })
        }
    }

    fn assistant_agent(text: &str) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::Assistant(assistant(text))))
    }

    #[test]
    fn state_mutators_update_snapshots_and_refuse_non_assistant_tail()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));

        let mut next_model = test_model();
        next_model.id = "next".to_owned();
        next_model.name = "next".to_owned();
        agent.set_model(next_model.clone());
        agent.set_thinking_level(ModelThinkingLevel::High);
        agent.set_system_prompt("updated-sys".to_owned());
        let tool: Arc<dyn AgentTool> = Arc::new(NamedTool {
            name: "t1".to_owned(),
        });
        agent.set_tools(vec![Arc::clone(&tool)]);

        let snapshot = agent.state();
        assert_eq!(snapshot.model.id, "next");
        assert_eq!(snapshot.thinking_level, ModelThinkingLevel::High);
        assert_eq!(snapshot.system_prompt, "updated-sys");
        assert_eq!(snapshot.tools.len(), 1);
        assert_eq!(snapshot.tools[0].name(), "t1");

        let config = snapshot_config(&agent.inner);
        assert_eq!(config.model.id, "next");
        assert_eq!(config.reasoning, Some(ModelThinkingLevel::High));

        let user = user_text("u", std::iter::empty());
        let first = assistant_agent("a1");
        let second = assistant_agent("a2");
        agent.replace_messages(vec![user.clone(), first.clone(), second.clone()]);
        assert_eq!(agent.transcript().len(), 3);

        let last = agent
            .last_assistant()
            .ok_or_else(|| std::io::Error::other("tail assistant should be visible"))?;
        assert_eq!(
            last.content[0],
            AssistantContent::Text(TextContent::new("a2"))
        );

        let replacement = assistant("replaced");
        assert!(agent.replace_last_assistant(replacement.clone()));
        let replaced_last = agent
            .last_assistant()
            .ok_or_else(|| std::io::Error::other("replaced tail assistant"))?;
        assert_eq!(
            replaced_last.content[0],
            AssistantContent::Text(TextContent::new("replaced"))
        );
        assert_eq!(agent.transcript().len(), 3);

        assert!(agent.pop_last_if_assistant());
        assert_eq!(agent.transcript().len(), 2);
        // Tail is now the earlier assistant, not a user.
        assert!(agent.pop_last_if_assistant());
        assert_eq!(agent.transcript().len(), 1);
        assert!(!agent.pop_last_if_assistant());
        assert!(!agent.replace_last_assistant(assistant("nope")));
        assert_eq!(agent.transcript().len(), 1);
        assert_eq!(agent.transcript()[0].role(), "user");

        // Assistant buried under a later user is still discoverable, but
        // replacement remains strictly tail-only.
        agent.push_message(assistant_agent("buried"));
        agent.push_message(user_text("later", std::iter::empty()));
        assert!(agent.last_assistant().is_some());
        assert!(!agent.replace_last_assistant(assistant("still-no")));
        assert_eq!(
            agent.transcript().last().map(AgentMessage::role),
            Some("user")
        );

        let context = snapshot_context(&agent.inner);
        assert_eq!(context.system_prompt, "updated-sys");
        assert_eq!(context.tools.len(), 1);
        assert_eq!(context.messages.len(), 3);
        Ok(())
    }

    #[test]
    fn concurrent_readers_never_see_partial_mutation() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(done_event("ok"))]));
        let agent = Agent::new(agent_options(provider));
        agent.replace_messages(vec![
            user_text("u", std::iter::empty()),
            assistant_agent("a"),
        ]);

        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let agent = agent.clone();
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let transcript = agent.transcript();
                    // Only complete lengths produced by the mutator below.
                    assert!(
                        transcript.len() == 2 || transcript.len() == 3,
                        "reader observed partial length {}",
                        transcript.len()
                    );
                    if transcript.len() == 3 {
                        assert_eq!(transcript[2].role(), "assistant");
                    }
                    let snapshot = agent.state();
                    assert!(
                        snapshot.messages.len() == 2 || snapshot.messages.len() == 3,
                        "snapshot observed partial length {}",
                        snapshot.messages.len()
                    );
                }
            }));
        }

        for _ in 0..200 {
            agent.push_message(assistant_agent("x"));
            assert_eq!(agent.transcript().len(), 3);
            assert!(agent.pop_last_if_assistant());
            assert_eq!(agent.transcript().len(), 2);
        }

        stop.store(true, Ordering::Release);
        for handle in handles {
            if handle.join().is_err() {
                return Err(
                    std::io::Error::other("reader thread should finish without panic").into(),
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn state_mutators_leave_active_run_ownership_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(HangingProvider::after_start());
        let agent = Agent::new(agent_options(provider));

        let run = {
            let agent = agent.clone();
            tokio::spawn(async move {
                agent
                    .prompt(vec![user_text("hang", std::iter::empty())])
                    .await
            })
        };

        // Wait until the active token is held.
        timeout(Duration::from_secs(1), async {
            loop {
                if agent.state().is_streaming {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| "run never became streaming")?;

        let mut next_model = test_model();
        next_model.id = "during-run".to_owned();
        agent.set_model(next_model);
        agent.set_thinking_level(ModelThinkingLevel::Low);
        agent.set_system_prompt("during-run".to_owned());
        agent.set_tools(Vec::new());
        agent.push_message(user_text("idle-path", std::iter::empty()));

        // Active ownership is unchanged: a concurrent prompt is still rejected.
        let conflict = agent
            .prompt(vec![user_text("conflict", std::iter::empty())])
            .await;
        assert!(
            matches!(
                &conflict,
                Err(AgentLoopError::Message(message))
                    if message == "agent is already running"
            ),
            "expected already-running rejection, got {conflict:?}"
        );

        assert!(agent.state().is_streaming);
        assert_eq!(agent.state().model.id, "during-run");
        assert_eq!(agent.state().system_prompt, "during-run");

        agent.abort();
        let _ = run.await;
        agent.wait_for_idle().await;
        Ok(())
    }
}
