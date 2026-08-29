//! Native Anthropic Messages HTTP and streaming adapter.

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use reqwest::{Client, Request};
use serde_json::{Map, Value, json};

use crate::provider::{Provider, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ModelThinkingLevel, StopReason, ThinkingContent, ToolCall,
    ToolResultContent, UserContent, UserMessageContent,
};

use super::shared::{
    calculate_cost, parse_streaming_json, sanitize_surrogates, truncate_error_body,
};
use super::stream_state::{ProviderEventSender, ProviderEventStream};
use super::transport::{HttpTransport, SseLineBuffer, TransportError};

const STREAM_CAPACITY: NonZeroUsize = match NonZeroUsize::new(32) {
    Some(capacity) => capacity,
    None => NonZeroUsize::MIN,
};
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MESSAGE_STOP_MISSING: &str = "Anthropic stream ended before message_stop";

/// Anthropic's native Messages API adapter.
#[derive(Clone, Debug)]
pub struct AnthropicMessages {
    transport: HttpTransport,
    client: Client,
}

impl AnthropicMessages {
    /// Create an adapter using an already configured HTTP client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client.clone()),
            client,
        }
    }
}

impl Provider for AnthropicMessages {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> ProviderEventStream {
        let model = model.clone();
        let transport = self.transport.clone();
        let client = self.client.clone();
        let (sender, stream) = ProviderEventSender::channel(STREAM_CAPACITY);
        tokio::spawn(async move {
            let mut assembler = StreamAssembler::new(&model);
            if sender
                .start(Arc::new(assembler.message.clone()))
                .await
                .is_err()
            {
                return;
            }
            if let Err(error) = run_stream(
                &client,
                &transport,
                &model,
                context,
                &options,
                &sender,
                &mut assembler,
            )
            .await
            {
                let reason = classify_error(&error, options.signal.as_ref());
                assembler.message.stop_reason = match reason {
                    ErrorReason::Aborted => StopReason::Aborted,
                    ErrorReason::Error => StopReason::Error,
                };
                assembler.message.error_message = Some(error.to_string());
                let _ = sender.error(reason, assembler.message).await;
            }
        });
        stream
    }
}

async fn run_stream(
    client: &Client,
    transport: &HttpTransport,
    model: &Model,
    context: Context,
    options: &StreamOptions,
    sender: &ProviderEventSender,
    assembler: &mut StreamAssembler,
) -> Result<(), AdapterError> {
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(AdapterError::Cancelled);
    }

    let mut payload = build_payload(model, &context, options);
    if let Some(callback) = &options.on_payload {
        callback(&mut payload, model)
            .await
            .map_err(|error| AdapterError::Callback(error.to_string()))?;
    }
    let request = build_request(client, model, options, &payload)?;
    let response = transport
        .execute(
            request,
            model,
            options.signal.as_ref(),
            options.on_response.as_ref(),
        )
        .await
        .map_err(AdapterError::Transport)?;

    let status = response.status();
    if !status.is_success() {
        let body = HttpTransport::read_error_body(response, options.signal.as_ref())
            .await
            .map_err(AdapterError::Transport)?;
        return Err(AdapterError::Http {
            status: status.as_u16(),
            body: truncate_error_body(&body),
        });
    }

    let mut body = response.bytes_stream();
    let mut decoder = AnthropicSseDecoder::default();
    loop {
        let next = if let Some(signal) = options.signal.as_ref() {
            tokio::select! {
                () = signal.cancelled() => return Err(AdapterError::Cancelled),
                next = body.next() => next,
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(AdapterError::Body)?;
        for event in decoder.push(&chunk)? {
            assembler.apply(event, sender).await?;
        }
    }
    for event in decoder.finish()? {
        assembler.apply(event, sender).await?;
    }
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(AdapterError::Cancelled);
    }
    require_message_stop(assembler)?;
    if assembler.message.stop_reason == StopReason::Error {
        return Err(AdapterError::Protocol(
            assembler
                .message
                .error_message
                .clone()
                .unwrap_or_else(|| "Anthropic returned an error stop reason".to_owned()),
        ));
    }

    let reason = match assembler.message.stop_reason {
        StopReason::Stop => DoneReason::Stop,
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Error | StopReason::Aborted => {
            return Err(AdapterError::Protocol(
                "invalid Anthropic terminal state".to_owned(),
            ));
        }
    };
    sender
        .done(reason, assembler.message.clone())
        .await
        .map_err(|error| AdapterError::Delivery(error.to_string()))
}

fn require_message_stop(assembler: &StreamAssembler) -> Result<(), AdapterError> {
    if assembler.saw_message_stop {
        Ok(())
    } else {
        Err(AdapterError::Protocol(MESSAGE_STOP_MISSING.to_owned()))
    }
}

fn build_request(
    client: &Client,
    model: &Model,
    options: &StreamOptions,
    payload: &Value,
) -> Result<Request, AdapterError> {
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
    let mut request = ClientRequest::new(url, payload.clone());
    request.header("anthropic-version", Some(ANTHROPIC_VERSION));
    request.header("content-type", Some("application/json"));
    if let Some(api_key) = options.api_key.as_deref() {
        request.header("x-api-key", Some(api_key));
    }
    if let Some(headers) = &model.headers {
        for (name, value) in headers {
            request.header(name, Some(value));
        }
    }
    if let Some(headers) = &options.headers {
        for (name, value) in headers {
            request.header(name, value.as_deref());
        }
    }
    request.build(client, options.timeout_ms)
}

struct ClientRequest {
    url: String,
    payload: Value,
    headers: BTreeMap<String, Option<String>>,
}

impl ClientRequest {
    fn new(url: String, payload: Value) -> Self {
        Self {
            url,
            payload,
            headers: BTreeMap::new(),
        }
    }

    fn header(&mut self, name: &str, value: Option<&str>) {
        self.headers
            .insert(name.to_ascii_lowercase(), value.map(str::to_owned));
    }

    fn build(self, client: &Client, timeout_ms: Option<u64>) -> Result<Request, AdapterError> {
        let mut builder = client.post(self.url).json(&self.payload);
        for (name, value) in self.headers {
            if let Some(value) = value {
                builder = builder.header(name, value);
            }
        }
        if let Some(timeout_ms) = timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        }
        builder.build().map_err(AdapterError::RequestBuild)
    }
}

fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let cache_control = cache_control(model, options);
    let mut payload = Map::new();
    payload.insert("model".to_owned(), Value::String(model.id.clone()));
    payload.insert(
        "messages".to_owned(),
        Value::Array(convert_messages(model, context, cache_control.as_ref())),
    );
    payload.insert(
        "max_tokens".to_owned(),
        Value::from(options.max_tokens.unwrap_or(model.max_tokens)),
    );
    payload.insert("stream".to_owned(), Value::Bool(true));

    if let Some(system) = context.system_prompt.as_deref() {
        let mut block = json!({ "type": "text", "text": sanitize_surrogates(system) });
        if let Some(cache) = &cache_control {
            block["cache_control"] = cache.clone();
        }
        payload.insert("system".to_owned(), Value::Array(vec![block]));
    }

    insert_temperature(&mut payload, model, options);
    insert_thinking(&mut payload, model, options);
    insert_tools(&mut payload, model, context, cache_control.as_ref());

    if let Some(metadata) = &options.metadata
        && let Some(user_id) = metadata.get("user_id").and_then(Value::as_str)
    {
        payload.insert("metadata".to_owned(), json!({ "user_id": user_id }));
    }
    if let Some(tool_choice) = options.extra.get("toolChoice") {
        payload.insert(
            "tool_choice".to_owned(),
            if let Some(kind) = tool_choice.as_str() {
                json!({ "type": kind })
            } else {
                tool_choice.clone()
            },
        );
    }
    Value::Object(payload)
}

fn insert_temperature(payload: &mut Map<String, Value>, model: &Model, options: &StreamOptions) {
    let thinking_enabled = extra_bool(options, "thinkingEnabled");
    if let Some(temperature) = options.temperature
        && thinking_enabled != Some(true)
        && compat_bool(model, "supportsTemperature", true)
        && let Some(number) = serde_json::Number::from_f64(temperature)
    {
        payload.insert("temperature".to_owned(), Value::Number(number));
    }
}

fn insert_thinking(payload: &mut Map<String, Value>, model: &Model, options: &StreamOptions) {
    if !model.reasoning {
        return;
    }
    match extra_bool(options, "thinkingEnabled") {
        Some(true) => {
            let display = options
                .extra
                .get("thinkingDisplay")
                .and_then(Value::as_str)
                .unwrap_or("summarized");
            if compat_bool(model, "forceAdaptiveThinking", false) {
                payload.insert(
                    "thinking".to_owned(),
                    json!({ "type": "adaptive", "display": display }),
                );
                if let Some(effort) = options.extra.get("effort").and_then(Value::as_str) {
                    payload.insert("output_config".to_owned(), json!({ "effort": effort }));
                }
            } else {
                let budget = options
                    .extra
                    .get("thinkingBudgetTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(1024);
                payload.insert(
                    "thinking".to_owned(),
                    json!({ "type": "enabled", "budget_tokens": budget, "display": display }),
                );
            }
        }
        Some(false) => {
            // Upstream omits the explicit disable when the model's thinking
            // level map pins `off` to null (adaptive-only models).
            let off_is_null = model
                .thinking_level_map
                .as_ref()
                .is_some_and(|map| map.get(&ModelThinkingLevel::Off) == Some(&None));
            if !off_is_null {
                payload.insert("thinking".to_owned(), json!({ "type": "disabled" }));
            }
        }
        None => {}
    }
}

fn insert_tools(
    payload: &mut Map<String, Value>,
    model: &Model,
    context: &Context,
    cache_control: Option<&Value>,
) {
    let Some(tools) = context.tools.as_deref() else {
        return;
    };
    if tools.is_empty() {
        return;
    }

    let mut converted: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut input_schema = tool.parameters.clone();
            if !input_schema.is_object() {
                input_schema = json!({});
            }
            if let Some(schema) = input_schema.as_object_mut() {
                schema
                    .entry("type")
                    .or_insert_with(|| Value::String("object".to_owned()));
                schema.entry("properties").or_insert_with(|| json!({}));
                schema.entry("required").or_insert_with(|| json!([]));
            }
            let mut value = json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": input_schema,
            });
            if compat_bool(model, "supportsEagerToolInputStreaming", true) {
                value["eager_input_streaming"] = Value::Bool(true);
            }
            value
        })
        .collect();
    if compat_bool(model, "supportsCacheControlOnTools", true)
        && let (Some(last), Some(cache)) = (converted.last_mut(), cache_control)
    {
        last["cache_control"] = cache.clone();
    }
    payload.insert("tools".to_owned(), Value::Array(converted));
}

fn cache_control(model: &Model, options: &StreamOptions) -> Option<Value> {
    let retention = options.cache_retention.unwrap_or_else(|| {
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
    });
    match retention {
        CacheRetention::None => None,
        CacheRetention::Long if compat_bool(model, "supportsLongCacheRetention", true) => {
            Some(json!({ "type": "ephemeral", "ttl": "1h" }))
        }
        CacheRetention::Short | CacheRetention::Long => Some(json!({ "type": "ephemeral" })),
    }
}

fn compat_bool(model: &Model, key: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn extra_bool(options: &StreamOptions, key: &str) -> Option<bool> {
    options.extra.get(key).and_then(Value::as_bool)
}

fn convert_messages(model: &Model, context: &Context, cache: Option<&Value>) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut index = 0;
    while index < context.messages.len() {
        match &context.messages[index] {
            Message::User(message) => {
                if let Some(content) = convert_user_content(&message.content) {
                    messages.push(json!({ "role": "user", "content": content }));
                }
                index += 1;
            }
            Message::Assistant(message) => {
                let blocks = convert_assistant_content(
                    model,
                    message,
                    compat_bool(model, "allowEmptySignature", false),
                );
                if !blocks.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
                index += 1;
            }
            Message::ToolResult(_) => {
                let mut blocks = Vec::new();
                while let Some(Message::ToolResult(message)) = context.messages.get(index) {
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": normalize_tool_call_id(&message.tool_call_id),
                        "content": convert_tool_result_content(&message.content),
                        "is_error": message.is_error,
                    }));
                    index += 1;
                }
                messages.push(json!({ "role": "user", "content": blocks }));
            }
        }
    }

    if let (Some(cache), Some(last)) = (cache, messages.last_mut())
        && last.get("role").and_then(Value::as_str) == Some("user")
    {
        let last_content = &mut last["content"];
        if let Some(blocks) = last_content.as_array_mut() {
            if let Some(block) = blocks.last_mut()
                && matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("text" | "image" | "tool_result")
                )
            {
                block["cache_control"] = cache.clone();
            }
        } else if let Some(text) = last_content.as_str().map(str::to_owned) {
            *last_content = json!([{ "type": "text", "text": text, "cache_control": cache }]);
        }
    }
    messages
}

fn convert_user_content(content: &UserMessageContent) -> Option<Value> {
    match content {
        UserMessageContent::Text(text) if text.trim().is_empty() => None,
        UserMessageContent::Text(text) => {
            Some(Value::String(sanitize_surrogates(text).into_owned()))
        }
        UserMessageContent::Blocks(blocks) => {
            let mut converted = Vec::new();
            for block in blocks {
                match block {
                    UserContent::Text(text) if !text.text.trim().is_empty() => {
                        converted.push(
                            json!({ "type": "text", "text": sanitize_surrogates(&text.text) }),
                        );
                    }
                    UserContent::Image(image) => converted.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": image.mime_type,
                            "data": image.data,
                        }
                    })),
                    UserContent::Text(_) => {}
                }
            }
            (!converted.is_empty()).then_some(Value::Array(converted))
        }
    }
}

fn convert_assistant_content(
    model: &Model,
    message: &AssistantMessage,
    allow_empty_signature: bool,
) -> Vec<Value> {
    let same_model =
        message.provider == model.provider && message.api == model.api && message.model == model.id;
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) if text.text.trim().is_empty() => None,
            AssistantContent::Text(text) => Some(json!({
                "type": "text",
                "text": sanitize_surrogates(&text.text),
            })),
            AssistantContent::Thinking(thinking) if thinking.redacted == Some(true) => same_model
                .then(|| {
                    json!({
                        "type": "redacted_thinking",
                        "data": thinking.thinking_signature.as_deref().unwrap_or_default(),
                    })
                }),
            AssistantContent::Thinking(thinking) if !same_model => {
                (!thinking.thinking.trim().is_empty()).then(
                    || json!({ "type": "text", "text": sanitize_surrogates(&thinking.thinking) }),
                )
            }
            AssistantContent::Thinking(thinking) => {
                let signature = thinking.thinking_signature.as_deref().unwrap_or_default();
                if thinking.thinking.trim().is_empty() && signature.trim().is_empty() {
                    None
                } else if signature.trim().is_empty() && !allow_empty_signature {
                    Some(json!({ "type": "text", "text": sanitize_surrogates(&thinking.thinking) }))
                } else {
                    Some(json!({
                        "type": "thinking",
                        "thinking": sanitize_surrogates(&thinking.thinking),
                        "signature": signature,
                    }))
                }
            }
            AssistantContent::ToolCall(tool) => Some(json!({
                "type": "tool_use",
                "id": normalize_tool_call_id(&tool.id),
                "name": tool.name,
                "input": tool.arguments,
            })),
        })
        .collect()
}

fn convert_tool_result_content(content: &[ToolResultContent]) -> Value {
    let has_images = content
        .iter()
        .any(|block| matches!(block, ToolResultContent::Image(_)));
    if !has_images {
        return Value::String(
            content
                .iter()
                .filter_map(|block| match block {
                    ToolResultContent::Text(text) => Some(text.text.as_str()),
                    ToolResultContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut blocks = Vec::new();
    if !content
        .iter()
        .any(|block| matches!(block, ToolResultContent::Text(_)))
    {
        blocks.push(json!({ "type": "text", "text": "(see attached image)" }));
    }
    for block in content {
        match block {
            ToolResultContent::Text(text) => {
                blocks.push(json!({ "type": "text", "text": text.text }));
            }
            ToolResultContent::Image(image) => blocks.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": image.mime_type, "data": image.data },
            })),
        }
    }
    Value::Array(blocks)
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerSentEvent {
    event: Option<String>,
    data: String,
    raw: Vec<String>,
}

#[derive(Debug, Default)]
struct AnthropicSseDecoder {
    lines: SseLineBuffer,
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl AnthropicSseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<RawEvent>, AdapterError> {
        let mut output = Vec::new();
        for line in self.lines.push(chunk) {
            if let Some(event) = self.decode_line(&line)?
                && let Some(event) = decode_anthropic_event(event)?
            {
                output.push(event);
            }
        }
        Ok(output)
    }

    fn finish(&mut self) -> Result<Vec<RawEvent>, AdapterError> {
        let mut output = Vec::new();
        for line in self.lines.finish() {
            if let Some(event) = self.decode_line(&line)?
                && let Some(event) = decode_anthropic_event(event)?
            {
                output.push(event);
            }
        }
        if let Some(event) = self.flush()
            && let Some(event) = decode_anthropic_event(event)?
        {
            output.push(event);
        }
        Ok(output)
    }

    fn decode_line(&mut self, line: &[u8]) -> Result<Option<ServerSentEvent>, AdapterError> {
        let line =
            std::str::from_utf8(line).map_err(|error| AdapterError::Sse(error.to_string()))?;
        if line.is_empty() {
            return Ok(self.flush());
        }
        self.raw.push(line.to_owned());
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, mut value) = line.split_once(':').unwrap_or((line, ""));
        if let Some(stripped) = value.strip_prefix(' ') {
            value = stripped;
        }
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        Ok(None)
    }

    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            self.raw.clear();
            return None;
        }
        Some(ServerSentEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
            raw: std::mem::take(&mut self.raw),
        })
    }
}

fn decode_anthropic_event(event: ServerSentEvent) -> Result<Option<RawEvent>, AdapterError> {
    let Some(name) = event.event.as_deref() else {
        return Ok(None);
    };
    if name == "error" {
        return Err(AdapterError::Protocol(event.data));
    }
    if !matches!(
        name,
        "message_start"
            | "message_delta"
            | "message_stop"
            | "content_block_start"
            | "content_block_delta"
            | "content_block_stop"
    ) {
        return Ok(None);
    }

    let value = repair_json(&event.data).map_err(|message| {
        AdapterError::Sse(format!(
            "Could not parse Anthropic SSE event {name}: {message}; data={}; raw={}",
            event.data,
            event.raw.join("\n")
        ))
    })?;
    let actual = value.get("type").and_then(Value::as_str);
    if actual != Some(name) {
        return Ok(None);
    }
    Ok(Some(RawEvent {
        kind: name.to_owned(),
        value,
    }))
}

fn repair_json(data: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str(data) {
        return Ok(value);
    }
    let object = parse_streaming_json(data);
    if object.is_empty() && data.trim() != "{}" {
        return Err("invalid JSON object".to_owned());
    }
    Ok(Value::Object(object))
}

#[derive(Clone, Debug)]
struct RawEvent {
    kind: String,
    value: Value,
}

struct StreamAssembler {
    message: AssistantMessage,
    model: Model,
    provider_blocks: HashMap<u64, u64>,
    partial_json: HashMap<u64, String>,
    saw_message_stop: bool,
}

impl StreamAssembler {
    fn new(model: &Model) -> Self {
        Self {
            message: AssistantMessage::new(&model.api, &model.provider, &model.id, now_millis()),
            model: model.clone(),
            provider_blocks: HashMap::new(),
            partial_json: HashMap::new(),
            saw_message_stop: false,
        }
    }

    async fn apply(
        &mut self,
        event: RawEvent,
        sender: &ProviderEventSender,
    ) -> Result<(), AdapterError> {
        match event.kind.as_str() {
            "message_start" => self.message_start(&event.value),
            "content_block_start" => {
                if let Some(event) = self.content_start(&event.value)? {
                    sender.event(event).await.map_err(delivery_error)?;
                }
            }
            "content_block_delta" => {
                if let Some(event) = self.content_delta(&event.value)? {
                    sender.event(event).await.map_err(delivery_error)?;
                }
            }
            "content_block_stop" => {
                if let Some(event) = self.content_stop(&event.value)? {
                    sender.event(event).await.map_err(delivery_error)?;
                }
            }
            "message_delta" => self.message_delta(&event.value),
            "message_stop" => self.saw_message_stop = true,
            _ => {}
        }
        Ok(())
    }

    fn message_start(&mut self, value: &Value) {
        let message = &value["message"];
        self.message.response_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
        update_usage(&self.model, &mut self.message, &message["usage"]);
        if self.message.usage.cache_write1h.is_none() {
            self.message.usage.cache_write1h = Some(0);
        }
    }

    fn content_start(
        &mut self,
        value: &Value,
    ) -> Result<Option<AssistantMessageEvent>, AdapterError> {
        let provider_index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            AdapterError::Protocol("Anthropic content_block_start missing index".to_owned())
        })?;
        let block = &value["content_block"];
        let event = match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                self.message
                    .content
                    .push(AssistantContent::Text(crate::types::TextContent::new("")));
                AssistantMessageEvent::TextStart {
                    content_index: self.last_index()?,
                    partial: Arc::new(self.message.clone()),
                }
            }
            Some("thinking") => {
                self.message
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent::new("")));
                AssistantMessageEvent::ThinkingStart {
                    content_index: self.last_index()?,
                    partial: Arc::new(self.message.clone()),
                }
            }
            Some("redacted_thinking") => {
                let mut thinking = ThinkingContent::new("[Reasoning redacted]");
                thinking.thinking_signature =
                    block.get("data").and_then(Value::as_str).map(str::to_owned);
                thinking.redacted = Some(true);
                self.message
                    .content
                    .push(AssistantContent::Thinking(thinking));
                AssistantMessageEvent::ThinkingStart {
                    content_index: self.last_index()?,
                    partial: Arc::new(self.message.clone()),
                }
            }
            Some("tool_use") => {
                let arguments = block
                    .get("input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                self.message
                    .content
                    .push(AssistantContent::ToolCall(ToolCall::new(
                        block.get("id").and_then(Value::as_str).unwrap_or_default(),
                        block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        arguments,
                    )));
                self.partial_json.insert(provider_index, String::new());
                AssistantMessageEvent::ToolCallStart {
                    content_index: self.last_index()?,
                    partial: Arc::new(self.message.clone()),
                }
            }
            _ => return Ok(None),
        };
        self.provider_blocks
            .insert(provider_index, self.last_index()?);
        Ok(Some(event))
    }

    fn content_delta(
        &mut self,
        value: &Value,
    ) -> Result<Option<AssistantMessageEvent>, AdapterError> {
        let provider_index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            AdapterError::Protocol("Anthropic content_block_delta missing index".to_owned())
        })?;
        let Some(&content_index) = self.provider_blocks.get(&provider_index) else {
            return Ok(None);
        };
        let delta = &value["delta"];
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let AssistantContent::Text(block) = self.content_mut(content_index)? else {
                    return Ok(None);
                };
                block.text.push_str(text);
                Ok(Some(AssistantMessageEvent::TextDelta {
                    content_index,
                    delta: text.to_owned(),
                    partial: Arc::new(self.message.clone()),
                }))
            }
            Some("thinking_delta") => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let AssistantContent::Thinking(block) = self.content_mut(content_index)? else {
                    return Ok(None);
                };
                block.thinking.push_str(text);
                Ok(Some(AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta: text.to_owned(),
                    partial: Arc::new(self.message.clone()),
                }))
            }
            Some("signature_delta") => {
                let signature = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let AssistantContent::Thinking(block) = self.content_mut(content_index)? else {
                    return Ok(None);
                };
                block
                    .thinking_signature
                    .get_or_insert_with(String::new)
                    .push_str(signature);
                Ok(None)
            }
            Some("input_json_delta") => {
                let fragment = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let partial_json = self.partial_json.entry(provider_index).or_default();
                partial_json.push_str(fragment);
                let arguments = parse_streaming_json(partial_json);
                if let AssistantContent::ToolCall(tool) = self.content_mut(content_index)? {
                    tool.arguments = arguments;
                }
                Ok(Some(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta: fragment.to_owned(),
                    partial: Arc::new(self.message.clone()),
                }))
            }
            _ => Ok(None),
        }
    }

    fn content_stop(
        &mut self,
        value: &Value,
    ) -> Result<Option<AssistantMessageEvent>, AdapterError> {
        let provider_index = value.get("index").and_then(Value::as_u64).ok_or_else(|| {
            AdapterError::Protocol("Anthropic content_block_stop missing index".to_owned())
        })?;
        let Some(content_index) = self.provider_blocks.remove(&provider_index) else {
            return Ok(None);
        };
        match self.message.content.get(
            usize::try_from(content_index)
                .map_err(|_| AdapterError::Protocol("content index overflow".to_owned()))?,
        ) {
            Some(AssistantContent::Text(block)) => Ok(Some(AssistantMessageEvent::TextEnd {
                content_index,
                content: block.text.clone(),
                partial: Arc::new(self.message.clone()),
            })),
            Some(AssistantContent::Thinking(block)) => {
                Ok(Some(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: block.thinking.clone(),
                    partial: Arc::new(self.message.clone()),
                }))
            }
            Some(AssistantContent::ToolCall(_)) => {
                if let Some(json) = self.partial_json.remove(&provider_index)
                    && !json.is_empty()
                {
                    let arguments = parse_streaming_json(&json);
                    if let AssistantContent::ToolCall(tool) = self.content_mut(content_index)? {
                        tool.arguments = arguments;
                    }
                }
                let Some(AssistantContent::ToolCall(tool_call)) = self
                    .message
                    .content
                    .get(usize::try_from(content_index).map_err(|_| {
                        AdapterError::Protocol("content index overflow".to_owned())
                    })?)
                else {
                    return Ok(None);
                };
                Ok(Some(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call: tool_call.clone(),
                    partial: Arc::new(self.message.clone()),
                }))
            }
            None => Ok(None),
        }
    }

    fn message_delta(&mut self, value: &Value) {
        if let Some(reason) = value["delta"].get("stop_reason").and_then(Value::as_str) {
            self.message.stop_reason = match reason {
                "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                "tool_use" => StopReason::ToolUse,
                "refusal" => {
                    self.message.error_message = Some(
                        value["delta"]
                            .get("stop_details")
                            .and_then(|details| details.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("Anthropic refused the request")
                            .to_owned(),
                    );
                    StopReason::Error
                }
                _ => StopReason::Stop,
            };
        }
        update_usage(&self.model, &mut self.message, &value["usage"]);
    }

    fn last_index(&self) -> Result<u64, AdapterError> {
        let index = self.message.content.len().checked_sub(1).ok_or_else(|| {
            AdapterError::Protocol("Anthropic content index underflow".to_owned())
        })?;
        u64::try_from(index)
            .map_err(|_| AdapterError::Protocol("content index overflow".to_owned()))
    }

    fn content_mut(&mut self, index: u64) -> Result<&mut AssistantContent, AdapterError> {
        self.message
            .content
            .get_mut(
                usize::try_from(index)
                    .map_err(|_| AdapterError::Protocol("content index overflow".to_owned()))?,
            )
            .ok_or_else(|| {
                AdapterError::Protocol(format!("missing Anthropic content block {index}"))
            })
    }
}

fn update_usage(model: &Model, message: &mut AssistantMessage, value: &Value) {
    if let Some(input) = value.get("input_tokens").and_then(Value::as_u64) {
        message.usage.input = input;
    }
    if let Some(output) = value.get("output_tokens").and_then(Value::as_u64) {
        message.usage.output = output;
    }
    if let Some(cache_read) = value.get("cache_read_input_tokens").and_then(Value::as_u64) {
        message.usage.cache_read = cache_read;
    }
    if let Some(cache_write) = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        message.usage.cache_write = cache_write;
    }
    if let Some(cache_write1h) = value
        .get("cache_creation")
        .and_then(|cache| cache.get("ephemeral_1h_input_tokens"))
        .and_then(Value::as_u64)
    {
        message.usage.cache_write1h = Some(cache_write1h);
    }
    if let Some(reasoning) = value
        .get("output_tokens_details")
        .and_then(|details| details.get("thinking_tokens"))
        .and_then(Value::as_u64)
    {
        message.usage.reasoning = Some(reasoning);
    }
    message.usage.total_tokens = message
        .usage
        .input
        .saturating_add(message.usage.output)
        .saturating_add(message.usage.cache_read)
        .saturating_add(message.usage.cache_write);
    calculate_cost(model, &mut message.usage);
}

fn delivery_error(error: super::stream_state::EventSendError) -> AdapterError {
    AdapterError::Delivery(error.to_string())
}

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn classify_error(
    error: &AdapterError,
    signal: Option<&tokio_util::sync::CancellationToken>,
) -> ErrorReason {
    if matches!(
        error,
        AdapterError::Cancelled | AdapterError::Transport(TransportError::Cancelled)
    ) || signal.is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        ErrorReason::Aborted
    } else {
        ErrorReason::Error
    }
}

#[derive(Debug, thiserror::Error)]
enum AdapterError {
    #[error("request cancelled")]
    Cancelled,
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Sse(String),
    #[error("payload or response callback failed: {0}")]
    Callback(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("request construction failed: {0}")]
    RequestBuild(reqwest::Error),
    #[error("response body failed: {0}")]
    Body(reqwest::Error),
    #[error("{0}")]
    Transport(TransportError),
    #[error("event delivery failed: {0}")]
    Delivery(String),
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::types::{
        ModelCost, ModelInput, TextContent, Tool, ToolResultMessage, Usage, UserMessage,
    };

    fn model() -> Model {
        Model {
            id: "claude-test".to_owned(),
            name: "Claude Test".to_owned(),
            api: "anthropic-messages".to_owned(),
            provider: "anthropic".to_owned(),
            base_url: "https://example.invalid".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
                tiers: None,
            },
            context_window: 200_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn sse_decoder_supports_multiline_comments_and_all_line_endings() {
        let mut decoder = AnthropicSseDecoder::default();
        assert!(
            decoder
                .push(b": keepalive\revent: message_start\r\ndata: {\"type\":\r")
                .unwrap_or_default()
                .is_empty()
        );
        let events = decoder
            .push(b"data: \"message_start\",\"message\":{\"id\":\"m\",\"usage\":{}}}\n\n")
            .unwrap_or_default();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message_start");
        assert_eq!(events[0].value["message"]["id"], "m");
    }

    #[test]
    fn sse_decoder_repairs_json_and_filters_unknown_events() {
        let mut decoder = AnthropicSseDecoder::default();
        let events = decoder
            .push(
                b"event: ping\ndata: {}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"\n\n",
            )
            .unwrap_or_default();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message_stop");
    }

    #[test]
    fn payload_converts_cache_thinking_tools_and_messages() {
        let context = Context {
            system_prompt: Some("system".to_owned()),
            messages: vec![
                Message::User(UserMessage::new(
                    UserMessageContent::Text("hello".to_owned()),
                    0,
                )),
                Message::Assistant({
                    let mut message =
                        AssistantMessage::new("anthropic-messages", "anthropic", "old", 0);
                    message
                        .content
                        .push(AssistantContent::Thinking(ThinkingContent::new("thought")));
                    message.content.push(AssistantContent::ToolCall(ToolCall::new(
                        "bad id/with punctuation and a suffix that makes this identifier much longer than sixty four characters",
                        "read",
                        Map::new(),
                    )));
                    message
                }),
                Message::ToolResult(ToolResultMessage::new(
                    "bad id/with punctuation and a suffix that makes this identifier much longer than sixty four characters",
                    "read",
                    vec![ToolResultContent::Text(TextContent::new("ok"))],
                    false,
                    0,
                )),
            ],
            tools: Some(vec![Tool {
                name: "read".to_owned(),
                description: "read a file".to_owned(),
                parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
            }]),
        };
        let mut options = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        options
            .extra
            .insert("thinkingEnabled".to_owned(), Value::Bool(true));
        options
            .extra
            .insert("thinkingBudgetTokens".to_owned(), Value::from(2048));
        let payload = build_payload(&model(), &context, &options);
        assert_eq!(payload["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(payload["thinking"]["budget_tokens"], 2048);
        assert_eq!(payload["tools"][0]["eager_input_streaming"], true);
        assert_eq!(payload["tools"][0]["cache_control"]["ttl"], "1h");
        let tool_id = payload["messages"][1]["content"][1]["id"]
            .as_str()
            .unwrap_or_default();
        assert!(tool_id.len() <= 64);
        assert!(
            tool_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert_eq!(payload["messages"][2]["content"][0]["tool_use_id"], tool_id);
    }

    #[test]
    fn thinking_disable_respects_off_null_sentinel() {
        let context = Context::default();
        let mut options = StreamOptions::default();
        options
            .extra
            .insert("thinkingEnabled".to_owned(), Value::Bool(false));

        // Plain reasoning model: explicit disable is sent (upstream parity).
        let payload = build_payload(&model(), &context, &options);
        assert_eq!(payload["thinking"]["type"], "disabled");

        // off:null pins the level as unsupported: the disable is omitted.
        let mut adaptive = model();
        adaptive.thinking_level_map = Some(std::collections::BTreeMap::from([(
            ModelThinkingLevel::Off,
            None,
        )]));
        let payload = build_payload(&adaptive, &context, &options);
        assert!(payload.get("thinking").is_none());
    }

    #[test]
    fn tool_id_sanitization_is_ascii_and_bounded() {
        let normalized = normalize_tool_call_id(&format!("a/b:c?{}", "x".repeat(100)));
        assert_eq!(normalized.len(), 64);
        assert!(
            normalized.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            )
        );
        assert_eq!(normalize_tool_call_id("abc_DEF-123"), "abc_DEF-123");
    }

    #[test]
    fn anthropic_usage_includes_cache_write1h_and_exact_cost() {
        let model = model();
        let mut message =
            AssistantMessage::new("anthropic-messages", "anthropic", "claude-test", 0);
        update_usage(
            &model,
            &mut message,
            &json!({
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 40,
                "cache_creation": { "ephemeral_1h_input_tokens": 10 },
                "output_tokens_details": { "thinking_tokens": 7 }
            }),
        );
        assert_eq!(message.usage.total_tokens, 190);
        assert_eq!(message.usage.cache_write1h, Some(10));
        assert_eq!(message.usage.reasoning, Some(7));
        let expected = 3.0 * 100.0 / 1_000_000.0
            + 15.0 * 20.0 / 1_000_000.0
            + 0.3 * 30.0 / 1_000_000.0
            + (3.75 * 30.0 + 3.0 * 2.0 * 10.0) / 1_000_000.0;
        assert!((message.usage.cost.total - expected).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn missing_message_stop_is_a_terminal_protocol_error() {
        let model = model();
        let mut assembler = StreamAssembler::new(&model);
        let mut decoder = AnthropicSseDecoder::default();
        for event in decoder
            .push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n")
            .unwrap_or_default()
        {
            let (sender, mut stream) = ProviderEventSender::channel(STREAM_CAPACITY);
            assert!(sender.start(Arc::new(assembler.message.clone())).await.is_ok());
            assert!(assembler.apply(event, &sender).await.is_ok());
            drop(sender);
            while stream.next().await.is_some() {}
        }
        assert!(!assembler.saw_message_stop);
        let error = require_message_stop(&assembler)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert_eq!(error, MESSAGE_STOP_MISSING);
    }

    #[test]
    fn cancellation_has_aborted_classification() {
        let signal = tokio_util::sync::CancellationToken::new();
        assert_eq!(
            classify_error(&AdapterError::Cancelled, Some(&signal)),
            ErrorReason::Aborted
        );
        assert_eq!(
            classify_error(
                &AdapterError::Protocol("bad stream".to_owned()),
                Some(&signal)
            ),
            ErrorReason::Error
        );
        signal.cancel();
        assert_eq!(
            classify_error(
                &AdapterError::Protocol("body closed".to_owned()),
                Some(&signal)
            ),
            ErrorReason::Aborted
        );
    }

    #[tokio::test]
    async fn non_success_http_body_is_truncated_to_display_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = format!("{{\"error\":\"{}\"}}", "x".repeat(4_200));
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            let (mut socket, _) = listener.accept()?;
            let mut request = [0_u8; 8_192];
            let _read = socket.read(&mut request)?;
            socket.write_all(response.as_bytes())?;
            Ok(())
        });

        let mut model = model();
        model.base_url = format!("http://{address}");
        let client = Client::new();
        let provider = AnthropicMessages::new(client);
        let events = provider
            .stream(
                &model,
                Context::default(),
                StreamOptions {
                    api_key: Some("test-key".to_owned()),
                    ..StreamOptions::default()
                },
            )
            .collect::<Vec<_>>()
            .await;
        server.join().map_err(|_| "server thread failed")??;

        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        let Some(Ok(AssistantMessageEvent::Error { reason, error })) = events.get(1) else {
            return Err("expected terminal error event".into());
        };
        assert_eq!(*reason, ErrorReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        let message = error.error_message.as_deref().unwrap_or_default();
        assert!(message.starts_with("HTTP 400:"));
        assert!(message.contains("[truncated"));
        assert!(message.chars().count() < body.chars().count());
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_error_body_read_is_aborted_after_start()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            let (mut socket, _) = listener.accept()?;
            let mut request = [0_u8; 8_192];
            let _read = socket.read(&mut request)?;
            // Headers only; leave the body hanging so cancellation wins during body read.
            socket.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n",
            )?;
            // Keep the connection open until the client times out/cancels.
            let mut sink = [0_u8; 1];
            let _ = socket.read(&mut sink);
            Ok(())
        });

        let mut model = model();
        model.base_url = format!("http://{address}");
        let signal = tokio_util::sync::CancellationToken::new();
        let cancel = signal.clone();
        let client = Client::new();
        let provider = AnthropicMessages::new(client);
        let mut stream = provider.stream(
            &model,
            Context::default(),
            StreamOptions {
                api_key: Some("test-key".to_owned()),
                signal: Some(signal),
                ..StreamOptions::default()
            },
        );

        let first = stream.next().await;
        assert!(matches!(
            first,
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        // Allow the adapter to reach the error-body read, then cancel.
        tokio::task::yield_now().await;
        cancel.cancel();

        let terminal = stream.next().await;
        let Some(Ok(AssistantMessageEvent::Error { reason, error })) = terminal else {
            return Err("expected aborted terminal error".into());
        };
        assert_eq!(reason, ErrorReason::Aborted);
        assert_eq!(error.stop_reason, StopReason::Aborted);
        assert!(stream.next().await.is_none());
        let _ = server.join();
        Ok(())
    }

    #[test]
    fn usage_default_remains_wire_compatible() {
        let usage = Usage::default();
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.cache_write1h, None);
    }
}
