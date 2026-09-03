//! Shared Google `GenerateContent` conversion and streaming state.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::provider::{StreamOptionKey, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, ErrorReason,
    Message, Model, ModelInput, StopReason, TextContent, ThinkingContent, Tool, ToolCall,
    ToolResultContent, Usage, UsageCost, UserContent, UserMessageContent,
};

use super::{calculate_cost, sanitize_surrogates, transform_messages, truncate_error_body};
use crate::providers::stream_state::ProviderEventSender;
use crate::providers::transport::{DataSseDecoder, DataSseEvent};

/// Maximum number of queued semantic events before provider reading applies backpressure.
pub(crate) const EVENT_CAPACITY: usize = 32;

/// Failure produced by Google request construction or stream processing.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct GoogleFailure {
    pub(crate) message: String,
    pub(crate) aborted: bool,
}

impl GoogleFailure {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    pub(crate) fn aborted() -> Self {
        Self {
            message: "Request was aborted".to_owned(),
            aborted: true,
        }
    }
}

/// Gemini thinking-level values accepted by `GenerateContent`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoogleThinkingLevel {
    Unspecified,
    Minimal,
    Low,
    Medium,
    High,
}

impl GoogleThinkingLevel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "THINKING_LEVEL_UNSPECIFIED",
            Self::Minimal => "MINIMAL",
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "THINKING_LEVEL_UNSPECIFIED" => Some(Self::Unspecified),
            "MINIMAL" => Some(Self::Minimal),
            "LOW" => Some(Self::Low),
            "MEDIUM" => Some(Self::Medium),
            "HIGH" => Some(Self::High),
            _ => None,
        }
    }
}

/// Build the `GenerateContent` wire body shared by Gemini API and Vertex AI.
pub(crate) fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    thinking_config: Option<Value>,
) -> Value {
    let mut body = Map::new();
    body.insert(
        "contents".to_owned(),
        Value::Array(convert_messages(model, context)),
    );

    let mut generation_config = Map::new();
    if let Some(temperature) = options.temperature
        && let Some(number) = serde_json::Number::from_f64(temperature)
    {
        generation_config.insert("temperature".to_owned(), Value::Number(number));
    }
    if let Some(max_tokens) = options.max_tokens {
        generation_config.insert("maxOutputTokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(thinking_config) = thinking_config {
        generation_config.insert("thinkingConfig".to_owned(), thinking_config);
    }
    body.insert(
        "generationConfig".to_owned(),
        Value::Object(generation_config),
    );

    if let Some(system_prompt) = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())
    {
        body.insert(
            "systemInstruction".to_owned(),
            json!({
                "role": "user",
                "parts": [{"text": sanitize_surrogates(system_prompt)}],
            }),
        );
    }

    if let Some(tools) = context.tools.as_deref().filter(|tools| !tools.is_empty()) {
        if let Some(converted) = convert_tools(tools, false) {
            body.insert("tools".to_owned(), converted);
        }
        if let Some(choice) = options
            .extra_value(StreamOptionKey::TOOL_CHOICE)
            .and_then(Value::as_str)
            .or_else(|| {
                options
                    .extra_value(StreamOptionKey::TOOL_CHOICE_SNAKE_CASE)
                    .and_then(Value::as_str)
            })
        {
            body.insert(
                "toolConfig".to_owned(),
                json!({"functionCallingConfig": {"mode": map_tool_choice(choice)}}),
            );
        }
    }

    Value::Object(body)
}

/// Convert pi conversation messages to Google `GenerateContent` contents.
pub(crate) fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let transformed = transform_messages(&context.messages, model, |id, target, _source| {
        normalize_tool_call_id(id, target)
    });
    let include_ids = requires_tool_call_id(&model.id);
    let mut contents = Vec::new();

    for message in transformed {
        match message {
            Message::User(message) => {
                if let Some(content) = convert_user_message(message) {
                    contents.push(content);
                }
            }
            Message::Assistant(message) => {
                if let Some(content) = convert_assistant_message(model, include_ids, message) {
                    contents.push(content);
                }
            }
            Message::ToolResult(message) => {
                append_tool_result(&mut contents, model, include_ids, message);
            }
        }
    }

    contents
}

fn convert_user_message(message: crate::types::UserMessage) -> Option<Value> {
    let parts = match message.content {
        UserMessageContent::Text(text) => {
            vec![json!({"text": sanitize_surrogates(&text)})]
        }
        UserMessageContent::Blocks(blocks) => blocks
            .into_iter()
            .map(|block| match block {
                UserContent::Text(text) => {
                    json!({"text": sanitize_surrogates(&text.text)})
                }
                UserContent::Image(image) => json!({
                    "inlineData": {
                        "mimeType": image.mime_type,
                        "data": image.data,
                    }
                }),
            })
            .collect(),
    };
    (!parts.is_empty()).then(|| json!({"role": "user", "parts": parts}))
}

fn convert_assistant_message(
    model: &Model,
    include_ids: bool,
    message: AssistantMessage,
) -> Option<Value> {
    let same_provider_and_model = message.provider == model.provider && message.model == model.id;
    let mut parts = Vec::new();
    for block in message.content {
        if let Some(part) = convert_assistant_part(block, same_provider_and_model, include_ids) {
            parts.push(part);
        }
    }
    (!parts.is_empty()).then(|| json!({"role": "model", "parts": parts}))
}

fn convert_assistant_part(
    block: AssistantContent,
    same_provider_and_model: bool,
    include_ids: bool,
) -> Option<Value> {
    match block {
        AssistantContent::Text(text) => {
            let signature =
                resolve_thought_signature(same_provider_and_model, text.text_signature.as_deref());
            // Gemini attaches signatures to empty-text parts and requires them
            // echoed back; dropping the part breaks the reasoning chain.
            if text.text.is_empty() && signature.is_none() {
                return None;
            }
            let mut part = Map::new();
            part.insert(
                "text".to_owned(),
                Value::String(sanitize_surrogates(&text.text).into_owned()),
            );
            if let Some(signature) = signature {
                part.insert(
                    "thoughtSignature".to_owned(),
                    Value::String(signature.to_owned()),
                );
            }
            Some(Value::Object(part))
        }
        AssistantContent::Thinking(thinking) => {
            let signature = resolve_thought_signature(
                same_provider_and_model,
                thinking.thinking_signature.as_deref(),
            );
            if thinking.thinking.trim().is_empty() && signature.is_none() {
                return None;
            }
            let mut part = Map::new();
            if same_provider_and_model {
                part.insert("thought".to_owned(), Value::Bool(true));
            }
            part.insert(
                "text".to_owned(),
                Value::String(sanitize_surrogates(&thinking.thinking).into_owned()),
            );
            if let Some(signature) = signature {
                part.insert(
                    "thoughtSignature".to_owned(),
                    Value::String(signature.to_owned()),
                );
            }
            Some(Value::Object(part))
        }
        AssistantContent::ToolCall(tool_call) => {
            let mut function_call = Map::new();
            function_call.insert("name".to_owned(), Value::String(tool_call.name.clone()));
            function_call.insert(
                "args".to_owned(),
                Value::Object(tool_call.arguments.clone()),
            );
            if include_ids {
                function_call.insert("id".to_owned(), Value::String(tool_call.id.clone()));
            }
            let mut part = Map::new();
            part.insert("functionCall".to_owned(), Value::Object(function_call));
            if let Some(signature) = resolve_thought_signature(
                same_provider_and_model,
                tool_call.thought_signature.as_deref(),
            ) {
                part.insert(
                    "thoughtSignature".to_owned(),
                    Value::String(signature.to_owned()),
                );
            }
            Some(Value::Object(part))
        }
        AssistantContent::Text(_) | AssistantContent::Thinking(_) => None,
    }
}

fn append_tool_result(
    contents: &mut Vec<Value>,
    model: &Model,
    include_ids: bool,
    message: crate::types::ToolResultMessage,
) {
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = if model.input.contains(&ModelInput::Image) {
        message
            .content
            .iter()
            .filter_map(|content| match content {
                ToolResultContent::Image(image) => Some(json!({
                    "inlineData": {
                        "mimeType": image.mime_type,
                        "data": image.data,
                    }
                })),
                ToolResultContent::Text(_) => None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let response_value = if !text.is_empty() {
        sanitize_surrogates(&text).into_owned()
    } else if !images.is_empty() {
        "(see attached image)".to_owned()
    } else {
        String::new()
    };

    let mut response = Map::new();
    response.insert(
        if message.is_error { "error" } else { "output" }.to_owned(),
        Value::String(response_value),
    );
    let mut function_response = Map::new();
    function_response.insert("name".to_owned(), Value::String(message.tool_name));
    function_response.insert("response".to_owned(), Value::Object(response));
    let supports_multimodal_response = supports_multimodal_function_response(&model.id);
    if !images.is_empty() && supports_multimodal_response {
        function_response.insert("parts".to_owned(), Value::Array(images.clone()));
    }
    if include_ids {
        function_response.insert("id".to_owned(), Value::String(message.tool_call_id));
    }
    let response_part = json!({"functionResponse": function_response});

    if let Some(last) = contents.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && last
            .get("parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("functionResponse").is_some())
            })
        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        parts.push(response_part);
    } else {
        contents.push(json!({"role": "user", "parts": [response_part]}));
    }

    if !images.is_empty() && !supports_multimodal_response {
        let mut parts = Vec::with_capacity(images.len() + 1);
        parts.push(json!({"text": "Tool result image:"}));
        parts.extend(images);
        contents.push(json!({"role": "user", "parts": parts}));
    }
}

/// Convert tools to Google's function-declaration wrapper.
pub(crate) fn convert_tools(tools: &[Tool], use_parameters: bool) -> Option<Value> {
    if tools.is_empty() {
        return None;
    }
    let declarations = tools
        .iter()
        .map(|tool| {
            let mut declaration = Map::new();
            declaration.insert("name".to_owned(), Value::String(tool.name.clone()));
            declaration.insert(
                "description".to_owned(),
                Value::String(tool.description.clone()),
            );
            declaration.insert(
                if use_parameters {
                    "parameters"
                } else {
                    "parametersJsonSchema"
                }
                .to_owned(),
                if use_parameters {
                    sanitize_schema_for_openapi(&tool.parameters)
                } else {
                    tool.parameters.clone()
                },
            );
            Value::Object(declaration)
        })
        .collect::<Vec<_>>();
    Some(json!([{"functionDeclarations": declarations}]))
}

fn sanitize_schema_for_openapi(schema: &Value) -> Value {
    const META_KEYS: &[&str] = &[
        "$schema",
        "$id",
        "$anchor",
        "$dynamicAnchor",
        "$vocabulary",
        "$comment",
        "$defs",
        "definitions",
    ];
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !META_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), sanitize_schema_for_openapi(value)))
                .collect(),
        ),
        Value::Array(array) => {
            Value::Array(array.iter().map(sanitize_schema_for_openapi).collect())
        }
        value => value.clone(),
    }
}

fn normalize_tool_call_id(id: &str, model: &Model) -> String {
    if !requires_tool_call_id(&model.id) {
        return id.to_owned();
    }
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

fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-") || model_id.starts_with("gpt-oss-")
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|version| version >= 3)
}

fn gemini_major_version(model_id: &str) -> Option<u64> {
    let lower = model_id.to_ascii_lowercase();
    let suffix = lower
        .strip_prefix("gemini-live-")
        .or_else(|| lower.strip_prefix("gemini-"))?;
    let digit_count = suffix.chars().take_while(char::is_ascii_digit).count();
    if digit_count == 0 {
        return None;
    }
    suffix[..digit_count].parse().ok()
}

/// A streamed part is thinking only when Google marks `thought: true`.
pub(crate) fn is_thinking_part(part: &Value) -> bool {
    part.get("thought").and_then(Value::as_bool) == Some(true)
}

/// Retain the most recent non-empty signature for one streamed block.
pub(crate) fn retain_thought_signature(
    existing: Option<String>,
    incoming: Option<&str>,
) -> Option<String> {
    incoming
        .filter(|signature| !signature.is_empty())
        .map(str::to_owned)
        .or(existing)
}

fn resolve_thought_signature(same_model: bool, signature: Option<&str>) -> Option<&str> {
    same_model
        .then_some(signature)
        .flatten()
        .filter(|value| is_valid_thought_signature(value))
}

fn is_valid_thought_signature(signature: &str) -> bool {
    if signature.is_empty() || !signature.len().is_multiple_of(4) {
        return false;
    }
    let bytes = signature.as_bytes();
    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        && bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte == b'=')
}

fn map_tool_choice(choice: &str) -> &'static str {
    match choice.to_ascii_lowercase().as_str() {
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// Map a raw Google finish reason to pi's canonical stop reason.
pub(crate) fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        _ => StopReason::Error,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenBlock {
    Text(u64),
    Thinking(u64),
}

fn content_index(index: u64) -> Result<usize, GoogleFailure> {
    usize::try_from(index).map_err(|_| GoogleFailure::error("Google content index overflow"))
}

fn text_content(output: &AssistantMessage, index: u64) -> Result<&TextContent, GoogleFailure> {
    match output.content.get(content_index(index)?) {
        Some(AssistantContent::Text(content)) => Ok(content),
        _ => Err(GoogleFailure::error("invalid Google text block state")),
    }
}

fn text_content_mut(
    output: &mut AssistantMessage,
    index: u64,
) -> Result<&mut TextContent, GoogleFailure> {
    match output.content.get_mut(content_index(index)?) {
        Some(AssistantContent::Text(content)) => Ok(content),
        _ => Err(GoogleFailure::error("invalid Google text block state")),
    }
}

fn thinking_content(
    output: &AssistantMessage,
    index: u64,
) -> Result<&ThinkingContent, GoogleFailure> {
    match output.content.get(content_index(index)?) {
        Some(AssistantContent::Thinking(content)) => Ok(content),
        _ => Err(GoogleFailure::error("invalid Google thinking block state")),
    }
}

fn thinking_content_mut(
    output: &mut AssistantMessage,
    index: u64,
) -> Result<&mut ThinkingContent, GoogleFailure> {
    match output.content.get_mut(content_index(index)?) {
        Some(AssistantContent::Thinking(content)) => Ok(content),
        _ => Err(GoogleFailure::error("invalid Google thinking block state")),
    }
}

#[derive(Default)]
struct GoogleChunkState {
    open: Option<OpenBlock>,
    saw_finish_reason: bool,
    saw_done_marker: bool,
}

/// Consume a successful Google SSE response and emit canonical events.
pub(crate) async fn consume_response(
    response: reqwest::Response,
    model: &Model,
    options: &StreamOptions,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    tool_call_counter: &AtomicU64,
) -> Result<(), GoogleFailure> {
    let mut decoder = DataSseDecoder::default();
    let mut body = response.bytes_stream();
    let mut state = GoogleChunkState::default();

    'body: loop {
        let next = if let Some(signal) = &options.signal {
            tokio::select! {
                () = signal.cancelled() => return Err(GoogleFailure::aborted()),
                item = body.next() => item,
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|error| {
            GoogleFailure::error(format!("Google GenerateContent stream failed: {error}"))
        })?;
        for event in decoder
            .push(&chunk)
            .map_err(|error| GoogleFailure::error(error.to_string()))?
        {
            if process_sse_event(event, sender, output, model, &mut state, tool_call_counter)
                .await?
            {
                break 'body;
            }
        }
    }

    if !state.saw_done_marker {
        for event in decoder
            .finish()
            .map_err(|error| GoogleFailure::error(error.to_string()))?
        {
            if process_sse_event(event, sender, output, model, &mut state, tool_call_counter)
                .await?
            {
                break;
            }
        }
    }

    finish_open_block(sender, output, &mut state).await?;
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(GoogleFailure::aborted());
    }
    if !state.saw_finish_reason {
        return Err(GoogleFailure::error(
            "Google GenerateContent stream ended before a finish reason",
        ));
    }

    let reason = match output.stop_reason {
        StopReason::Stop => DoneReason::Stop,
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Error => return Err(GoogleFailure::error("An unknown error occurred")),
        StopReason::Aborted => return Err(GoogleFailure::aborted()),
    };
    sender
        .done(reason, output.clone())
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))
}

async fn process_sse_event(
    event: DataSseEvent,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    model: &Model,
    state: &mut GoogleChunkState,
    tool_call_counter: &AtomicU64,
) -> Result<bool, GoogleFailure> {
    let DataSseEvent::Data(data) = event else {
        state.saw_done_marker = true;
        return Ok(true);
    };
    let chunk: Value = serde_json::from_str(&data)
        .map_err(|error| GoogleFailure::error(format!("invalid Google SSE JSON: {error}")))?;
    if let Some(provider_error) = chunk.get("error") {
        return Err(GoogleFailure::error(format!(
            "Google API stream error: {}",
            truncate_error_body(&provider_error.to_string())
        )));
    }
    process_chunk(sender, output, model, state, tool_call_counter, &chunk).await?;
    Ok(false)
}

async fn process_chunk(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    model: &Model,
    state: &mut GoogleChunkState,
    tool_call_counter: &AtomicU64,
    chunk: &Value,
) -> Result<(), GoogleFailure> {
    if output.response_id.is_none() {
        output.response_id = chunk
            .get("responseId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
    }

    let candidate = chunk
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first());
    if let Some(parts) = candidate
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                append_text_part(sender, output, state, part, text).await?;
            }
            if let Some(function_call) = part.get("functionCall") {
                finish_open_block(sender, output, state).await?;
                append_tool_call(
                    sender,
                    output,
                    function_call,
                    part.get("thoughtSignature").and_then(Value::as_str),
                    tool_call_counter,
                )
                .await?;
            }
        }
    }

    if let Some(reason) = candidate
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str)
    {
        state.saw_finish_reason = true;
        output.stop_reason = map_stop_reason(reason);
        if output
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            output.stop_reason = StopReason::ToolUse;
        }
    }

    if let Some(usage) = chunk.get("usageMetadata") {
        apply_usage(output, model, usage);
    }
    Ok(())
}

async fn append_text_part(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    state: &mut GoogleChunkState,
    part: &Value,
    delta: &str,
) -> Result<(), GoogleFailure> {
    let thinking = is_thinking_part(part);
    let needs_new_block = match state.open {
        None => true,
        Some(OpenBlock::Text(_)) => thinking,
        Some(OpenBlock::Thinking(_)) => !thinking,
    };
    if needs_new_block {
        finish_open_block(sender, output, state).await?;
        let index = u64::try_from(output.content.len())
            .map_err(|_| GoogleFailure::error("Google content index overflow"))?;
        if thinking {
            output
                .content
                .push(AssistantContent::Thinking(ThinkingContent::new("")));
            state.open = Some(OpenBlock::Thinking(index));
            sender
                .event(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: Arc::new(output.clone()),
                })
                .await
                .map_err(|error| GoogleFailure::error(error.to_string()))?;
        } else {
            output
                .content
                .push(AssistantContent::Text(TextContent::new("")));
            state.open = Some(OpenBlock::Text(index));
            sender
                .event(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: Arc::new(output.clone()),
                })
                .await
                .map_err(|error| GoogleFailure::error(error.to_string()))?;
        }
    }

    let signature = part.get("thoughtSignature").and_then(Value::as_str);
    let event = match state.open {
        Some(OpenBlock::Text(index)) => {
            let content = text_content_mut(output, index)?;
            content.text.push_str(delta);
            content.text_signature =
                retain_thought_signature(content.text_signature.take(), signature);
            AssistantMessageEvent::TextDelta {
                content_index: index,
                delta: delta.to_owned(),
                partial: Arc::new(output.clone()),
            }
        }
        Some(OpenBlock::Thinking(index)) => {
            let content = thinking_content_mut(output, index)?;
            content.thinking.push_str(delta);
            content.thinking_signature =
                retain_thought_signature(content.thinking_signature.take(), signature);
            AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta: delta.to_owned(),
                partial: Arc::new(output.clone()),
            }
        }
        None => return Err(GoogleFailure::error("missing Google content block")),
    };
    sender
        .event(event)
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))
}

async fn finish_open_block(
    sender: &ProviderEventSender,
    output: &AssistantMessage,
    state: &mut GoogleChunkState,
) -> Result<(), GoogleFailure> {
    let Some(open) = state.open.take() else {
        return Ok(());
    };
    let event = match open {
        OpenBlock::Text(index) => {
            let content = text_content(output, index)?;
            AssistantMessageEvent::TextEnd {
                content_index: index,
                content: content.text.clone(),
                partial: Arc::new(output.clone()),
            }
        }
        OpenBlock::Thinking(index) => {
            let content = thinking_content(output, index)?;
            AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: content.thinking.clone(),
                partial: Arc::new(output.clone()),
            }
        }
    };
    sender
        .event(event)
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))
}

async fn append_tool_call(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    function_call: &Value,
    thought_signature: Option<&str>,
    tool_call_counter: &AtomicU64,
) -> Result<(), GoogleFailure> {
    let name = function_call
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provided_id = function_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let used_ids = output
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let id = if let Some(provided_id) = provided_id.filter(|id| !used_ids.contains(id)) {
        provided_id.to_owned()
    } else {
        let counter = tool_call_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{name}_{}_{}", unix_millis(), counter)
    };
    let arguments = function_call
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut tool_call = ToolCall::new(id, name, arguments.clone());
    tool_call.thought_signature = thought_signature
        .filter(|signature| !signature.is_empty())
        .map(str::to_owned);
    output
        .content
        .push(AssistantContent::ToolCall(tool_call.clone()));
    let index = u64::try_from(output.content.len().saturating_sub(1))
        .map_err(|_| GoogleFailure::error("Google content index overflow"))?;
    sender
        .event(AssistantMessageEvent::ToolCallStart {
            content_index: index,
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))?;
    sender
        .event(AssistantMessageEvent::ToolCallDelta {
            content_index: index,
            delta: Value::Object(arguments).to_string(),
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))?;
    sender
        .event(AssistantMessageEvent::ToolCallEnd {
            content_index: index,
            tool_call,
            partial: Arc::new(output.clone()),
        })
        .await
        .map_err(|error| GoogleFailure::error(error.to_string()))
}

fn apply_usage(output: &mut AssistantMessage, model: &Model, usage: &Value) {
    let prompt = token_count(usage, "promptTokenCount");
    let cached = token_count(usage, "cachedContentTokenCount");
    let candidates = token_count(usage, "candidatesTokenCount");
    let thoughts = token_count(usage, "thoughtsTokenCount");
    output.usage = Usage {
        input: prompt.saturating_sub(cached),
        output: candidates.saturating_add(thoughts),
        cache_read: cached,
        cache_write: 0,
        cache_write1h: None,
        reasoning: Some(thoughts),
        total_tokens: token_count(usage, "totalTokenCount"),
        cost: UsageCost::default(),
    };
    calculate_cost(model, &mut output.usage);
}

fn token_count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Set canonical failed state and send the sole terminal error.
pub(crate) async fn emit_failure(
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    failure: GoogleFailure,
) {
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
    let _ = sender.error(reason, output.clone()).await;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::sync::atomic::AtomicU64;

    use futures::StreamExt;

    use super::*;
    use crate::types::{ImageContent, ModelCost, ToolResultMessage, UserMessage};

    fn model(id: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text, ModelInput::Image],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 0.0,
                tiers: None,
            },
            context_window: 1_000,
            max_tokens: 100,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn empty_signed_parts_survive_history_replay() {
        use crate::types::{AssistantContent, TextContent, ThinkingContent};

        let mut text = TextContent::new(String::new());
        text.text_signature = Some("QUJDRA==".to_owned());
        let kept = convert_assistant_part(AssistantContent::Text(text), true, false);
        assert_eq!(
            kept.and_then(|part| part
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .map(str::to_owned)),
            Some("QUJDRA==".to_owned())
        );

        let mut thinking = ThinkingContent::new(String::new());
        thinking.thinking_signature = Some("QUJDRA==".to_owned());
        let kept = convert_assistant_part(AssistantContent::Thinking(thinking), true, false);
        let part = kept.expect("empty signed thinking must survive replay");
        assert_eq!(part.get("thought"), Some(&Value::Bool(true)));
        assert_eq!(
            part.get("thoughtSignature").and_then(Value::as_str),
            Some("QUJDRA==")
        );

        let unsigned = TextContent::new(String::new());
        assert_eq!(
            convert_assistant_part(AssistantContent::Text(unsigned), true, false),
            None
        );
        let cross_model = TextContent::new(String::new());
        assert_eq!(
            convert_assistant_part(AssistantContent::Text(cross_model), false, false),
            None
        );
    }
    #[test]
    fn thinking_marker_and_signature_retention_match_google_protocol() {
        assert!(is_thinking_part(&json!({"thought": true})));
        assert!(!is_thinking_part(&json!({"thoughtSignature": "sig"})));
        let signature = retain_thought_signature(None, Some("sig-1"));
        assert_eq!(signature.as_deref(), Some("sig-1"));
        let signature = retain_thought_signature(signature, None);
        assert_eq!(signature.as_deref(), Some("sig-1"));
        let signature = retain_thought_signature(signature, Some(""));
        assert_eq!(signature.as_deref(), Some("sig-1"));
        let signature = retain_thought_signature(signature, Some("sig-2"));
        assert_eq!(signature.as_deref(), Some("sig-2"));
    }

    #[test]
    fn tool_schema_modes_preserve_or_strip_metadata_without_mutation() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"value": {"type": "string"}},
            "properties": {
                "value": {"$ref": "#/$defs/value", "$comment": "remove"}
            }
        });
        let tool = Tool {
            name: "lookup".into(),
            description: "Lookup".into(),
            parameters: schema.clone(),
        };
        let full = convert_tools(std::slice::from_ref(&tool), false);
        assert!(full.is_some(), "tool conversion should succeed");
        let full = full.as_ref().unwrap_or(&Value::Null);
        assert_eq!(
            full.pointer("/0/functionDeclarations/0/parametersJsonSchema/$schema"),
            Some(&json!("https://json-schema.org/draft/2020-12/schema"))
        );
        let openapi = convert_tools(&[tool], true);
        assert!(openapi.is_some(), "openapi tool conversion should succeed");
        let openapi = openapi.as_ref().unwrap_or(&Value::Null);
        assert!(
            openapi
                .pointer("/0/functionDeclarations/0/parameters/$schema")
                .is_none()
        );
        assert!(
            openapi
                .pointer("/0/functionDeclarations/0/parameters/$defs")
                .is_none()
        );
        assert_eq!(
            openapi.pointer("/0/functionDeclarations/0/parameters/properties/value/$ref"),
            Some(&json!("#/$defs/value"))
        );
        assert_eq!(
            schema["$schema"],
            json!("https://json-schema.org/draft/2020-12/schema")
        );
        assert!(convert_tools(&[], false).is_none());
    }

    #[test]
    fn conversion_routes_images_by_gemini_generation_and_filters_signatures() {
        let valid_signature = "AAAAAAAAAAAAAAAAAAAAAA==";
        let mut assistant =
            AssistantMessage::new("google-generative-ai", "google", "gemini-3-pro-preview", 1);
        let mut call = ToolCall::new("call:1", "inspect", Map::new());
        call.thought_signature = Some(valid_signature.into());
        assistant.content.push(AssistantContent::ToolCall(call));
        let result = ToolResultMessage::new(
            "call:1",
            "inspect",
            vec![
                ToolResultContent::Text(TextContent::new("ok")),
                ToolResultContent::Image(ImageContent::new("aW1n", "image/png")),
            ],
            false,
            2,
        );
        let context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(assistant), Message::ToolResult(result)],
            tools: None,
        };

        let gemini3 = convert_messages(&model("gemini-3-pro-preview"), &context);
        assert_eq!(
            gemini3[0].pointer("/parts/0/thoughtSignature"),
            Some(&json!(valid_signature))
        );
        assert!(gemini3[0].pointer("/parts/0/functionCall/id").is_none());
        assert_eq!(
            gemini3[1].pointer("/parts/0/functionResponse/parts/0/inlineData/data"),
            Some(&json!("aW1n"))
        );

        let mut gemini2_model = model("gemini-2.5-flash");
        gemini2_model.id = "gemini-2.5-flash".into();
        let gemini2 = convert_messages(&gemini2_model, &context);
        assert_eq!(gemini2.len(), 3);
        assert_eq!(
            gemini2[2].pointer("/parts/0/text"),
            Some(&json!("Tool result image:"))
        );

        let mut cross_model = model("gemini-3-flash-preview");
        cross_model.provider = "other".into();
        let converted = convert_messages(&cross_model, &context);
        assert!(converted[0].pointer("/parts/0/thoughtSignature").is_none());
        assert!(
            converted[0]
                .get("skip_thought_signature_validator")
                .is_none()
        );
    }

    #[tokio::test]
    async fn chunk_processing_preserves_signatures_usage_and_tool_stop()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = model("gemini-3-flash-preview");
        let mut output =
            AssistantMessage::new("google-generative-ai", "google", model.id.clone(), 1);
        let (sender, mut stream) =
            ProviderEventSender::channel(NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN));
        sender.start(Arc::new(output.clone())).await?;
        let mut state = GoogleChunkState::default();
        let counter = AtomicU64::new(0);
        process_chunk(
            &sender,
            &mut output,
            &model,
            &mut state,
            &counter,
            &json!({
                "responseId": "response-1",
                "candidates": [{
                    "content": {"parts": [
                        {"text": "plan", "thought": true, "thoughtSignature": "sig-1"},
                        {"functionCall": {"name": "lookup", "args": {"q": "rust"}}, "thoughtSignature": "sig-2"}
                    ]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "cachedContentTokenCount": 3,
                    "candidatesTokenCount": 4,
                    "thoughtsTokenCount": 2,
                    "totalTokenCount": 16
                }
            }),
        )
        .await?;
        finish_open_block(&sender, &output, &mut state).await?;
        assert_eq!(output.response_id.as_deref(), Some("response-1"));
        assert_eq!(output.stop_reason, StopReason::ToolUse);
        assert_eq!(output.usage.input, 7);
        assert_eq!(output.usage.output, 6);
        assert_eq!(output.usage.cache_read, 3);
        assert_eq!(output.usage.reasoning, Some(2));
        assert_eq!(output.usage.total_tokens, 16);
        assert!(output.usage.cost.total > 0.0);
        assert!(matches!(
            &output.content[0],
            AssistantContent::Thinking(content)
                if content.thinking_signature.as_deref() == Some("sig-1")
        ));
        assert!(matches!(
            &output.content[1],
            AssistantContent::ToolCall(call)
                if call.thought_signature.as_deref() == Some("sig-2")
        ));
        sender.done(DoneReason::ToolUse, output).await?;
        drop(sender);
        let events = stream.by_ref().collect::<Vec<_>>().await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(AssistantMessageEvent::Done { .. })))
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn user_strings_are_sanitized_without_losing_wire_shape() {
        let context = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::new(
                UserMessageContent::Text("hello".into()),
                1,
            ))],
            tools: None,
        };
        assert_eq!(
            convert_messages(&model("gemini-2.5-flash"), &context),
            vec![json!({"role": "user", "parts": [{"text": "hello"}]})]
        );
    }
}
