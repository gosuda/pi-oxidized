//! Agent context and low-level loop configuration.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::provider::{OnPayloadFn, OnResponseFn};
use pi_ai::{
    AssistantMessage, CacheRetention, ImageContent, Model, ModelThinkingLevel, StreamOptions,
    TextContent, ToolCall, ToolResultMessage, Transport,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::error::{AgentLoopError, ToolError};
use crate::message::AgentMessage;
use crate::tool::{AgentTool, AgentToolResult, ToolExecutionMode};

/// Snapshot of messages, tools, and system prompt for one loop invocation.
#[derive(Clone)]
pub struct AgentContext {
    /// System prompt included with the request.
    pub system_prompt: String,
    /// Transcript visible to the model and hooks.
    pub messages: Vec<AgentMessage>,
    /// Tools available for this run.
    pub tools: Vec<Arc<dyn AgentTool>>,
}

/// Result returned from `before_tool_call`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BeforeToolCallResult {
    /// When true, the tool is not executed.
    pub block: bool,
    /// Error text used when blocking; a default is used when omitted.
    pub reason: Option<String>,
}

/// Partial override returned from `after_tool_call`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AfterToolCallResult {
    /// Replaces the full content array when present.
    pub content: Option<Vec<pi_ai::ToolResultContent>>,
    /// Replaces the full details payload when present.
    pub details: Option<Value>,
    /// Replaces the error flag when present.
    pub is_error: Option<bool>,
    /// Replaces the early-termination hint when present.
    pub terminate: Option<bool>,
}

/// Context passed to `before_tool_call`.
#[derive(Clone)]
pub struct BeforeToolCallContext {
    /// Assistant message that requested the tool call.
    pub assistant_message: AssistantMessage,
    /// Raw tool call block from the assistant message.
    pub tool_call: ToolCall,
    /// Validated tool arguments.
    pub args: Map<String, Value>,
    /// Current agent context while preparing the call.
    pub context: AgentContext,
}

/// Context passed to `after_tool_call`.
#[derive(Clone)]
pub struct AfterToolCallContext {
    /// Assistant message that requested the tool call.
    pub assistant_message: AssistantMessage,
    /// Raw tool call block from the assistant message.
    pub tool_call: ToolCall,
    /// Validated tool arguments.
    pub args: Map<String, Value>,
    /// Executed tool result before overrides.
    pub result: AgentToolResult,
    /// Whether the executed result is currently an error.
    pub is_error: bool,
    /// Current agent context while finalizing the call.
    pub context: AgentContext,
}

/// Context passed to turn-boundary hooks.
#[derive(Clone)]
pub struct ShouldStopAfterTurnContext {
    /// Assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool results passed to the preceding `turn_end` event.
    pub tool_results: Vec<ToolResultMessage>,
    /// Context after the turn's assistant message and tool results were appended.
    pub context: AgentContext,
    /// Messages this loop invocation will return if it exits here.
    pub new_messages: Vec<AgentMessage>,
}

/// Context passed to `prepare_next_turn`.
pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// Replacement runtime state for the next provider request.
#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Context for the next provider request.
    pub context: Option<AgentContext>,
    /// Model for the next provider request.
    pub model: Option<Model>,
    /// Thinking level for the next provider request.
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Converts agent messages into provider-facing LLM messages.
pub type ConvertToLlm = Arc<
    dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Result<Vec<pi_ai::Message>, AgentLoopError>>
        + Send
        + Sync,
>;

/// Optional transform applied before `convert_to_llm`.
pub type TransformContext = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Vec<AgentMessage>, AgentLoopError>>
        + Send
        + Sync,
>;

/// Resolves an API key dynamically for each LLM call.
pub type GetApiKey =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<Option<String>, AgentLoopError>> + Send + Sync>;

/// Decides whether the loop should exit after the current turn.
pub type ShouldStopAfterTurn = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> BoxFuture<'static, Result<bool, AgentLoopError>>
        + Send
        + Sync,
>;

/// Supplies replacement context/model/thinking state before the next request.
pub type PrepareNextTurn = Arc<
    dyn Fn(
            PrepareNextTurnContext,
        ) -> BoxFuture<'static, Result<Option<AgentLoopTurnUpdate>, AgentLoopError>>
        + Send
        + Sync,
>;

/// Returns steering or follow-up messages at a queue drain point.
pub type GetMessages =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<AgentMessage>, AgentLoopError>> + Send + Sync>;

/// Called after argument validation and before tool execution.
pub type BeforeToolCall = Arc<
    dyn Fn(
            BeforeToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Option<BeforeToolCallResult>, AgentLoopError>>
        + Send
        + Sync,
>;

/// Called after tool execution and before tool-result events are emitted.
pub type AfterToolCall = Arc<
    dyn Fn(
            AfterToolCallContext,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Option<AfterToolCallResult>, AgentLoopError>>
        + Send
        + Sync,
>;

/// Configuration for the low-level agent loop.
///
/// Stream scalar fields mirror `SimpleStreamOptions`. Reasoning and thinking
/// budgets are mapped into [`StreamOptions::extra`] by [`build_stream_options`].
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// Model used for provider requests.
    pub model: Model,
    /// Optional reasoning / thinking level.
    pub reasoning: Option<ModelThinkingLevel>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u64>,
    /// Optional session identifier.
    pub session_id: Option<String>,
    /// Preferred transport.
    pub transport: Option<Transport>,
    /// Prompt-cache retention preference.
    pub cache_retention: Option<CacheRetention>,
    /// Custom token budgets for thinking levels.
    pub thinking_budgets: Option<Value>,
    /// Maximum delay in milliseconds to honor for long retry waits.
    pub max_retry_delay_ms: Option<u64>,
    /// Optional metadata included in API requests.
    pub metadata: Option<Map<String, Value>>,
    /// Custom HTTP headers. A `None` value suppresses a default header.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    /// Provider-scoped environment overrides.
    pub env: Option<BTreeMap<String, String>>,
    /// Extra provider-specific options forwarded into [`StreamOptions::extra`].
    pub stream_extra: Map<String, Value>,
    /// Default tool execution mode for batches without a sequential override.
    pub tool_execution: ToolExecutionMode,
    /// Required conversion from agent messages to LLM messages.
    pub convert_to_llm: ConvertToLlm,
    /// Optional context transform applied before conversion.
    pub transform_context: Option<TransformContext>,
    /// Optional API-key resolver.
    pub get_api_key: Option<GetApiKey>,
    /// Optional post-turn stop predicate.
    pub should_stop_after_turn: Option<ShouldStopAfterTurn>,
    /// Optional next-turn state replacement.
    pub prepare_next_turn: Option<PrepareNextTurn>,
    /// Optional steering-message source.
    pub get_steering_messages: Option<GetMessages>,
    /// Optional follow-up-message source.
    pub get_follow_up_messages: Option<GetMessages>,
    /// Optional pre-execution tool hook.
    pub before_tool_call: Option<BeforeToolCall>,
    /// Optional post-execution tool hook.
    pub after_tool_call: Option<AfterToolCall>,
    /// Optional payload mutation callback.
    pub on_payload: Option<OnPayloadFn>,
    /// Optional response inspection callback.
    pub on_response: Option<OnResponseFn>,
}

impl AgentLoopConfig {
    /// Builds provider stream options from this config, a resolved key, and cancel token.
    ///
    /// When `reasoning` is present and not [`ModelThinkingLevel::Off`], inserts
    /// `extra["reasoning"]` as the lowercase level string. When
    /// `thinking_budgets` is present, inserts `extra["thinkingBudgets"]`.
    /// Values already present in `stream_extra` are preserved and take
    /// precedence over the mapped reasoning/thinking keys only when the caller
    /// already set those exact keys; otherwise the mapped keys are inserted.
    #[must_use]
    pub fn build_stream_options(
        &self,
        resolved_key: Option<String>,
        signal: Option<CancellationToken>,
    ) -> StreamOptions {
        build_stream_options(self, resolved_key, signal)
    }
}

/// Builds [`StreamOptions`] for a provider stream request.
#[must_use]
pub fn build_stream_options(
    config: &AgentLoopConfig,
    resolved_key: Option<String>,
    signal: Option<CancellationToken>,
) -> StreamOptions {
    let mut extra = config.stream_extra.clone();
    if let Some(level) = config.reasoning
        && level != ModelThinkingLevel::Off
    {
        extra
            .entry("reasoning".to_owned())
            .or_insert_with(|| Value::String(thinking_level_wire(level).to_owned()));
    }
    if let Some(budgets) = &config.thinking_budgets {
        extra
            .entry("thinkingBudgets".to_owned())
            .or_insert_with(|| budgets.clone());
    }

    StreamOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        signal,
        api_key: resolved_key,
        transport: config.transport,
        cache_retention: config.cache_retention,
        session_id: config.session_id.clone(),
        on_payload: config.on_payload.clone(),
        on_response: config.on_response.clone(),
        headers: config.headers.clone(),
        timeout_ms: None,
        websocket_connect_timeout_ms: None,
        max_retries: None,
        max_retry_delay_ms: config.max_retry_delay_ms,
        metadata: config.metadata.clone(),
        env: config.env.clone(),
        extra,
    }
}

fn thinking_level_wire(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

/// Builds a default [`ConvertToLlm`] that keeps only LLM-compatible messages.
#[must_use]
pub fn default_convert_to_llm_hook() -> ConvertToLlm {
    Arc::new(|messages| {
        Box::pin(async move { Ok(crate::message::default_convert_to_llm(&messages)) })
    })
}

/// Convenience constructor for a text-only user message at the current time.
#[must_use]
pub fn text_user_message(text: impl Into<String>) -> AgentMessage {
    crate::message::user_text(text, std::iter::empty::<ImageContent>())
}

/// Convenience constructor for a text tool-result content block.
#[must_use]
pub fn text_tool_content(text: impl Into<String>) -> pi_ai::ToolResultContent {
    pi_ai::ToolResultContent::Text(TextContent::new(text))
}

/// Maps a [`ToolError`] into an [`AgentToolResult`].
#[must_use]
pub fn tool_error_result(error: ToolError) -> AgentToolResult {
    AgentToolResult::from(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{Model, ModelCost, ModelInput};
    use serde_json::json;

    fn sample_model() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "openai-completions".to_owned(),
            provider: "openai".to_owned(),
            base_url: "https://example.test".to_owned(),
            reasoning: true,
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

    fn sample_config(
        reasoning: Option<ModelThinkingLevel>,
        budgets: Option<Value>,
    ) -> AgentLoopConfig {
        AgentLoopConfig {
            model: sample_model(),
            reasoning,
            temperature: Some(0.2),
            max_tokens: Some(256),
            session_id: Some("session-1".to_owned()),
            transport: Some(Transport::Sse),
            cache_retention: Some(CacheRetention::Short),
            thinking_budgets: budgets,
            max_retry_delay_ms: Some(1_000),
            metadata: Some(Map::from_iter([("k".to_owned(), json!("v"))])),
            headers: Some(BTreeMap::from([(
                "X-Test".to_owned(),
                Some("1".to_owned()),
            )])),
            env: Some(BTreeMap::from([("FOO".to_owned(), "bar".to_owned())])),
            stream_extra: Map::from_iter([("toolChoice".to_owned(), json!("auto"))]),
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
        }
    }

    #[test]
    fn build_stream_options_maps_reasoning_and_thinking_budgets_keys() {
        let budgets = json!({ "low": 2048, "high": 8192 });
        let options = build_stream_options(
            &sample_config(Some(ModelThinkingLevel::High), Some(budgets.clone())),
            Some("key".to_owned()),
            Some(CancellationToken::new()),
        );

        assert_eq!(options.temperature, Some(0.2));
        assert_eq!(options.max_tokens, Some(256));
        assert_eq!(options.api_key.as_deref(), Some("key"));
        assert_eq!(options.session_id.as_deref(), Some("session-1"));
        assert_eq!(options.transport, Some(Transport::Sse));
        assert_eq!(options.cache_retention, Some(CacheRetention::Short));
        assert_eq!(options.max_retry_delay_ms, Some(1_000));
        assert_eq!(
            options.metadata,
            Some(Map::from_iter([("k".to_owned(), json!("v"))]))
        );
        assert!(options.signal.is_some());
        assert_eq!(options.extra.get("reasoning"), Some(&json!("high")));
        assert_eq!(options.extra.get("thinkingBudgets"), Some(&budgets));
        assert_eq!(options.extra.get("toolChoice"), Some(&json!("auto")));
    }

    #[test]
    fn build_stream_options_skips_reasoning_when_off_or_absent() {
        let off = build_stream_options(
            &sample_config(Some(ModelThinkingLevel::Off), None),
            None,
            None,
        );
        assert!(off.extra.get("reasoning").is_none());
        assert!(off.extra.get("thinkingBudgets").is_none());

        let absent = build_stream_options(&sample_config(None, None), None, None);
        assert!(absent.extra.get("reasoning").is_none());
    }

    #[test]
    fn build_stream_options_preserves_existing_stream_extra_keys() {
        let mut config = sample_config(Some(ModelThinkingLevel::Low), Some(json!({ "low": 1 })));
        config
            .stream_extra
            .insert("reasoning".to_owned(), json!("custom"));
        config
            .stream_extra
            .insert("thinkingBudgets".to_owned(), json!({ "low": 9 }));

        let options = build_stream_options(&config, None, None);
        assert_eq!(options.extra.get("reasoning"), Some(&json!("custom")));
        assert_eq!(
            options.extra.get("thinkingBudgets"),
            Some(&json!({ "low": 9 }))
        );
    }
}
