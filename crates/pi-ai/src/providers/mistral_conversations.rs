//! Native Mistral Conversations (`/v1/chat/completions`) adapter.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::Arc;

use futures::{StreamExt, stream::BoxStream};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::provider::{Provider, ProviderError, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ModelInput, StopReason, TextContent, ThinkingContent, ToolCall,
    ToolResultContent, UserContent, UserMessageContent,
};

use super::shared::{
    calculate_cost, parse_streaming_json, sanitize_surrogates, short_hash, transform_messages,
    truncate_error_body,
};
use super::stream_state::ProviderEventSender;
use super::transport::{DataSseDecoder, DataSseEvent, HttpTransport, TransportError};

const TOOL_CALL_ID_LENGTH: usize = 9;
const EVENT_CAPACITY: usize = 32;

/// Streams Mistral's OpenAI-compatible Conversations API without an SDK.
#[derive(Clone, Debug)]
pub struct MistralConversations {
    transport: HttpTransport,
}

impl MistralConversations {
    /// Construct an adapter around a configured HTTP client.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for MistralConversations {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let model = model.clone();
        let transport = self.transport.clone();
        let (sender, stream) = ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        tokio::spawn(async move {
            let mut output = AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            if sender.start(Arc::new(output.clone())).await.is_err() {
                return;
            }
            if let Err(failure) =
                run_request(&transport, &model, context, &options, &sender, &mut output).await
            {
                let reason = if failure.aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                output.stop_reason = if failure.aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.error_message = Some(failure.message);
                let _ = sender.error(reason, output).await;
            }
        });
        stream
    }
}

#[derive(Debug)]
struct StreamFailure {
    message: String,
    aborted: bool,
}

impl StreamFailure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    fn aborted() -> Self {
        Self {
            message: "Request was aborted".to_owned(),
            aborted: true,
        }
    }
}

async fn run_request(
    transport: &HttpTransport,
    model: &Model,
    context: Context,
    options: &StreamOptions,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
) -> Result<(), StreamFailure> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            StreamFailure::error(format!("No API key for provider: {}", model.provider))
        })?;

    let mut payload = build_payload(model, &context, options);
    if let Some(callback) = &options.on_payload {
        callback(&mut payload, model)
            .await
            .map_err(|error| StreamFailure::error(error.to_string()))?;
    }

    let url = format!(
        "{}/v1/chat/completions",
        model.base_url.trim_end_matches('/')
    );
    let mut builder = transport
        .post(&url)
        .headers(build_headers(model, options, api_key)?)
        .json(&payload);
    if let Some(timeout_ms) = options.timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout_ms));
    }
    let request = builder.build().map_err(|error| {
        StreamFailure::error(format!("failed to build Mistral request: {error}"))
    })?;

    let response = transport
        .execute(
            request,
            model,
            options.signal.as_ref(),
            options.on_response.as_ref(),
        )
        .await
        .map_err(map_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = match HttpTransport::read_error_body(response, options.signal.as_ref()).await {
            Ok(body) => body,
            Err(TransportError::Cancelled) => return Err(StreamFailure::aborted()),
            Err(error) => format!("failed to read error body: {error}"),
        };
        return Err(StreamFailure::error(format!(
            "Mistral API error ({}): {}",
            status.as_u16(),
            truncate_error_body(&body)
        )));
    }

    consume_response(response, options, sender, output, model).await
}

fn map_transport_error(error: TransportError) -> StreamFailure {
    match error {
        TransportError::Cancelled => StreamFailure::aborted(),
        TransportError::Request(error) | TransportError::Body(error) => {
            StreamFailure::error(format!("Mistral request failed: {error}"))
        }
        TransportError::Callback(error) => StreamFailure::error(error.to_string()),
    }
}

fn build_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<HeaderMap, StreamFailure> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|error| StreamFailure::error(format!("invalid API key header: {error}")))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header(&mut headers, name, value)?;
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| StreamFailure::error(format!("invalid header name: {error}")))?;
            if let Some(value) = value {
                headers.insert(
                    name,
                    HeaderValue::from_str(value).map_err(|error| {
                        StreamFailure::error(format!("invalid header value: {error}"))
                    })?,
                );
            } else {
                headers.remove(name);
            }
        }
    }
    if should_cache(options) && !headers.contains_key("x-affinity") {
        insert_header(
            &mut headers,
            "x-affinity",
            options.session_id.as_deref().unwrap_or_default(),
        )?;
    }
    Ok(headers)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), StreamFailure> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| StreamFailure::error(format!("invalid header name: {error}")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|error| StreamFailure::error(format!("invalid header value: {error}")))?;
    headers.insert(name, value);
    Ok(())
}

fn should_cache(options: &StreamOptions) -> bool {
    options.cache_retention != Some(CacheRetention::None)
        && options.session_id.as_ref().is_some_and(|id| !id.is_empty())
}

fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut id_map = MistralToolIdMap::default();
    let messages = transform_messages(&context.messages, model, |id, _, _| id_map.normalize(id));
    let mut payload = Map::new();
    payload.insert("model".into(), Value::String(model.id.clone()));
    payload.insert("stream".into(), Value::Bool(true));
    payload.insert(
        "messages".into(),
        Value::Array(to_chat_messages(
            &messages,
            model.input.contains(&ModelInput::Image),
        )),
    );
    if let Some(system) = context.system_prompt.as_deref() {
        let system = json!({"role": "system", "content": sanitize_surrogates(system)});
        if let Some(Value::Array(messages)) = payload.get_mut("messages") {
            messages.insert(0, system);
        }
    }
    if let Some(tools) = context.tools.as_ref().filter(|tools| !tools.is_empty()) {
        payload.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters,
                                "strict": false
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(temperature) = options.temperature.and_then(serde_json::Number::from_f64) {
        payload.insert("temperature".into(), Value::Number(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        payload.insert("max_tokens".into(), Value::Number(max_tokens.into()));
    }
    if let Some(choice) = options.extra.get("toolChoice") {
        payload.insert("tool_choice".into(), choice.clone());
    }
    apply_reasoning_options(&mut payload, model, options);
    if should_cache(options) {
        payload.insert(
            "prompt_cache_key".into(),
            Value::String(options.session_id.clone().unwrap_or_default()),
        );
    }
    Value::Object(payload)
}

fn apply_reasoning_options(
    payload: &mut Map<String, Value>,
    model: &Model,
    options: &StreamOptions,
) {
    if let Some(value) = options.extra.get("promptMode") {
        payload.insert("prompt_mode".into(), value.clone());
        return;
    }
    if let Some(value) = options.extra.get("reasoningEffort") {
        payload.insert("reasoning_effort".into(), value.clone());
        return;
    }
    let Some(level) = options.extra.get("reasoning").and_then(Value::as_str) else {
        return;
    };
    if !model.reasoning || matches!(level, "off" | "none") {
        return;
    }
    if uses_reasoning_effort(&model.id) {
        let mapped = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| {
                map.iter().find_map(|(key, value)| {
                    (serde_json::to_value(key).ok()?.as_str()? == level).then(|| value.clone())
                })
            })
            .flatten()
            .unwrap_or_else(|| "high".to_owned());
        payload.insert("reasoning_effort".into(), Value::String(mapped));
    } else {
        payload.insert("prompt_mode".into(), Value::String("reasoning".to_owned()));
    }
}

fn uses_reasoning_effort(model_id: &str) -> bool {
    matches!(
        model_id,
        "mistral-small-2603" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => push_user_message(&mut result, user, supports_images),
            Message::Assistant(assistant) => push_assistant_message(&mut result, assistant),
            Message::ToolResult(tool_result) => {
                push_tool_result_message(&mut result, tool_result, supports_images);
            }
        }
    }
    result
}

fn push_user_message(
    result: &mut Vec<Value>,
    user: &crate::types::UserMessage,
    supports_images: bool,
) {
    match &user.content {
        UserMessageContent::Text(text) => result.push(json!({
            "role": "user",
            "content": sanitize_surrogates(text)
        })),
        UserMessageContent::Blocks(blocks) => {
            let had_images = blocks
                .iter()
                .any(|part| matches!(part, UserContent::Image(_)));
            let content: Vec<_> = blocks
                .iter()
                .filter_map(|part| match part {
                    UserContent::Text(text) => Some(json!({
                        "type": "text", "text": sanitize_surrogates(&text.text)
                    })),
                    UserContent::Image(image) if supports_images => Some(json!({
                        "type": "image_url",
                        "image_url": format!("data:{};base64,{}", image.mime_type, image.data)
                    })),
                    UserContent::Image(_) => None,
                })
                .collect();
            if !content.is_empty() {
                result.push(json!({"role": "user", "content": content}));
            } else if had_images {
                result.push(json!({
                    "role": "user",
                    "content": "(image omitted: model does not support images)"
                }));
            }
        }
    }
}

fn push_assistant_message(result: &mut Vec<Value>, assistant: &AssistantMessage) {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &assistant.content {
        match block {
            AssistantContent::Text(text) if !text.text.trim().is_empty() => {
                content.push(json!({"type": "text", "text": sanitize_surrogates(&text.text)}));
            }
            AssistantContent::Thinking(thinking) if !thinking.thinking.trim().is_empty() => {
                content.push(json!({
                    "type": "thinking",
                    "thinking": [{"type": "text", "text": sanitize_surrogates(&thinking.thinking)}]
                }));
            }
            AssistantContent::ToolCall(call) => tool_calls.push(json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": Value::Object(call.arguments.clone()).to_string()
                }
            })),
            _ => {}
        }
    }
    if content.is_empty() && tool_calls.is_empty() {
        return;
    }
    let mut value = Map::new();
    value.insert("role".into(), Value::String("assistant".into()));
    if !content.is_empty() {
        value.insert("content".into(), Value::Array(content));
    }
    if !tool_calls.is_empty() {
        value.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    result.push(Value::Object(value));
}

fn push_tool_result_message(
    result: &mut Vec<Value>,
    tool_result: &crate::types::ToolResultMessage,
    supports_images: bool,
) {
    let text = tool_result
        .content
        .iter()
        .filter_map(|part| match part {
            ToolResultContent::Text(text) => Some(sanitize_surrogates(&text.text)),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let has_images = tool_result
        .content
        .iter()
        .any(|part| matches!(part, ToolResultContent::Image(_)));
    let mut content = vec![json!({
        "type": "text",
        "text": tool_result_text(&text, has_images, supports_images, tool_result.is_error)
    })];
    if supports_images {
        content.extend(tool_result.content.iter().filter_map(|part| match part {
            ToolResultContent::Image(image) => Some(json!({
                "type": "image_url",
                "image_url": format!("data:{};base64,{}", image.mime_type, image.data)
            })),
            ToolResultContent::Text(_) => None,
        }));
    }
    result.push(json!({
        "role": "tool",
        "tool_call_id": tool_result.tool_call_id,
        "name": tool_result.tool_name,
        "content": content
    }));
}

fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let trimmed = text.trim();
    let prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{prefix}{trimmed}{suffix}");
    }
    match (has_images, supports_images, is_error) {
        (true, true, true) => "[tool error] (see attached image)".to_owned(),
        (true, true, false) => "(see attached image)".to_owned(),
        (true, false, true) => {
            "[tool error] (image omitted: model does not support images)".to_owned()
        }
        (true, false, false) => "(image omitted: model does not support images)".to_owned(),
        (false, _, true) => "[tool error] (no tool output)".to_owned(),
        (false, _, false) => "(no tool output)".to_owned(),
    }
}

#[derive(Default)]
struct MistralToolIdMap {
    forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
}

impl MistralToolIdMap {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.forward.get(id) {
            return existing.clone();
        }
        for attempt in 0_u64.. {
            let candidate = derive_tool_call_id(id, attempt);
            if self.reverse.get(&candidate).is_none_or(|owner| owner == id) {
                self.forward.insert(id.to_owned(), candidate.clone());
                self.reverse.insert(candidate.clone(), id.to_owned());
                return candidate;
            }
        }
        unreachable!("the tool id hash space is not exhaustible")
    }
}

fn derive_tool_call_id(id: &str, attempt: u64) -> String {
    let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if attempt == 0 && normalized.len() == TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let base = if normalized.is_empty() {
        id
    } else {
        normalized.as_str()
    };
    let seed = if attempt == 0 {
        base.to_owned()
    } else {
        format!("{base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(TOOL_CALL_ID_LENGTH)
        .collect()
}

#[derive(Clone, Copy)]
enum OpenBlock {
    Text(u64),
    Thinking(u64),
}

#[derive(Default)]
struct ChunkState {
    open: Option<OpenBlock>,
    tool_indices: HashMap<u64, u64>,
    tool_args: HashMap<u64, String>,
}

async fn consume_response(
    response: reqwest::Response,
    options: &StreamOptions,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    model: &Model,
) -> Result<(), StreamFailure> {
    let mut decoder = DataSseDecoder::default();
    let mut body = response.bytes_stream();
    let mut chunks = ChunkState::default();
    let mut terminal = false;

    loop {
        let next = if let Some(signal) = &options.signal {
            tokio::select! {
                () = signal.cancelled() => return Err(StreamFailure::aborted()),
                item = body.next() => item,
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else { break };
        let chunk = chunk
            .map_err(|error| StreamFailure::error(format!("Mistral stream failed: {error}")))?;
        for event in decoder
            .push(&chunk)
            .map_err(|error| StreamFailure::error(error.to_string()))?
        {
            if process_sse_event(event, sender, output, model, &mut chunks).await? {
                terminal = true;
                break;
            }
        }
        if terminal {
            break;
        }
    }
    if !terminal {
        for event in decoder
            .finish()
            .map_err(|error| StreamFailure::error(error.to_string()))?
        {
            if process_sse_event(event, sender, output, model, &mut chunks).await? {
                terminal = true;
                break;
            }
        }
    }
    require_terminal(terminal)?;
    finish_open_block(sender, output, &mut chunks).await?;
    finish_tool_calls(sender, output, &mut chunks).await?;
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(StreamFailure::aborted());
    }
    let reason = match output.stop_reason {
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Stop => DoneReason::Stop,
        StopReason::Error => return Err(StreamFailure::error("An unknown error occurred")),
        StopReason::Aborted => return Err(StreamFailure::aborted()),
    };
    sender
        .done(reason, output.clone())
        .await
        .map_err(|error| StreamFailure::error(error.to_string()))
}

fn require_terminal(terminal: bool) -> Result<(), StreamFailure> {
    if terminal {
        Ok(())
    } else {
        Err(StreamFailure::error(
            "Mistral stream ended without a terminal event",
        ))
    }
}

async fn process_sse_event(
    event: DataSseEvent,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    model: &Model,
    chunks: &mut ChunkState,
) -> Result<bool, StreamFailure> {
    let DataSseEvent::Data(data) = event else {
        return Ok(true);
    };
    let chunk: Value = serde_json::from_str(&data)
        .map_err(|error| StreamFailure::error(format!("invalid Mistral SSE JSON: {error}")))?;
    if let Some(error) = chunk.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned);
        return Err(StreamFailure::error(format!(
            "Mistral API error: {message}"
        )));
    }
    process_chunk(&chunk, sender, output, model, chunks).await?;
    Ok(false)
}

async fn process_chunk(
    chunk: &Value,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    model: &Model,
    chunks: &mut ChunkState,
) -> Result<(), StreamFailure> {
    if output.response_id.is_none() {
        output.response_id = chunk
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
    }
    if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
        apply_usage(output, model, usage);
    }
    let Some(choice) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(());
    };
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        output.stop_reason = map_stop_reason(reason);
    }
    let Some(delta) = choice.get("delta") else {
        return Ok(());
    };
    if let Some(content) = delta.get("content").filter(|content| !content.is_null()) {
        let items: Vec<&Value> = content
            .as_array()
            .map_or_else(|| vec![content], |values| values.iter().collect());
        for item in items {
            if let Some(text) = item.as_str() {
                append_text(sender, output, chunks, text).await?;
            } else if item.get("type").and_then(Value::as_str) == Some("thinking") {
                let text = item
                    .get("thinking")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
                if !text.is_empty() {
                    append_thinking(sender, output, chunks, &text).await?;
                }
            } else if item.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = item.get("text").and_then(Value::as_str)
            {
                append_text(sender, output, chunks, text).await?;
            }
        }
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            finish_open_block(sender, output, chunks).await?;
            append_tool_call(sender, output, chunks, call).await?;
        }
    }
    Ok(())
}

async fn finish_open_block(
    sender: &ProviderEventSender,
    output: &AssistantMessage,
    chunks: &mut ChunkState,
) -> Result<(), StreamFailure> {
    let Some(open) = chunks.open.take() else {
        return Ok(());
    };
    let event = match open {
        OpenBlock::Text(index) => {
            let content = match content_block(output, index)? {
                AssistantContent::Text(text) => text.text.clone(),
                _ => return Err(StreamFailure::error("invalid Mistral text block state")),
            };
            AssistantMessageEvent::TextEnd {
                content_index: index,
                content,
                partial: Arc::new(output.clone()),
            }
        }
        OpenBlock::Thinking(index) => {
            let content = match content_block(output, index)? {
                AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
                _ => return Err(StreamFailure::error("invalid Mistral thinking block state")),
            };
            AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content,
                partial: Arc::new(output.clone()),
            }
        }
    };
    sender
        .event(event)
        .await
        .map_err(|error| StreamFailure::error(error.to_string()))
}

async fn append_text(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    chunks: &mut ChunkState,
    delta: &str,
) -> Result<(), StreamFailure> {
    if !matches!(chunks.open, Some(OpenBlock::Text(_))) {
        finish_open_block(sender, output, chunks).await?;
        output
            .content
            .push(AssistantContent::Text(TextContent::new("")));
        let index = last_content_index(output)?;
        chunks.open = Some(OpenBlock::Text(index));
        sender
            .event(AssistantMessageEvent::TextStart {
                content_index: index,
                partial: Arc::new(output.clone()),
            })
            .await
            .map_err(|error| StreamFailure::error(error.to_string()))?;
    }
    let Some(OpenBlock::Text(index)) = chunks.open else {
        return Err(StreamFailure::error("invalid Mistral text block state"));
    };
    if let Some(AssistantContent::Text(text)) = content_block_mut(output, index)? {
        text.text.push_str(delta);
    }
    sender
        .event(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: delta.to_owned(),
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| StreamFailure::error(error.to_string()))
}

async fn append_thinking(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    chunks: &mut ChunkState,
    delta: &str,
) -> Result<(), StreamFailure> {
    if !matches!(chunks.open, Some(OpenBlock::Thinking(_))) {
        finish_open_block(sender, output, chunks).await?;
        output
            .content
            .push(AssistantContent::Thinking(ThinkingContent::new("")));
        let index = last_content_index(output)?;
        chunks.open = Some(OpenBlock::Thinking(index));
        sender
            .event(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial: Arc::new(output.clone()),
            })
            .await
            .map_err(|error| StreamFailure::error(error.to_string()))?;
    }
    let Some(OpenBlock::Thinking(index)) = chunks.open else {
        return Err(StreamFailure::error("invalid Mistral thinking block state"));
    };
    if let Some(AssistantContent::Thinking(thinking)) = content_block_mut(output, index)? {
        thinking.thinking.push_str(delta);
    }
    sender
        .event(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: delta.to_owned(),
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| StreamFailure::error(error.to_string()))
}

async fn append_tool_call(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    chunks: &mut ChunkState,
    call: &Value,
) -> Result<(), StreamFailure> {
    let stream_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
    let function = call.get("function").unwrap_or(&Value::Null);
    let content_index = if let Some(index) = chunks.tool_indices.get(&stream_index).copied() {
        index
    } else {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| *id != "null" && !id.is_empty())
            .map_or_else(
                || derive_tool_call_id(&format!("toolcall:{stream_index}"), 0),
                str::to_owned,
            );
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        output
            .content
            .push(AssistantContent::ToolCall(ToolCall::new(
                &id,
                name,
                Map::new(),
            )));
        let index = last_content_index(output)?;
        chunks.tool_indices.insert(stream_index, index);
        chunks.tool_args.insert(index, String::new());
        sender
            .event(AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial: Arc::new(output.clone()),
            })
            .await
            .map_err(|error| StreamFailure::error(error.to_string()))?;
        index
    };
    if let Some(name) = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        && let Some(AssistantContent::ToolCall(tool)) = content_block_mut(output, content_index)?
    {
        name.clone_into(&mut tool.name);
    }
    let args_delta = match function.get("arguments") {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(arguments) if !arguments.is_null() => arguments.to_string(),
        _ => String::new(),
    };
    chunks
        .tool_args
        .entry(content_index)
        .or_default()
        .push_str(&args_delta);
    sender
        .event(AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta: args_delta,
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| StreamFailure::error(error.to_string()))
}

async fn finish_tool_calls(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    chunks: &mut ChunkState,
) -> Result<(), StreamFailure> {
    let mut indices: Vec<_> = chunks.tool_args.keys().copied().collect();
    indices.sort_unstable();
    for index in indices {
        let arguments =
            parse_streaming_json(chunks.tool_args.get(&index).map_or("", String::as_str));
        let Some(AssistantContent::ToolCall(tool)) = content_block_mut(output, index)? else {
            return Err(StreamFailure::error("invalid Mistral tool block state"));
        };
        tool.arguments = arguments;
        let tool = tool.clone();
        sender
            .event(AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call: tool,
                partial: Arc::new(output.clone()),
            })
            .await
            .map_err(|error| StreamFailure::error(error.to_string()))?;
    }
    Ok(())
}

fn apply_usage(output: &mut AssistantMessage, model: &Model, usage: &Value) {
    let prompt_tokens = number(usage, &["prompt_tokens", "promptTokens"]);
    let cached = cached_tokens(usage).min(prompt_tokens);
    output.usage.input = prompt_tokens.saturating_sub(cached);
    output.usage.output = number(usage, &["completion_tokens", "completionTokens"]);
    output.usage.cache_read = cached;
    output.usage.cache_write = 0;
    let reported_total = number(usage, &["total_tokens", "totalTokens"]);
    output.usage.total_tokens = if reported_total == 0 {
        output.usage.input + output.usage.output + output.usage.cache_read
    } else {
        reported_total
    };
    calculate_cost(model, &mut output.usage);
}

fn cached_tokens(usage: &Value) -> u64 {
    for path in [
        &["promptTokensDetails", "cachedTokens"][..],
        &["prompt_tokens_details", "cached_tokens"],
        &["promptTokenDetails", "cachedTokens"],
        &["prompt_token_details", "cached_tokens"],
    ] {
        if let Some(value) = usage
            .get(path[0])
            .and_then(|details| details.get(path[1]))
            .and_then(Value::as_u64)
        {
            return value;
        }
    }
    number(usage, &["numCachedTokens", "num_cached_tokens"])
}

fn number(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "length" | "model_length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        "error" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

fn unix_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn content_index(index: u64) -> Result<usize, StreamFailure> {
    usize::try_from(index).map_err(|_| StreamFailure::error("content index overflow"))
}

fn last_content_index(output: &AssistantMessage) -> Result<u64, StreamFailure> {
    let len = output.content.len();
    let index = len
        .checked_sub(1)
        .ok_or_else(|| StreamFailure::error("invalid Mistral content state"))?;
    u64::try_from(index).map_err(|_| StreamFailure::error("content index overflow"))
}

fn content_block(
    output: &AssistantMessage,
    index: u64,
) -> Result<&AssistantContent, StreamFailure> {
    output
        .content
        .get(content_index(index)?)
        .ok_or_else(|| StreamFailure::error("invalid Mistral content state"))
}

fn content_block_mut(
    output: &mut AssistantMessage,
    index: u64,
) -> Result<Option<&mut AssistantContent>, StreamFailure> {
    Ok(output.content.get_mut(content_index(index)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, Tool, UserMessage};
    use futures::StreamExt;
    use std::collections::BTreeMap;

    fn model(id: &str, reasoning: bool) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: "mistral-conversations".to_owned(),
            provider: "mistral".to_owned(),
            base_url: "http://127.0.0.1:1".to_owned(),
            reasoning,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 2.0,
                output: 4.0,
                cache_read: 1.0,
                cache_write: 0.0,
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn tool_ids_are_exactly_nine_alphanumeric_and_collisions_are_bidirectional() {
        let mut ids = MistralToolIdMap::default();
        let first = ids.normalize("abc-defghi");
        let second = ids.normalize("abcdefghi");
        assert_eq!(first, "abcdefghi");
        assert_ne!(first, second);
        assert_eq!(second.len(), 9);
        assert!(second.chars().all(|ch| ch.is_ascii_alphanumeric()));
        assert_eq!(ids.normalize("abc-defghi"), first);
        assert_eq!(
            ids.reverse.get(&first).map(String::as_str),
            Some("abc-defghi")
        );
        assert_eq!(
            ids.reverse.get(&second).map(String::as_str),
            Some("abcdefghi")
        );
    }

    #[test]
    fn request_conversion_maps_tools_reasoning_and_affinity_cache() {
        let mut context = Context {
            system_prompt: Some("system".to_owned()),
            messages: vec![Message::User(UserMessage::new(
                UserMessageContent::Text("hello".to_owned()),
                1,
            ))],
            tools: Some(vec![Tool {
                name: "read".to_owned(),
                description: "Read a file".to_owned(),
                parameters: json!({"type": "object"}),
            }]),
        };
        let mut options = StreamOptions {
            max_tokens: Some(123),
            session_id: Some("session-1".to_owned()),
            cache_retention: Some(CacheRetention::Short),
            ..StreamOptions::default()
        };
        options
            .extra
            .insert("reasoning".into(), Value::String("high".into()));
        let payload = build_payload(&model("mistral-small-latest", true), &context, &options);
        assert_eq!(payload["max_tokens"], 123);
        assert_eq!(payload["reasoning_effort"], "high");
        assert!(payload.get("prompt_mode").is_none());
        assert_eq!(payload["prompt_cache_key"], "session-1");
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["tools"][0]["function"]["strict"], false);

        options.extra.clear();
        options
            .extra
            .insert("reasoning".into(), Value::String("high".into()));
        context.tools = None;
        let payload = build_payload(&model("magistral-medium", true), &context, &options);
        assert_eq!(payload["prompt_mode"], "reasoning");
        assert!(payload.get("reasoning_effort").is_none());

        let headers = match build_headers(&model("x", false), &options, "key") {
            Ok(headers) => headers,
            Err(_) => HeaderMap::new(),
        };
        assert_eq!(
            headers
                .get("x-affinity")
                .and_then(|value| value.to_str().ok()),
            Some("session-1")
        );
    }

    #[test]
    fn usage_subtracts_all_cached_input_variants_and_uses_reported_total() {
        let mut output = AssistantMessage::new("mistral-conversations", "mistral", "x", 1);
        let model = model("x", false);
        apply_usage(
            &mut output,
            &model,
            &json!({
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 40}
            }),
        );
        assert_eq!(output.usage.input, 60);
        assert_eq!(output.usage.cache_read, 40);
        assert_eq!(output.usage.output, 20);
        assert_eq!(output.usage.total_tokens, 120);
        assert!((output.usage.cost.input - 0.000_12).abs() < f64::EPSILON);
        assert!((output.usage.cost.output - 0.000_08).abs() < f64::EPSILON);
        assert!((output.usage.cost.cache_read - 0.000_04).abs() < f64::EPSILON);
        assert!((output.usage.cost.total - 0.000_24).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn later_tool_deltas_without_ids_reuse_the_stream_index() {
        let (sender, mut stream) =
            ProviderEventSender::channel(NonZeroUsize::new(8).unwrap_or(NonZeroUsize::MIN));
        let mut output = AssistantMessage::new("mistral-conversations", "mistral", "x", 1);
        let _ = sender.start(Arc::new(output.clone())).await;
        let mut chunks = ChunkState::default();
        let first = append_tool_call(
            &sender,
            &mut output,
            &mut chunks,
            &json!({
                "index": 0,
                "id": "abcdefghi",
                "function": {"name": "read", "arguments": "{\"path\":"}
            }),
        )
        .await;
        assert!(first.is_ok());
        let second = append_tool_call(
            &sender,
            &mut output,
            &mut chunks,
            &json!({"index": 0, "function": {"arguments": "\"file\"}"}}),
        )
        .await;
        assert!(second.is_ok());
        let finished = finish_tool_calls(&sender, &mut output, &mut chunks).await;
        assert!(finished.is_ok());
        drop(sender);
        while stream.next().await.is_some() {}
        let calls: Vec<_> = output
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "abcdefghi");
        assert_eq!(
            calls[0].arguments,
            json!({"path": "file"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn eof_without_done_is_one_error_terminal_after_start() {
        let (sender, mut stream) =
            ProviderEventSender::channel(NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN));
        let output = AssistantMessage::new("mistral-conversations", "mistral", "x", 1);
        let _ = sender.start(Arc::new(output.clone())).await;
        let Err(missing) = require_terminal(false) else {
            unreachable!("EOF must be rejected");
        };
        assert_eq!(
            missing.message,
            "Mistral stream ended without a terminal event"
        );
        let mut decoder = DataSseDecoder::default();
        let events = decoder
            .push(b"data: {\"choices\":[]}\n\n")
            .unwrap_or_default();
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, DataSseEvent::Done))
        );
        let mut failed = output;
        failed.stop_reason = StopReason::Error;
        failed.error_message = Some(missing.message);
        let _ = sender.error(ErrorReason::Error, failed).await;
        drop(sender);
        let events: Vec<_> = stream.by_ref().collect().await;
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        assert!(matches!(
            events.last(),
            Some(Ok(AssistantMessageEvent::Error { .. }))
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(AssistantMessageEvent::Error { .. })))
                .count(),
            1
        );
    }
}
