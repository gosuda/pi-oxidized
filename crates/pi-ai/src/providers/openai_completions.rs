//! Native OpenAI-compatible Chat Completions adapter.
//!
//! Chat Completions intentionally owns its request/message/tool conversion and
//! streaming accumulator. Responses payloads are not routed through this module.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{StreamExt, stream::BoxStream};
use reqwest::{Client, Request, Response};
use serde_json::{Map, Value, json};

use super::shared::cloudflare::resolve_model;
use super::shared::{
    calculate_cost, parse_streaming_json, sanitize_surrogates, transform_messages,
    truncate_error_body,
};
use super::stream_state::{AssistantState, ProviderEventSender};
use super::transport::{DataSseDecoder, DataSseEvent, HttpTransport, TransportError};
use crate::provider::{Provider, ProviderError, StreamOptionKey, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ModelInput, ModelThinkingLevel, StopReason, ThinkingContent, Tool,
    ToolResultContent, Usage, UsageCost, UserContent, UserMessageContent,
};

const EVENT_CHANNEL_CAPACITY: usize = 64;
const NO_TOOL_OUTPUT: &str = "(no tool output)";

/// OpenAI-compatible `/chat/completions` streaming adapter.
#[derive(Clone, Debug)]
pub struct OpenAiCompletions {
    transport: HttpTransport,
}

impl OpenAiCompletions {
    /// Create an adapter backed by an already-configured reqwest client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for OpenAiCompletions {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<crate::types::AssistantMessageEvent, ProviderError>> {
        let (sender, stream) = ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CHANNEL_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        let adapter = self.clone();
        let model = resolve_model(model, options.env.as_ref()).into_owned();
        tokio::spawn(async move {
            let message = AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            let mut processor = CompletionsProcessor::new(model.clone(), message, sender);
            if processor.start().await.is_err() {
                return;
            }
            if let Err(failure) = adapter
                .run(&model, &context, &options, &mut processor)
                .await
            {
                let aborted = failure.aborted
                    || options
                        .signal
                        .as_ref()
                        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
                let reason = if aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                let message = if aborted {
                    failure.message
                } else {
                    format!("OpenAI API error: {}", failure.message)
                };
                let _terminal = processor.fail(reason, message).await;
            }
        });
        stream
    }
}

impl OpenAiCompletions {
    async fn run(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        processor: &mut CompletionsProcessor,
    ) -> Result<(), AdapterFailure> {
        let compat = Compat::resolve(model);
        let cache_retention = resolve_cache_retention(options);
        let headers = build_headers(model, options, &compat, cache_retention);
        ensure_auth(model, options, &headers)?;
        let mut payload = build_payload(model, context, options, &compat, cache_retention);
        if let Some(callback) = options.on_payload.as_ref() {
            callback(&mut payload, model)
                .await
                .map_err(|error| AdapterFailure::new(error.to_string()))?;
        }
        let request = build_request(&self.transport, model, options, headers, &payload)?;
        let response = self
            .transport
            .execute(
                request,
                model,
                options.signal.as_ref(),
                options.on_response.as_ref(),
            )
            .await
            .map_err(AdapterFailure::from_transport)?;
        consume_response(response, options, processor).await
    }
}

async fn consume_response(
    response: Response,
    options: &StreamOptions,
    processor: &mut CompletionsProcessor,
) -> Result<(), AdapterFailure> {
    let status = response.status();
    if !status.is_success() {
        let body = HttpTransport::read_error_body(response, options.signal.as_ref())
            .await
            .map_err(AdapterFailure::from_transport)?;
        return Err(AdapterFailure::new(format!(
            "{}: {}",
            status.as_u16(),
            truncate_error_body(&body)
        )));
    }
    let mut decoder = DataSseDecoder::default();
    let mut body = response.bytes_stream();
    let mut provider_done = false;
    while !provider_done {
        let next = if let Some(signal) = options.signal.as_ref() {
            tokio::select! {
                () = signal.cancelled() => return Err(AdapterFailure::aborted("Request was aborted")),
                next = body.next() => next,
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| AdapterFailure::new(error.to_string()))?;
        for event in decoder
            .push(&chunk)
            .map_err(|error| AdapterFailure::new(error.to_string()))?
        {
            match event {
                DataSseEvent::Done => provider_done = true,
                DataSseEvent::Data(data) => {
                    let chunk = serde_json::from_str::<Value>(&data).map_err(|error| {
                        AdapterFailure::new(format!("invalid Chat Completions chunk: {error}"))
                    })?;
                    processor.process_chunk(&chunk).await?;
                }
            }
        }
    }
    if !provider_done {
        for event in decoder
            .finish()
            .map_err(|error| AdapterFailure::new(error.to_string()))?
        {
            if let DataSseEvent::Data(data) = event {
                let chunk = serde_json::from_str::<Value>(&data).map_err(|error| {
                    AdapterFailure::new(format!("invalid Chat Completions chunk: {error}"))
                })?;
                processor.process_chunk(&chunk).await?;
            }
        }
    }
    processor.complete().await
}

fn build_request(
    transport: &HttpTransport,
    model: &Model,
    options: &StreamOptions,
    headers: BTreeMap<String, String>,
    payload: &Value,
) -> Result<Request, AdapterFailure> {
    let endpoint = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let mut builder = transport.post(endpoint).json(&payload);
    for (name, value) in headers {
        builder = builder.header(&name, &value);
    }
    if let Some(timeout_ms) = options.timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout_ms));
    }
    builder
        .build()
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

#[derive(Clone, Debug)]
struct CompletionsProcessor {
    model: Model,
    sender: ProviderEventSender,
    state: AssistantState,
    text_index: Option<u64>,
    thinking_index: Option<u64>,
    tool_by_stream_index: BTreeMap<u64, u64>,
    tool_by_id: BTreeMap<String, u64>,
    partial_arguments: BTreeMap<u64, String>,
    pending_reasoning: BTreeMap<String, String>,
    has_finish_reason: bool,
    finish_reason: StopReason,
    finish_error: Option<String>,
}

impl CompletionsProcessor {
    fn new(model: Model, message: AssistantMessage, sender: ProviderEventSender) -> Self {
        Self {
            model,
            sender,
            state: AssistantState::new(message),
            text_index: None,
            thinking_index: None,
            tool_by_stream_index: BTreeMap::new(),
            tool_by_id: BTreeMap::new(),
            partial_arguments: BTreeMap::new(),
            pending_reasoning: BTreeMap::new(),
            has_finish_reason: false,
            finish_reason: StopReason::Stop,
            finish_error: None,
        }
    }

    async fn start(&self) -> Result<(), AdapterFailure> {
        self.sender
            .start(self.state.snapshot())
            .await
            .map_err(send_failure)
    }

    async fn process_chunk(&mut self, chunk: &Value) -> Result<(), AdapterFailure> {
        if !chunk.is_object() {
            return Ok(());
        }
        let response_id = chunk.get("id").and_then(Value::as_str).map(str::to_owned);
        let response_model = chunk
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && *value != self.model.id)
            .map(str::to_owned);
        if response_id.is_some() || response_model.is_some() {
            self.update_message(|message| {
                if message.response_id.is_none() {
                    message.response_id = response_id;
                }
                if message.response_model.is_none() {
                    message.response_model = response_model;
                }
            });
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            let parsed = parse_chunk_usage(usage, &self.model);
            self.update_message(|message| message.usage = parsed);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(usage) = choice.get("usage").filter(|usage| usage.is_object()) {
            let parsed = parse_chunk_usage(usage, &self.model);
            self.update_message(|message| message.usage = parsed);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            let (stop_reason, error) = map_stop_reason(reason);
            self.finish_reason = stop_reason;
            self.finish_error = error;
            self.has_finish_reason = true;
        }
        let Some(delta) = choice.get("delta").filter(|delta| delta.is_object()) else {
            return Ok(());
        };
        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let index = self.ensure_text().await?;
            let event = self
                .state
                .text_delta(index, content)
                .map_err(state_failure)?;
            self.sender.event(event).await.map_err(send_failure)?;
        }
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(reasoning) = delta
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                let signature = if self.model.provider == "opencode-go" && field == "reasoning" {
                    "reasoning_content"
                } else {
                    field
                };
                let index = self.ensure_thinking(signature).await?;
                let event = self
                    .state
                    .thinking_delta(index, reasoning)
                    .map_err(state_failure)?;
                self.sender.event(event).await.map_err(send_failure)?;
                break;
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_delta in tool_calls {
                self.process_tool_delta(tool_delta).await?;
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                self.process_reasoning_detail(detail)?;
            }
        }
        Ok(())
    }

    async fn ensure_text(&mut self) -> Result<u64, AdapterFailure> {
        if let Some(index) = self.text_index {
            return Ok(index);
        }
        let index = u64::try_from(self.state.message().content.len())
            .map_err(|_| AdapterFailure::new("content index overflow"))?;
        let event = self.state.start_text().map_err(state_failure)?;
        self.sender.event(event).await.map_err(send_failure)?;
        self.text_index = Some(index);
        Ok(index)
    }

    async fn ensure_thinking(&mut self, signature: &str) -> Result<u64, AdapterFailure> {
        if let Some(index) = self.thinking_index {
            return Ok(index);
        }
        let index = u64::try_from(self.state.message().content.len())
            .map_err(|_| AdapterFailure::new("content index overflow"))?;
        let event = self.state.start_thinking().map_err(state_failure)?;
        self.set_thinking_signature(index, signature)?;
        let event = refresh_partial(event, &self.state.snapshot())?;
        self.sender.event(event).await.map_err(send_failure)?;
        self.thinking_index = Some(index);
        Ok(index)
    }

    async fn process_tool_delta(&mut self, delta: &Value) -> Result<(), AdapterFailure> {
        let stream_index = delta.get("index").and_then(Value::as_u64);
        let wire_id = delta.get("id").and_then(Value::as_str);
        let mut content_index = stream_index
            .and_then(|index| self.tool_by_stream_index.get(&index).copied())
            .or_else(|| wire_id.and_then(|id| self.tool_by_id.get(id).copied()));
        if content_index.is_none() {
            let index = u64::try_from(self.state.message().content.len())
                .map_err(|_| AdapterFailure::new("content index overflow"))?;
            let id = wire_id.unwrap_or("");
            let name = delta
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let event = self
                .state
                .start_tool_call(id, name)
                .map_err(state_failure)?;
            self.sender.event(event).await.map_err(send_failure)?;
            self.partial_arguments.insert(index, String::new());
            content_index = Some(index);
        }
        let content_index = content_index.unwrap_or(0);
        if let Some(index) = stream_index {
            self.tool_by_stream_index.insert(index, content_index);
        }
        if let Some(id) = wire_id {
            self.tool_by_id.insert(id.to_owned(), content_index);
            self.update_tool(content_index, |tool| id.clone_into(&mut tool.id))?;
            if let Some(signature) = self.pending_reasoning.remove(id) {
                self.update_tool(content_index, |tool| {
                    tool.thought_signature = Some(signature);
                })?;
            }
        }
        if let Some(name) = delta
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            self.update_tool(content_index, |tool| name.clone_into(&mut tool.name))?;
        }
        let arguments = delta
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("");
        let partial = self.partial_arguments.entry(content_index).or_default();
        partial.push_str(arguments);
        let parsed = parse_streaming_json(partial);
        self.update_tool(content_index, |tool| tool.arguments = parsed)?;
        let event = self
            .state
            .tool_call_delta(content_index, arguments)
            .map_err(state_failure)?;
        self.sender.event(event).await.map_err(send_failure)
    }

    fn process_reasoning_detail(&mut self, detail: &Value) -> Result<(), AdapterFailure> {
        if detail.get("type").and_then(Value::as_str) != Some("reasoning.encrypted")
            || detail
                .get("data")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Ok(());
        }
        let Some(id) = detail.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        let signature = serde_json::to_string(detail).unwrap_or_else(|_| "{}".to_owned());
        if let Some(content_index) = self.tool_by_id.get(id).copied() {
            self.update_tool(content_index, |tool| {
                tool.thought_signature = Some(signature);
            })?;
        } else {
            self.pending_reasoning.insert(id.to_owned(), signature);
        }
        Ok(())
    }

    async fn complete(&mut self) -> Result<(), AdapterFailure> {
        let content_len = self.state.message().content.len();
        for index in 0..content_len {
            let content_index =
                u64::try_from(index).map_err(|_| AdapterFailure::new("content index overflow"))?;
            let block = self.state.message().content[index].clone();
            let event = match block {
                AssistantContent::Text(_) => {
                    self.state.end_text(content_index).map_err(state_failure)?
                }
                AssistantContent::Thinking(_) => self
                    .state
                    .end_thinking(content_index)
                    .map_err(state_failure)?,
                AssistantContent::ToolCall(_) => {
                    let arguments = self
                        .partial_arguments
                        .remove(&content_index)
                        .map_or_else(Map::new, |partial| parse_streaming_json(&partial));
                    self.state
                        .end_tool_call(content_index, arguments)
                        .map_err(state_failure)?
                }
            };
            self.sender.event(event).await.map_err(send_failure)?;
        }
        if !self.has_finish_reason {
            return Err(AdapterFailure::new("Stream ended without finish_reason"));
        }
        if self.finish_reason == StopReason::Error {
            return Err(AdapterFailure::new(
                self.finish_error
                    .clone()
                    .unwrap_or_else(|| "Provider returned an error stop reason".into()),
            ));
        }
        let reason = match self.finish_reason {
            StopReason::Length => DoneReason::Length,
            StopReason::ToolUse => DoneReason::ToolUse,
            _ => DoneReason::Stop,
        };
        let message = self.state.finish(reason);
        self.sender
            .done(reason, message)
            .await
            .map_err(send_failure)
    }

    async fn fail(&mut self, reason: ErrorReason, message: String) -> Result<(), AdapterFailure> {
        let message = self.state.fail(reason, message);
        self.sender
            .error(reason, message)
            .await
            .map_err(send_failure)
    }

    fn update_message(&mut self, update: impl FnOnce(&mut AssistantMessage)) {
        self.state.message_mut(update);
    }

    fn update_tool(
        &mut self,
        content_index: u64,
        update: impl FnOnce(&mut crate::types::ToolCall),
    ) -> Result<(), AdapterFailure> {
        self.update_content(content_index, |block| {
            if let AssistantContent::ToolCall(tool) = block {
                update(tool);
            }
        })
    }

    fn set_thinking_signature(
        &mut self,
        content_index: u64,
        signature: &str,
    ) -> Result<(), AdapterFailure> {
        self.update_content(content_index, |block| {
            if let AssistantContent::Thinking(thinking) = block {
                thinking.thinking_signature = Some(signature.to_owned());
            }
        })
    }

    fn update_content(
        &mut self,
        content_index: u64,
        update: impl FnOnce(&mut AssistantContent),
    ) -> Result<(), AdapterFailure> {
        let index = usize::try_from(content_index)
            .map_err(|_| AdapterFailure::new("content index overflow"))?;
        self.state.message_mut(|message| {
            let block = message.content.get_mut(index).ok_or_else(|| {
                AdapterFailure::new(format!("content block {content_index} does not exist"))
            })?;
            update(block);
            Ok::<(), AdapterFailure>(())
        })?;
        Ok(())
    }
}

fn build_headers(
    model: &Model,
    options: &StreamOptions,
    compat: &Compat,
    cache_retention: CacheRetention,
) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(session_id) = options.session_id.as_deref().filter(|_| {
        cache_retention != CacheRetention::None && compat.stream.send_session_affinity_headers
    }) {
        match compat.stream.session_affinity_format.as_str() {
            "openrouter" => {
                headers.insert("x-session-id".into(), session_id.into());
            }
            "openai-nosession" => {
                headers.insert("x-client-request-id".into(), session_id.into());
                headers.insert("x-session-affinity".into(), session_id.into());
            }
            _ => {
                headers.insert("session_id".into(), session_id.into());
                headers.insert("x-client-request-id".into(), session_id.into());
                headers.insert("x-session-affinity".into(), session_id.into());
            }
        }
    }
    merge_option_headers(&mut headers, options.headers.as_ref());
    if let Some(api_key) = options.api_key.as_deref()
        && !has_nonempty_header(&headers, "authorization")
    {
        headers.insert("authorization".into(), format!("Bearer {api_key}"));
    }
    headers
}

fn ensure_auth(
    model: &Model,
    options: &StreamOptions,
    headers: &BTreeMap<String, String>,
) -> Result<(), AdapterFailure> {
    if options
        .api_key
        .as_deref()
        .is_some_and(|key| !key.is_empty())
        || has_nonempty_header(headers, "authorization")
        || has_nonempty_header(headers, "cf-aig-authorization")
    {
        Ok(())
    } else {
        Err(AdapterFailure::new(format!(
            "No API key for provider: {}",
            model.provider
        )))
    }
}

fn build_payload(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &Compat,
    cache_retention: CacheRetention,
) -> Value {
    let mut messages = convert_messages(model, context, compat);
    let mut payload = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
    });
    let cache_key_allowed = (model.base_url.contains("api.openai.com")
        && cache_retention != CacheRetention::None)
        || (cache_retention == CacheRetention::Long && compat.store.supports_long_cache_retention);
    if cache_key_allowed && let Some(session_id) = options.session_id.as_deref() {
        payload["prompt_cache_key"] = Value::String(session_id.chars().take(64).collect());
    }
    if cache_retention == CacheRetention::Long && compat.store.supports_long_cache_retention {
        payload["prompt_cache_retention"] = Value::String("24h".into());
    }
    if compat.stream.supports_usage_in_streaming {
        payload["stream_options"] = json!({"include_usage": true});
    }
    if compat.store.supports_store {
        payload["store"] = Value::Bool(false);
    }
    if let Some(max_tokens) = options.max_tokens {
        payload[&compat.stream.max_tokens_field] = Value::from(max_tokens);
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = Value::from(temperature);
    }

    let deferred_names = if compat.tools.deferred_tools_mode.as_deref() == Some("kimi") {
        deferred_tool_names(&context.messages)
    } else {
        BTreeSet::new()
    };
    let active_tools: Vec<Tool> = context
        .tools
        .iter()
        .flatten()
        .filter(|tool| !deferred_names.contains(&tool.name))
        .cloned()
        .collect();
    if !active_tools.is_empty() {
        payload["tools"] = Value::Array(convert_tools(&active_tools, compat));
        if compat.tools.zai_tool_stream {
            payload["tool_stream"] = Value::Bool(true);
        }
    } else if has_tool_history(&context.messages) {
        payload["tools"] = Value::Array(Vec::new());
    }
    if compat.store.cache_control_format.as_deref() == Some("anthropic")
        && cache_retention != CacheRetention::None
    {
        let cache_control = if cache_retention == CacheRetention::Long
            && compat.store.supports_long_cache_retention
        {
            json!({"type":"ephemeral", "ttl":"1h"})
        } else {
            json!({"type":"ephemeral"})
        };
        apply_anthropic_cache_control(&mut messages, payload.get_mut("tools"), &cache_control);
        payload["messages"] = Value::Array(messages);
    }
    if let Some(tool_choice) = options.extra_value(StreamOptionKey::TOOL_CHOICE) {
        payload["tool_choice"] = tool_choice.clone();
    }
    apply_thinking(model, options, compat, &mut payload);
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|value| value.get("openRouterRouting"))
    {
        payload["provider"] = routing.clone();
    }
    if let Some(routing) = model
        .compat
        .as_ref()
        .and_then(|value| value.get("vercelGatewayRouting"))
    {
        let only = routing.get("only");
        let order = routing.get("order");
        if only.is_some() || order.is_some() {
            let mut gateway = Map::new();
            if let Some(only) = only {
                gateway.insert("only".into(), only.clone());
            }
            if let Some(order) = order {
                gateway.insert("order".into(), order.clone());
            }
            payload["providerOptions"] = json!({"gateway": gateway});
        }
    }
    payload
}

fn convert_messages(model: &Model, context: &Context, compat: &Compat) -> Vec<Value> {
    let mut normalize = |id: &str, _target: &Model, _source: &AssistantMessage| {
        normalize_completion_tool_id(id, &model.provider)
    };
    let transformed = transform_messages(&context.messages, model, &mut normalize);
    let mut messages = Vec::new();
    if let Some(system_prompt) = context.system_prompt.as_deref() {
        let role = if model.reasoning && compat.roles.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({"role": role, "content": sanitize_surrogates(system_prompt)}));
    }
    let mut index = 0;
    let mut last_role = "";
    while index < transformed.len() {
        let message = &transformed[index];
        if compat.tools.requires_assistant_after_tool_result
            && last_role == "toolResult"
            && matches!(message, Message::User(_))
        {
            messages
                .push(json!({"role":"assistant","content":"I have processed the tool results."}));
        }
        match message {
            Message::User(user) => {
                convert_user_message(user, &mut messages);
                last_role = "user";
            }
            Message::Assistant(assistant) => {
                if convert_assistant_message(model, assistant, compat, &mut messages) {
                    last_role = "assistant";
                }
            }
            Message::ToolResult(_) => {
                last_role = convert_tool_result_batch(
                    model,
                    context,
                    compat,
                    &transformed,
                    &mut index,
                    &mut messages,
                );
            }
        }
        index += 1;
    }
    messages
}

fn convert_user_message(user: &crate::types::UserMessage, messages: &mut Vec<Value>) {
    match &user.content {
        UserMessageContent::Text(text) => messages.push(json!({
            "role":"user", "content": sanitize_surrogates(text)
        })),
        UserMessageContent::Blocks(blocks) => {
            let parts: Vec<Value> = blocks
                .iter()
                .map(|block| match block {
                    UserContent::Text(text) => json!({
                        "type":"text", "text": sanitize_surrogates(&text.text)
                    }),
                    UserContent::Image(image) => json!({
                        "type":"image_url",
                        "image_url":{"url":format!("data:{};base64,{}",image.mime_type,image.data)}
                    }),
                })
                .collect();
            if !parts.is_empty() {
                messages.push(json!({"role":"user","content":parts}));
            }
        }
    }
}

fn convert_assistant_message(
    model: &Model,
    assistant: &AssistantMessage,
    compat: &Compat,
    messages: &mut Vec<Value>,
) -> bool {
    let text_parts: Vec<&str> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                Some(text.text.as_str())
            }
            _ => None,
        })
        .collect();
    let text = text_parts.join("");
    let thinking: Vec<_> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Thinking(thinking) if !thinking.thinking.trim().is_empty() => {
                Some(thinking)
            }
            _ => None,
        })
        .collect();
    let mut converted = json!({
        "role":"assistant",
        "content": if compat.tools.requires_assistant_after_tool_result {
            Value::String(String::new())
        } else {
            Value::Null
        },
    });
    apply_assistant_text_and_thinking(model, compat, &text_parts, &text, &thinking, &mut converted);
    apply_assistant_tool_calls(assistant, &mut converted);
    if compat.thinking.requires_reasoning_content
        && model.reasoning
        && converted.get("reasoning_content").is_none()
    {
        converted["reasoning_content"] = Value::String(String::new());
    }
    let has_content = converted
        .get("content")
        .is_some_and(|content| match content {
            Value::String(text) => !text.is_empty(),
            Value::Array(parts) => !parts.is_empty(),
            _ => false,
        });
    if has_content || converted.get("tool_calls").is_some() {
        messages.push(converted);
        true
    } else {
        false
    }
}

fn apply_assistant_text_and_thinking(
    model: &Model,
    compat: &Compat,
    text_parts: &[&str],
    text: &str,
    thinking: &[&ThinkingContent],
    converted: &mut Value,
) {
    if !thinking.is_empty() {
        if compat.thinking.requires_thinking_as_text {
            let thinking_text = thinking
                .iter()
                .map(|block| sanitize_surrogates(&block.thinking).into_owned())
                .collect::<Vec<_>>()
                .join("\n\n");
            let mut parts = vec![json!({"type":"text","text":thinking_text})];
            parts.extend(
                text_parts
                    .iter()
                    .map(|text| json!({"type":"text","text":sanitize_surrogates(text)})),
            );
            converted["content"] = Value::Array(parts);
        } else {
            if !text.is_empty() {
                converted["content"] = Value::String(sanitize_surrogates(text).into_owned());
            }
            if let Some(mut signature) = thinking[0].thinking_signature.as_deref() {
                if model.provider == "opencode-go" && signature == "reasoning" {
                    signature = "reasoning_content";
                }
                converted[signature] = Value::String(
                    thinking
                        .iter()
                        .map(|block| block.thinking.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
    } else if !text.is_empty() {
        converted["content"] = Value::String(sanitize_surrogates(text).into_owned());
    }
}

fn apply_assistant_tool_calls(assistant: &AssistantMessage, converted: &mut Value) {
    let tool_calls: Vec<_> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(tool) => Some(tool),
            _ => None,
        })
        .collect();
    if tool_calls.is_empty() {
        return;
    }
    converted["tool_calls"] = Value::Array(
        tool_calls
            .iter()
            .map(|tool| {
                json!({
                    "id":tool.id,
                    "type":"function",
                    "function":{
                        "name":tool.name,
                        "arguments":serde_json::to_string(&tool.arguments)
                            .unwrap_or_else(|_| "{}".into())
                    }
                })
            })
            .collect(),
    );
    let reasoning_details: Vec<Value> = tool_calls
        .iter()
        .filter_map(|tool| tool.thought_signature.as_deref())
        .filter_map(|signature| serde_json::from_str(signature).ok())
        .collect();
    if !reasoning_details.is_empty() {
        converted["reasoning_details"] = Value::Array(reasoning_details);
    }
}

fn convert_tool_result_batch(
    model: &Model,
    context: &Context,
    compat: &Compat,
    transformed: &[Message],
    index: &mut usize,
    messages: &mut Vec<Value>,
) -> &'static str {
    let mut images = Vec::new();
    let mut deferred_names = BTreeSet::new();
    let mut cursor = *index;
    while cursor < transformed.len() {
        let Message::ToolResult(result) = &transformed[cursor] else {
            break;
        };
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let has_images = result
            .content
            .iter()
            .any(|block| matches!(block, ToolResultContent::Image(_)));
        let output = if !text.is_empty() {
            text
        } else if has_images {
            "(see attached image)".into()
        } else {
            NO_TOOL_OUTPUT.into()
        };
        let mut converted = json!({
            "role":"tool",
            "content":sanitize_surrogates(&output),
            "tool_call_id":result.tool_call_id,
        });
        if compat.tools.requires_tool_result_name && !result.tool_name.is_empty() {
            converted["name"] = Value::String(result.tool_name.clone());
        }
        messages.push(converted);
        if compat.tools.deferred_tools_mode.as_deref() == Some("kimi") {
            deferred_names.extend(result.added_tool_names.iter().flatten().cloned());
        }
        if has_images && model.input.contains(&ModelInput::Image) {
            for block in &result.content {
                if let ToolResultContent::Image(image) = block {
                    images.push(json!({
                        "type":"image_url",
                        "image_url":{
                            "url":format!("data:{};base64,{}", image.mime_type, image.data)
                        }
                    }));
                }
            }
        }
        cursor += 1;
    }
    *index = cursor.saturating_sub(1);
    let last_role = if images.is_empty() {
        "toolResult"
    } else {
        if compat.tools.requires_assistant_after_tool_result {
            messages
                .push(json!({"role":"assistant","content":"I have processed the tool results."}));
        }
        let mut parts = vec![json!({"type":"text","text":"Attached image(s) from tool result:"})];
        parts.extend(images);
        messages.push(json!({"role":"user","content":parts}));
        "user"
    };
    if !deferred_names.is_empty() {
        let deferred: Vec<Tool> = context
            .tools
            .iter()
            .flatten()
            .filter(|tool| deferred_names.contains(&tool.name))
            .cloned()
            .collect();
        if !deferred.is_empty() {
            messages.push(json!({"role":"system","tools":convert_tools(&deferred,compat)}));
        }
    }
    last_role
}

fn convert_tools(tools: &[Tool], compat: &Compat) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut function = json!({
                "name":tool.name,
                "description":tool.description,
                "parameters":tool.parameters,
            });
            if compat.store.supports_strict_mode {
                function["strict"] = Value::Bool(false);
            }
            json!({"type":"function","function":function})
        })
        .collect()
}

fn apply_chat_template_object(
    model: &Model,
    effort: Option<&str>,
    input_key: &str,
    output_key: &str,
    payload: &mut Value,
) {
    if let Some(source) = model
        .compat
        .as_ref()
        .and_then(|value| value.get(input_key))
        .and_then(Value::as_object)
    {
        let mut resolved = Map::new();
        for (name, value) in source {
            if let Some(value) = resolve_chat_template_value(model, effort, value) {
                resolved.insert(name.clone(), value);
            }
        }
        if !resolved.is_empty() {
            payload[output_key] = Value::Object(resolved);
        }
    }
}
fn apply_openai_reasoning_effort(model: &Model, effort: Option<&str>, payload: &mut Value) {
    let value = match effort {
        Some(requested) => Some(map_thinking_level(model, requested)),
        None => off_thinking_value(model),
    };
    if let Some(value) = value {
        payload["reasoning_effort"] = Value::String(value);
    }
}

fn apply_thinking(model: &Model, options: &StreamOptions, compat: &Compat, payload: &mut Value) {
    if !model.reasoning {
        return;
    }
    let effort = options
        .extra_value(StreamOptionKey::REASONING_EFFORT)
        .and_then(Value::as_str);
    let mapped = effort.map(|value| map_thinking_level(model, value));
    match compat.thinking.thinking_format.as_str() {
        "zai" => {
            payload["thinking"] = if effort.is_some() {
                json!({"type":"enabled","clear_thinking":false})
            } else {
                json!({"type":"disabled"})
            };
            if compat.thinking.supports_reasoning_effort
                && let Some(mapped) = mapped
            {
                payload["reasoning_effort"] = Value::String(mapped);
            }
        }
        "qwen" => payload["enable_thinking"] = Value::Bool(effort.is_some()),
        "qwen-chat-template" => {
            payload["chat_template_kwargs"] =
                json!({"enable_thinking":effort.is_some(),"preserve_thinking":true});
        }
        "chat-template" => {
            apply_chat_template_object(
                model,
                effort,
                "chatTemplateKwargs",
                "chat_template_kwargs",
                payload,
            );
        }
        "baseten" => {
            apply_chat_template_object(
                model,
                effort,
                "chatTemplateArgs",
                "chat_template_args",
                payload,
            );
            if compat.thinking.supports_reasoning_effort {
                apply_openai_reasoning_effort(model, effort, payload);
            }
        }
        "deepseek" => {
            let off_supported = !matches!(
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get(&ModelThinkingLevel::Off)),
                Some(None)
            );
            if effort.is_some() {
                payload["thinking"] = json!({"type":"enabled"});
            } else if off_supported {
                payload["thinking"] = json!({"type":"disabled"});
            }
            if compat.thinking.supports_reasoning_effort
                && let Some(mapped) = mapped
            {
                payload["reasoning_effort"] = Value::String(mapped);
            }
        }
        "openrouter" => {
            let value = mapped.or_else(|| off_thinking_value(model));
            if let Some(value) = value {
                payload["reasoning"] = json!({"effort":value});
            }
        }
        "ant-ling" if effort.is_some() => {
            if let Some(mapped) = mapped {
                payload["reasoning"] = json!({"effort":mapped});
            }
        }
        "together" => {
            payload["reasoning"] = json!({"enabled":effort.is_some()});
            if compat.thinking.supports_reasoning_effort
                && let Some(mapped) = mapped
            {
                payload["reasoning_effort"] = Value::String(mapped);
            }
        }
        "string-thinking" => {
            if let Some(value) = mapped.or_else(|| off_thinking_value(model)) {
                payload["thinking"] = Value::String(value);
            }
        }
        _ => {
            if compat.thinking.supports_reasoning_effort {
                apply_openai_reasoning_effort(model, effort, payload);
            }
        }
    }
}

fn resolve_chat_template_value(
    model: &Model,
    effort: Option<&str>,
    value: &Value,
) -> Option<Value> {
    let Some(object) = value.as_object() else {
        return Some(value.clone());
    };
    if effort.is_none() && object.get("omitWhenOff").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    if object.get("$var").and_then(Value::as_str) == Some("thinking.enabled") {
        return Some(Value::Bool(effort.is_some()));
    }
    effort
        .map(|value| Value::String(map_thinking_level(model, value)))
        .or_else(|| off_thinking_value(model).map(Value::String))
}

fn off_thinking_value(model: &Model) -> Option<String> {
    model
        .thinking_level_map
        .as_ref()?
        .get(&ModelThinkingLevel::Off)?
        .clone()
}

fn map_thinking_level(model: &Model, effort: &str) -> String {
    let level = match effort {
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    };
    level
        .and_then(|level| model.thinking_level_map.as_ref()?.get(&level)?.clone())
        .unwrap_or_else(|| effort.to_owned())
}

fn apply_anthropic_cache_control(
    messages: &mut [Value],
    tools: Option<&mut Value>,
    cache_control: &Value,
) {
    if let Some(message) = messages.iter_mut().find(|message| {
        matches!(
            message.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    }) {
        add_cache_control_to_content(message, cache_control);
    }
    if let Some(tool) = tools
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
    {
        tool["cache_control"] = cache_control.clone();
    }
    for message in messages.iter_mut().rev() {
        if matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant")
        ) && add_cache_control_to_content(message, cache_control)
        {
            break;
        }
    }
}

fn add_cache_control_to_content(message: &mut Value, cache_control: &Value) -> bool {
    let Some(content) = message.get_mut("content") else {
        return false;
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        *content = json!([{"type":"text","text":text,"cache_control":cache_control}]);
        return true;
    }
    if let Some(parts) = content.as_array_mut()
        && let Some(part) = parts
            .iter_mut()
            .rev()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))
    {
        part["cache_control"] = cache_control.clone();
        return true;
    }
    false
}

fn normalize_completion_tool_id(id: &str, provider: &str) -> String {
    if let Some((call_id, _)) = id.split_once('|') {
        return call_id
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
    }
    if provider == "openai" {
        id.chars().take(40).collect()
    } else {
        id.to_owned()
    }
}

fn parse_chunk_usage(raw: &Value, model: &Model) -> Usage {
    let prompt_tokens = u64_field(raw, "prompt_tokens");
    let details = raw.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let cache_read = details
        .get("cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| raw.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| raw.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = u64_field(details, "cache_write_tokens");
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let output = u64_field(raw, "completion_tokens");
    let reasoning = raw
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write1h: None,
        reasoning: Some(reasoning),
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost::default(),
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

#[derive(Clone, Debug)]
struct Compat {
    store: CompatStore,
    roles: CompatRoles,
    thinking: CompatThinking,
    stream: CompatStream,
    tools: CompatTools,
}

#[derive(Clone, Debug)]
struct CompatStore {
    supports_store: bool,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    cache_control_format: Option<String>,
}

#[derive(Clone, Debug)]
struct CompatRoles {
    supports_developer_role: bool,
}

#[derive(Clone, Debug)]
struct CompatThinking {
    supports_reasoning_effort: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content: bool,
    thinking_format: String,
}

#[derive(Clone, Debug)]
struct CompatStream {
    supports_usage_in_streaming: bool,
    send_session_affinity_headers: bool,
    max_tokens_field: String,
    session_affinity_format: String,
}

#[derive(Clone, Debug)]
struct CompatTools {
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    zai_tool_stream: bool,
    deferred_tools_mode: Option<String>,
}

impl Compat {
    fn resolve(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let base = model.base_url.as_str();
        let detection = detect_provider(provider, base, model.id.as_str());
        let compat = model.compat.as_ref();
        Self {
            store: CompatStore {
                supports_store: compat_bool(
                    compat,
                    "supportsStore",
                    !detection.identity.nonstandard,
                ),
                supports_long_cache_retention: compat_bool(
                    compat,
                    "supportsLongCacheRetention",
                    detection.limits.supports_long_cache_retention,
                ),
                supports_strict_mode: compat_bool(
                    compat,
                    "supportsStrictMode",
                    detection.limits.supports_strict_mode,
                ),
                cache_control_format: compat_string(compat, "cacheControlFormat")
                    .map(str::to_owned)
                    .or_else(|| {
                        (provider == "openrouter" && model.id.starts_with("anthropic/"))
                            .then(|| "anthropic".into())
                    }),
            },
            roles: CompatRoles {
                supports_developer_role: compat_bool(
                    compat,
                    "supportsDeveloperRole",
                    detection.features.supports_developer_role,
                ),
            },
            thinking: CompatThinking {
                supports_reasoning_effort: compat_bool(
                    compat,
                    "supportsReasoningEffort",
                    detection.features.supports_reasoning_effort,
                ),
                requires_thinking_as_text: compat_bool(compat, "requiresThinkingAsText", false),
                requires_reasoning_content: compat_bool(
                    compat,
                    "requiresReasoningContentOnAssistantMessages",
                    detection.identity.is_deepseek,
                ),
                thinking_format: compat_string(compat, "thinkingFormat")
                    .unwrap_or(detection.thinking_format)
                    .into(),
            },
            stream: CompatStream {
                supports_usage_in_streaming: compat_bool(compat, "supportsUsageInStreaming", true),
                send_session_affinity_headers: compat_bool(
                    compat,
                    "sendSessionAffinityHeaders",
                    false,
                ),
                max_tokens_field: compat_string(compat, "maxTokensField")
                    .unwrap_or(if detection.limits.use_max_tokens {
                        "max_tokens"
                    } else {
                        "max_completion_tokens"
                    })
                    .into(),
                session_affinity_format: compat_string(compat, "sessionAffinityFormat")
                    .unwrap_or(if detection.identity.is_openrouter {
                        "openrouter"
                    } else {
                        "openai"
                    })
                    .into(),
            },
            tools: CompatTools {
                requires_tool_result_name: compat_bool(compat, "requiresToolResultName", false),
                requires_assistant_after_tool_result: compat_bool(
                    compat,
                    "requiresAssistantAfterToolResult",
                    false,
                ),
                zai_tool_stream: compat_bool(compat, "zaiToolStream", false),
                deferred_tools_mode: compat_string(compat, "deferredToolsMode").map(str::to_owned),
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ProviderDetection {
    identity: ProviderIdentity,
    limits: ProviderLimits,
    features: ProviderFeatures,
    thinking_format: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct ProviderIdentity {
    nonstandard: bool,
    is_openrouter: bool,
    is_deepseek: bool,
}

#[derive(Clone, Copy, Debug)]
struct ProviderLimits {
    use_max_tokens: bool,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
}

#[derive(Clone, Copy, Debug)]
struct ProviderFeatures {
    supports_reasoning_effort: bool,
    supports_developer_role: bool,
}

fn detect_provider(provider: &str, base: &str, model_id: &str) -> ProviderDetection {
    let is_zai = provider_is_zai(provider, base);
    let is_together = provider_is_together(provider, base);
    let is_moonshot = provider_is_moonshot(provider, base);
    let is_openrouter = provider == "openrouter" || base.contains("openrouter.ai");
    let is_workers = provider == "cloudflare-workers-ai" || base.contains("api.cloudflare.com");
    let is_gateway =
        provider == "cloudflare-ai-gateway" || base.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base.contains("api.ant-ling.com");
    let is_grok = provider == "xai" || base.contains("api.x.ai");
    let is_deepseek = provider == "deepseek" || base.contains("deepseek.com");
    let nonstandard = is_nvidia
        || provider == "cerebras"
        || base.contains("cerebras.ai")
        || is_grok
        || is_together
        || base.contains("chutes.ai")
        || base.contains("deepseek.com")
        || is_zai
        || is_moonshot
        || provider == "opencode"
        || base.contains("opencode.ai")
        || is_workers
        || is_gateway
        || is_ant_ling;
    let openrouter_developer =
        is_openrouter && (model_id.starts_with("anthropic/") || model_id.starts_with("openai/"));
    let thinking_format = if is_deepseek {
        "deepseek"
    } else if is_zai {
        "zai"
    } else if is_together {
        "together"
    } else if is_ant_ling {
        "ant-ling"
    } else if is_openrouter {
        "openrouter"
    } else {
        "openai"
    };
    ProviderDetection {
        identity: ProviderIdentity {
            nonstandard,
            is_openrouter,
            is_deepseek,
        },
        limits: ProviderLimits {
            use_max_tokens: base.contains("chutes.ai")
                || is_moonshot
                || is_gateway
                || is_together
                || is_nvidia
                || is_ant_ling,
            supports_long_cache_retention: !(is_together
                || is_workers
                || is_gateway
                || is_nvidia
                || is_ant_ling),
            supports_strict_mode: !(is_moonshot || is_together || is_gateway || is_nvidia),
        },
        features: ProviderFeatures {
            supports_reasoning_effort: !(is_grok
                || is_zai
                || is_moonshot
                || is_together
                || is_gateway
                || is_nvidia
                || is_ant_ling),
            supports_developer_role: openrouter_developer || (!nonstandard && !is_openrouter),
        },
        thinking_format,
    }
}

fn provider_is_zai(provider: &str, base: &str) -> bool {
    matches!(provider, "zai" | "zai-coding-cn")
        || base.contains("api.z.ai")
        || base.contains("open.bigmodel.cn")
}

fn provider_is_together(provider: &str, base: &str) -> bool {
    provider == "together" || base.contains("api.together.ai") || base.contains("api.together.xyz")
}

fn provider_is_moonshot(provider: &str, base: &str) -> bool {
    matches!(provider, "moonshotai" | "moonshotai-cn") || base.contains("api.moonshot.")
}

fn compat_bool(compat: Option<&Value>, name: &str, default: bool) -> bool {
    compat
        .and_then(|value| value.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn compat_string<'a>(compat: Option<&'a Value>, name: &str) -> Option<&'a str> {
    compat
        .and_then(|value| value.get(name))
        .and_then(Value::as_str)
}

fn deferred_tool_names(messages: &[Message]) -> BTreeSet<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => result.added_tool_names.as_ref(),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        Message::User(_) => false,
    })
}

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention.unwrap_or_else(|| {
        if options
            .env
            .as_ref()
            .and_then(|env| env.get("PI_CACHE_RETENTION"))
            .map(String::as_str)
            == Some("long")
        {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn u64_field(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn merge_option_headers(
    headers: &mut BTreeMap<String, String>,
    overrides: Option<&BTreeMap<String, Option<String>>>,
) {
    for (name, value) in overrides.into_iter().flatten() {
        if let Some(existing) = headers
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .cloned()
        {
            headers.remove(&existing);
        }
        if let Some(value) = value {
            headers.insert(name.clone(), value.clone());
        }
    }
}

fn has_nonempty_header(headers: &BTreeMap<String, String>, name: &str) -> bool {
    headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn send_failure(error: impl std::fmt::Display) -> AdapterFailure {
    AdapterFailure::new(error.to_string())
}

fn state_failure(error: impl std::fmt::Display) -> AdapterFailure {
    AdapterFailure::new(error.to_string())
}

fn refresh_partial(
    event: AssistantMessageEvent,
    partial: &AssistantMessage,
) -> Result<AssistantMessageEvent, AdapterFailure> {
    let mut value =
        serde_json::to_value(event).map_err(|error| AdapterFailure::new(error.to_string()))?;
    value["partial"] =
        serde_json::to_value(partial).map_err(|error| AdapterFailure::new(error.to_string()))?;
    serde_json::from_value(value).map_err(|error| AdapterFailure::new(error.to_string()))
}

#[derive(Clone, Debug)]
struct AdapterFailure {
    message: String,
    aborted: bool,
}

impl AdapterFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: true,
        }
    }

    fn from_transport(error: TransportError) -> Self {
        match error {
            TransportError::Cancelled => Self::aborted("request cancelled"),
            TransportError::Request(error) => Self::new(format!("request failed: {error}")),
            TransportError::Callback(error) => {
                Self::new(format!("response callback failed: {error}"))
            }
            TransportError::Body(error) => Self::new(format!("response body failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DoneReason, ModelCost, ModelInput, StopReason};

    fn model(provider: &str) -> Model {
        Model {
            id: "model".into(),
            name: "Model".into(),
            api: "openai-completions".into(),
            provider: provider.into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn event_capacity() -> NonZeroUsize {
        NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN)
    }

    fn processor(
        model: &Model,
    ) -> (
        CompletionsProcessor,
        crate::providers::stream_state::ProviderEventStream,
    ) {
        let (sender, stream) = ProviderEventSender::channel(event_capacity());
        (
            CompletionsProcessor::new(
                model.clone(),
                AssistantMessage::new(
                    model.api.clone(),
                    model.provider.clone(),
                    model.id.clone(),
                    1,
                ),
                sender,
            ),
            stream,
        )
    }

    #[test]
    fn tools_are_nested_and_usage_subtracts_cache_classes() {
        let compat = Compat::resolve(&model("openai"));
        let tools = convert_tools(
            &[Tool {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({"type":"object"}),
            }],
            &compat,
        );
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["strict"], false);
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":1}}),
            &model("openai"),
        );
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.total_tokens
            ),
            (5, 4, 3, 2, 14)
        );
        assert_eq!(usage.reasoning, Some(1));
    }
    #[test]
    fn thinking_formats_emit_provider_reasoning_controls() {
        fn effort_options(effort: Option<&str>) -> StreamOptions {
            let mut options = StreamOptions::default();
            if let Some(effort) = effort {
                options.insert_extra(
                    StreamOptionKey::REASONING_EFFORT,
                    Value::String(effort.to_owned()),
                );
            }
            options
        }

        // Qwen declares its format through catalog compat with effort mapping off.
        let mut qwen = model("qwen-token-plan");
        qwen.reasoning = true;
        qwen.base_url = "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1".into();
        qwen.compat = Some(json!({"thinkingFormat":"qwen","supportsReasoningEffort":false}));
        let compat = Compat::resolve(&qwen);
        let mut payload = json!({});
        apply_thinking(&qwen, &effort_options(Some("high")), &compat, &mut payload);
        assert_eq!(payload.get("enable_thinking"), Some(&Value::Bool(true)));
        assert_eq!(payload.get("reasoning_effort"), None);
        let mut payload = json!({});
        apply_thinking(&qwen, &effort_options(None), &compat, &mut payload);
        assert_eq!(payload.get("enable_thinking"), Some(&Value::Bool(false)));

        // Default OpenAI shape: raw effort, then the off-map value without effort.
        let mut openai = model("openai");
        openai.reasoning = true;
        openai.thinking_level_map = Some(BTreeMap::from([(
            ModelThinkingLevel::Off,
            Some("none".to_owned()),
        )]));
        let compat = Compat::resolve(&openai);
        let mut payload = json!({});
        apply_thinking(
            &openai,
            &effort_options(Some("high")),
            &compat,
            &mut payload,
        );
        assert_eq!(
            payload.get("reasoning_effort"),
            Some(&Value::String("high".to_owned()))
        );
        let mut payload = json!({});
        apply_thinking(&openai, &effort_options(None), &compat, &mut payload);
        assert_eq!(
            payload.get("reasoning_effort"),
            Some(&Value::String("none".to_owned()))
        );
    }
    #[test]
    fn baseten_format_emits_chat_template_args() {
        fn baseten_model(supports_effort: bool) -> Model {
            let mut model = model("baseten");
            model.reasoning = true;
            model.base_url = "https://inference.baseten.co/v1".into();
            model.compat = Some(json!({
                "thinkingFormat": "baseten",
                "supportsReasoningEffort": supports_effort,
                "chatTemplateArgs": {"enable_thinking": {"$var": "thinking.enabled"}},
            }));
            model
        }
        fn effort_options(effort: Option<&str>) -> StreamOptions {
            let mut options = StreamOptions::default();
            if let Some(effort) = effort {
                options.insert_extra(
                    StreamOptionKey::REASONING_EFFORT,
                    Value::String(effort.to_owned()),
                );
            }
            options
        }

        // Effort support off (Kimi-K2.5 shape): only the template flag is sent.
        let model = baseten_model(false);
        let compat = Compat::resolve(&model);
        let mut payload = json!({});
        apply_thinking(&model, &effort_options(Some("high")), &compat, &mut payload);
        assert_eq!(
            payload.pointer("/chat_template_args/enable_thinking"),
            Some(&Value::Bool(true))
        );
        assert_eq!(payload.get("reasoning_effort"), None);
        let mut payload = json!({});
        apply_thinking(&model, &effort_options(None), &compat, &mut payload);
        assert_eq!(
            payload.pointer("/chat_template_args/enable_thinking"),
            Some(&Value::Bool(false))
        );

        // Effort support on (GLM-5.2 shape): the mapped effort rides along.
        let model = baseten_model(true);
        let compat = Compat::resolve(&model);
        let mut payload = json!({});
        apply_thinking(&model, &effort_options(Some("high")), &compat, &mut payload);
        assert_eq!(
            payload.pointer("/chat_template_args/enable_thinking"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            payload.get("reasoning_effort"),
            Some(&Value::String("high".to_owned()))
        );
    }
    #[test]
    fn usage_cache_read_falls_back_across_provider_placements() {
        // DeepSeek reports hits beside the details object.
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":40}),
            &model("deepseek"),
        );
        assert_eq!((usage.input, usage.cache_read), (60, 40));

        // Kimi documents top-level cached_tokens on the final usage chunk.
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens":100,"completion_tokens":10,"cached_tokens":25}),
            &model("kimi-coding"),
        );
        assert_eq!((usage.input, usage.cache_read), (75, 25));

        // Nested details win over every fallback placement.
        let usage = parse_chunk_usage(
            &json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":7},"prompt_cache_hit_tokens":40,"cached_tokens":25}),
            &model("openai"),
        );
        assert_eq!((usage.input, usage.cache_read), (93, 7));
    }

    #[test]
    fn request_cache_and_compat_fields_are_exact() {
        let mut options = StreamOptions {
            max_tokens: Some(100),
            session_id: Some("session".into()),
            ..StreamOptions::default()
        };
        options.insert_extra(
            StreamOptionKey::TOOL_CHOICE,
            Value::String("required".into()),
        );
        let model = model("openai");
        let payload = build_payload(
            &model,
            &Context::default(),
            &options,
            &Compat::resolve(&model),
            CacheRetention::Long,
        );
        assert_eq!(payload["max_completion_tokens"], 100);
        assert_eq!(payload["prompt_cache_key"], "session");
        assert_eq!(payload["prompt_cache_retention"], "24h");
        assert_eq!(payload["stream_options"], json!({"include_usage":true}));
        assert_eq!(payload["store"], false);
        assert_eq!(payload["tool_choice"], "required");
    }
    #[test]
    fn stream_options_follow_usage_in_streaming_support() {
        let options = StreamOptions::default();
        // Present by default (reference treats unset as supported).
        let supported = model("openai");
        let payload = build_payload(
            &supported,
            &Context::default(),
            &options,
            &Compat::resolve(&supported),
            CacheRetention::None,
        );
        assert_eq!(payload["stream_options"], json!({"include_usage":true}));
        // Absent once catalog compat opts out.
        let mut unsupported = model("openai");
        unsupported.compat = Some(json!({"supportsUsageInStreaming":false}));
        let payload = build_payload(
            &unsupported,
            &Context::default(),
            &options,
            &Compat::resolve(&unsupported),
            CacheRetention::None,
        );
        assert_eq!(payload.get("stream_options"), None);
    }

    #[test]
    fn openrouter_omits_reasoning_when_off_level_is_null() {
        let mut model = model("openrouter");
        model.reasoning = true;
        model.thinking_level_map = Some([(ModelThinkingLevel::Off, None)].into_iter().collect());
        let payload = build_payload(
            &model,
            &Context::default(),
            &StreamOptions::default(),
            &Compat::resolve(&model),
            CacheRetention::None,
        );

        assert_eq!(payload.get("reasoning"), None);
    }

    #[test]
    fn openrouter_sends_none_for_optional_reasoning() {
        let mut model = model("openrouter");
        model.reasoning = true;
        model.thinking_level_map = Some(
            [(ModelThinkingLevel::Off, Some("none".into()))]
                .into_iter()
                .collect(),
        );
        let payload = build_payload(
            &model,
            &Context::default(),
            &StreamOptions::default(),
            &Compat::resolve(&model),
            CacheRetention::None,
        );

        assert_eq!(payload["reasoning"], json!({"effort":"none"}));
    }

    #[tokio::test]
    async fn indexed_tools_coalesce_and_missing_finish_reason_fails() -> Result<(), String> {
        let model = model("openai");
        let (mut processor, _stream) = processor(&model);
        processor
            .start()
            .await
            .map_err(|error| error.message.clone())?;
        processor
            .process_chunk(&json!({"id":"r","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{\"path\":"}}]}}]}))
            .await
            .map_err(|error| error.message.clone())?;
        processor
            .process_chunk(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_b","function":{"arguments":"\"x\"}"}}]}}]}))
            .await
            .map_err(|error| error.message.clone())?;
        assert_eq!(processor.state.message().content.len(), 1);
        let AssistantContent::ToolCall(tool) = &processor.state.message().content[0] else {
            return Err("expected tool call content".into());
        };
        assert_eq!(tool.id, "call_b");
        assert_eq!(tool.arguments.get("path"), Some(&Value::String("x".into())));
        let error = match processor.complete().await {
            Ok(()) => return Err("missing finish_reason must fail".into()),
            Err(error) => error,
        };
        assert_eq!(error.message, "Stream ended without finish_reason");
        Ok(())
    }

    #[tokio::test]
    async fn finish_reason_produces_exact_done_fields() -> Result<(), String> {
        let model = model("openai");
        let (sender, mut stream) = ProviderEventSender::channel(event_capacity());
        let mut processor = CompletionsProcessor::new(
            model.clone(),
            AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                1,
            ),
            sender,
        );
        processor
            .start()
            .await
            .map_err(|error| error.message.clone())?;
        processor
            .process_chunk(&json!({
                "id":"resp_1",
                "choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}
            }))
            .await
            .map_err(|error| error.message.clone())?;
        processor
            .complete()
            .await
            .map_err(|error| error.message.clone())?;
        drop(processor);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.map_err(|error| error.to_string())?);
        }
        let Some(AssistantMessageEvent::Done { reason, message }) = events.last() else {
            return Err("expected terminal done event".into());
        };
        assert_eq!(*reason, DoneReason::Stop);
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.error_message, None);
        assert_eq!(message.response_id.as_deref(), Some("resp_1"));
        assert_eq!(message.usage.input, 3);
        assert_eq!(message.usage.output, 1);
        assert_eq!(message.usage.total_tokens, 4);
        assert!(matches!(
            message.content.as_slice(),
            [AssistantContent::Text(text)] if text.text == "hi"
        ));
        Ok(())
    }

    #[test]
    fn done_marker_is_terminal_for_completions_decoder() -> Result<(), String> {
        let mut decoder = DataSseDecoder::default();
        let events = decoder
            .push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            .map_err(|error| error.to_string())?;
        assert!(matches!(
            events.as_slice(),
            [DataSseEvent::Data(_), DataSseEvent::Done]
        ));
        Ok(())
    }
}
