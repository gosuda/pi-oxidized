//! Native `OpenAI` Responses API adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{StreamExt, stream::BoxStream};
use reqwest::{Client, Request, Response};
use serde_json::{Value, json};

use super::shared::cloudflare::resolve_model;
use super::shared::responses::{
    ConvertMessagesOptions, ConvertToolsOptions, ProcessOptions, ResponsesStreamProcessor,
    convert_messages, convert_tools,
};
use super::shared::truncate_error_body;
use super::transport::{DataSseDecoder, DataSseEvent, HttpTransport, TransportError};
use crate::provider::{Provider, ProviderError, StreamOptionKey, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, CacheRetention, Context, ErrorReason, Message, Model,
    ModelThinkingLevel, Tool,
};

const EVENT_CHANNEL_CAPACITY: usize = 64;
const MIN_OUTPUT_TOKENS: u64 = 16;

/// `OpenAI`'s `/responses` streaming adapter.
#[derive(Clone, Debug)]
pub struct OpenAiResponses {
    transport: HttpTransport,
}

impl OpenAiResponses {
    /// Create an adapter backed by an already-configured reqwest client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for OpenAiResponses {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<crate::types::AssistantMessageEvent, ProviderError>> {
        let (sender, stream) = super::stream_state::ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CHANNEL_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        let adapter = self.clone();
        let model = resolve_model(model, options.env.as_ref()).into_owned();
        tokio::spawn(async move {
            let request_tier = string_option(&options, StreamOptionKey::SERVICE_TIER);
            let message = AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            let mut processor = ResponsesStreamProcessor::new(
                model.clone(),
                message,
                sender,
                ProcessOptions {
                    request_service_tier: request_tier,
                    apply_service_tier_pricing: true,
                    default_service_tier_uses_request: false,
                },
            );
            if processor.start().await.is_err() {
                return;
            }
            if let Err(failure) = adapter
                .run(&model, &context, &options, &mut processor)
                .await
            {
                let cancelled = options
                    .signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
                let (reason, message) = format_failure(&failure, cancelled);
                let _terminal = processor.fail(reason, message).await;
            }
        });
        stream
    }
}

impl OpenAiResponses {
    async fn run(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        processor: &mut ResponsesStreamProcessor,
    ) -> Result<(), AdapterFailure> {
        let cache_retention = resolve_cache_retention(options);
        let headers = build_headers(model, context, options, cache_retention);
        ensure_auth(model, options, &headers)?;
        let mut payload = build_payload(model, context, options, cache_retention);
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
    processor: &mut ResponsesStreamProcessor,
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
                DataSseEvent::Done => {
                    provider_done = true;
                    break;
                }
                DataSseEvent::Data(data) => {
                    if process_sse_data(data, processor).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
    if !provider_done {
        for event in decoder
            .finish()
            .map_err(|error| AdapterFailure::new(error.to_string()))?
        {
            match event {
                DataSseEvent::Done => break,
                DataSseEvent::Data(data) => {
                    if process_sse_data(data, processor).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
    processor
        .finish()
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

async fn process_sse_data(
    data: String,
    processor: &mut ResponsesStreamProcessor,
) -> Result<bool, AdapterFailure> {
    let value = serde_json::from_str::<Value>(&data)
        .map_err(|error| AdapterFailure::new(format!("invalid Responses event: {error}")))?;
    processor
        .handle(value)
        .await
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

fn build_request(
    transport: &HttpTransport,
    model: &Model,
    options: &StreamOptions,
    headers: BTreeMap<String, String>,
    payload: &Value,
) -> Result<Request, AdapterFailure> {
    let endpoint = format!("{}/responses", model.base_url.trim_end_matches('/'));
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

fn build_headers(
    model: &Model,
    _context: &Context,
    options: &StreamOptions,
    cache_retention: CacheRetention,
) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(session_id) = options
        .session_id
        .as_deref()
        .filter(|_| cache_retention != CacheRetention::None)
    {
        match session_affinity_format(model) {
            "openrouter" => {
                headers.insert("x-session-id".into(), session_id.into());
            }
            "openai-nosession" => {
                headers.insert("x-client-request-id".into(), session_id.into());
            }
            _ => {
                headers.insert("session_id".into(), session_id.into());
                headers.insert("x-client-request-id".into(), session_id.into());
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
    cache_retention: CacheRetention,
) -> Value {
    let supports_tool_search = compat_bool(model, "supportsToolSearch", false);
    let (immediate_tools, deferred_tools) = split_deferred_tools(context, supports_tool_search);
    let allowed: BTreeSet<String> = ["openai", "openai-codex", "opencode"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let input = convert_messages(
        model,
        context,
        &allowed,
        &ConvertMessagesOptions {
            include_system_prompt: true,
            deferred_tools,
        },
    );
    let mut payload = json!({
        "model": model.id,
        "input": input,
        "stream": true,
        "store": false,
    });
    if cache_retention != CacheRetention::None
        && let Some(session_id) = options.session_id.as_deref()
    {
        payload["prompt_cache_key"] = Value::String(clamp_cache_key(session_id));
    }
    if cache_retention == CacheRetention::Long
        && compat_bool(model, "supportsLongCacheRetention", true)
    {
        payload["prompt_cache_retention"] = Value::String("24h".into());
    }
    if cache_retention == CacheRetention::None
        && compat_bool(model, "supportsExplicitPromptCacheMode", false)
    {
        payload["prompt_cache_options"] = json!({"mode": "explicit"});
    }
    if let Some(max_tokens) = options.max_tokens {
        payload["max_output_tokens"] = Value::from(max_tokens.max(MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = Value::from(temperature);
    }
    if let Some(value) = options.extra_value(StreamOptionKey::SERVICE_TIER) {
        payload["service_tier"] = value.clone();
    }
    if !immediate_tools.is_empty() {
        payload["tools"] = Value::Array(convert_tools(
            &immediate_tools,
            ConvertToolsOptions::default(),
        ));
    }
    if let Some(tool_choice) = options.extra_value(StreamOptionKey::TOOL_CHOICE) {
        payload["tool_choice"] = tool_choice.clone();
    }
    apply_reasoning(model, options, &mut payload, true);
    payload
}

fn apply_reasoning(
    model: &Model,
    options: &StreamOptions,
    payload: &mut Value,
    skip_copilot_off: bool,
) {
    if !model.reasoning {
        return;
    }
    let effort = string_option(options, StreamOptionKey::REASONING_EFFORT);
    let summary = options
        .extra_value(StreamOptionKey::REASONING_SUMMARY)
        .and_then(Value::as_str);
    if effort.is_some() || summary.is_some() {
        let resolved_effort = effort.as_deref().map_or_else(
            || "medium".to_owned(),
            |effort| map_thinking_level(model, effort),
        );
        payload["reasoning"] = json!({
            "effort": resolved_effort,
            "summary": summary.unwrap_or("auto"),
        });
        payload["include"] = json!(["reasoning.encrypted_content"]);
    } else if !(skip_copilot_off && model.provider == "github-copilot") {
        let off = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&ModelThinkingLevel::Off));
        if !matches!(off, Some(None)) {
            payload["reasoning"] = json!({
                "effort": off.and_then(Clone::clone).unwrap_or_else(|| "none".into()),
            });
        }
    }
    if model.provider == "xai" {
        payload["include"] = json!(["reasoning.encrypted_content"]);
    }
}

fn split_deferred_tools(context: &Context, enabled: bool) -> (Vec<Tool>, BTreeMap<String, Tool>) {
    let mut unique = BTreeMap::new();
    for tool in context.tools.iter().flatten() {
        unique.insert(tool.name.clone(), tool.clone());
    }
    if !enabled {
        return (unique.into_values().collect(), BTreeMap::new());
    }
    let mut deferred_names = BTreeSet::new();
    let mut used_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(tool_call) = block {
                        used_names.insert(tool_call.name.clone());
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in result.added_tool_names.iter().flatten() {
                    if !used_names.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            Message::User(_) => {}
        }
    }
    let mut immediate = Vec::new();
    let mut deferred = BTreeMap::new();
    for (name, tool) in unique {
        if deferred_names.contains(&name) {
            deferred.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention.unwrap_or_else(|| {
        if env_value(options, "PI_CACHE_RETENTION") == Some("long") {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn clamp_cache_key(value: &str) -> String {
    value.chars().take(64).collect()
}

fn session_affinity_format(model: &Model) -> &str {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get("sessionAffinityFormat"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
                "openrouter"
            } else {
                "openai"
            }
        })
}

fn map_thinking_level(model: &Model, effort: &str) -> String {
    let level = match effort {
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        "off" => Some(ModelThinkingLevel::Off),
        _ => None,
    };
    level
        .and_then(|level| model.thinking_level_map.as_ref()?.get(&level)?.clone())
        .unwrap_or_else(|| effort.to_owned())
}

fn compat_bool(model: &Model, name: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn string_option(options: &StreamOptions, key: StreamOptionKey) -> Option<String> {
    options
        .extra_value(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn merge_option_headers(
    headers: &mut BTreeMap<String, String>,
    overrides: Option<&BTreeMap<String, Option<String>>>,
) {
    for (name, value) in overrides.into_iter().flatten() {
        remove_header(headers, name);
        if let Some(value) = value {
            headers.insert(name.clone(), value.clone());
        }
    }
}

fn remove_header(headers: &mut BTreeMap<String, String>, name: &str) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
}

fn has_nonempty_header(headers: &BTreeMap<String, String>, name: &str) -> bool {
    headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
}

fn env_value<'a>(options: &'a StreamOptions, name: &str) -> Option<&'a str> {
    options
        .env
        .as_ref()
        .and_then(|env| env.get(name))
        .map(String::as_str)
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
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

fn format_failure(failure: &AdapterFailure, cancelled: bool) -> (ErrorReason, String) {
    if cancelled || failure.aborted {
        (ErrorReason::Aborted, failure.message.clone())
    } else {
        (
            ErrorReason::Error,
            format!("OpenAI API error: {}", failure.message),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DoneReason, ModelCost, ModelInput, StopReason};

    fn model() -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
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

    #[test]
    fn request_has_responses_fields_and_minimum_output_tokens() {
        let context = Context::default();
        let mut options = StreamOptions {
            max_tokens: Some(1),
            session_id: Some("x".repeat(80)),
            ..StreamOptions::default()
        };
        options.insert_extra(
            StreamOptionKey::REASONING_EFFORT,
            Value::String("high".into()),
        );
        options.insert_extra(StreamOptionKey::SERVICE_TIER, Value::String("flex".into()));
        let payload = build_payload(&model(), &context, &options, CacheRetention::Long);
        assert_eq!(payload["store"], false);
        assert_eq!(payload["max_output_tokens"], MIN_OUTPUT_TOKENS);
        assert_eq!(payload["prompt_cache_key"].as_str().map(str::len), Some(64));
        assert_eq!(payload["prompt_cache_retention"], "24h");
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["service_tier"], "flex");
    }
    #[test]
    fn explicit_cache_mode_is_opt_in_per_model() {
        let context = Context::default();
        let mut explicit = model();
        explicit.compat = Some(json!({"supportsExplicitPromptCacheMode": true}));
        // Retention none + opt-in model: explicit mode disables implicit caching.
        let payload = build_payload(
            &explicit,
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload["prompt_cache_options"], json!({"mode": "explicit"}));
        // Long retention on the same model: no explicit marker.
        let payload = build_payload(
            &explicit,
            &context,
            &StreamOptions::default(),
            CacheRetention::Long,
        );
        assert_eq!(payload.get("prompt_cache_options"), None);
        // Retention none without the flag: no marker either.
        let payload = build_payload(
            &model(),
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload.get("prompt_cache_options"), None);
    }
    #[test]
    fn reasoning_defaults_and_provider_overrides_are_exact() {
        let context = Context::default();
        // No retention: no cache key and no retention marker at all.
        let payload = build_payload(
            &model(),
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload.get("prompt_cache_key"), None);
        assert_eq!(payload.get("prompt_cache_retention"), None);
        // Effort without summary: auto summary plus encrypted include.
        let mut options = StreamOptions::default();
        options.insert_extra(
            StreamOptionKey::REASONING_EFFORT,
            Value::String("medium".into()),
        );
        let payload = build_payload(&model(), &context, &options, CacheRetention::None);
        assert_eq!(payload["reasoning"]["effort"], "medium");
        assert_eq!(payload["reasoning"]["summary"], "auto");
        assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
        // No effort: plain providers pin the off-map none.
        let payload = build_payload(
            &model(),
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload["reasoning"]["effort"], "none");
        // Copilot skips the implicit off; xAI always requests encrypted reasoning.
        let mut copilot = model();
        copilot.provider = "github-copilot".into();
        let payload = build_payload(
            &copilot,
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload.get("reasoning"), None);
        let mut xai = model();
        xai.provider = "xai".into();
        let payload = build_payload(
            &xai,
            &context,
            &StreamOptions::default(),
            CacheRetention::None,
        );
        assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn session_headers_obey_affinity_and_options_override_auth() {
        let mut options = StreamOptions {
            api_key: Some("key".into()),
            session_id: Some("session".into()),
            ..StreamOptions::default()
        };
        options.headers = Some(BTreeMap::from([(
            "Authorization".into(),
            Some("Bearer custom".into()),
        )]));
        let headers = build_headers(
            &model(),
            &Context::default(),
            &options,
            CacheRetention::Short,
        );
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer custom")
        );
        assert_eq!(
            headers.get("session_id").map(String::as_str),
            Some("session")
        );
        assert_eq!(
            headers.get("x-client-request-id").map(String::as_str),
            Some("session")
        );
    }

    #[test]
    fn failure_formatting_preserves_abort_message_and_prefixes_errors() {
        assert_eq!(
            format_failure(&AdapterFailure::aborted("Request was aborted"), false),
            (ErrorReason::Aborted, "Request was aborted".to_owned())
        );
        assert_eq!(
            format_failure(&AdapterFailure::new("request cancelled"), true),
            (ErrorReason::Aborted, "request cancelled".to_owned())
        );
        assert_eq!(
            format_failure(&AdapterFailure::new("bad request"), false),
            (
                ErrorReason::Error,
                "OpenAI API error: bad request".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn completed_event_produces_exact_done_fields() -> Result<(), String> {
        let (sender, mut stream) =
            super::super::stream_state::ProviderEventSender::channel(event_capacity());
        let mut processor = ResponsesStreamProcessor::new(
            model(),
            AssistantMessage::new("openai-responses", "openai", "gpt-5", 1),
            sender,
            ProcessOptions::default(),
        );
        processor.start().await.map_err(|error| error.to_string())?;
        processor
            .handle(json!({
                "type":"response.output_item.added",
                "output_index":0,
                "item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}
            }))
            .await
            .map_err(|error| error.to_string())?;
        processor
            .handle(json!({
                "type":"response.output_text.delta",
                "output_index":0,
                "delta":"hello"
            }))
            .await
            .map_err(|error| error.to_string())?;
        let terminal = processor
            .handle(json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_1",
                    "status":"completed",
                    "usage":{
                        "input_tokens":9,
                        "output_tokens":2,
                        "total_tokens":11,
                        "input_tokens_details":{"cached_tokens":1,"cache_write_tokens":0},
                        "output_tokens_details":{"reasoning_tokens":0}
                    },
                    "output":[]
                }
            }))
            .await
            .map_err(|error| error.to_string())?;
        assert!(terminal);
        processor.finish().map_err(|error| error.to_string())?;
        drop(processor);

        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.map_err(|error| error.to_string())?);
        }
        let Some(crate::types::AssistantMessageEvent::Done { reason, message }) = events.last()
        else {
            return Err("expected terminal done event".into());
        };
        assert_eq!(*reason, DoneReason::Stop);
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.error_message, None);
        assert_eq!(message.response_id.as_deref(), Some("resp_1"));
        assert_eq!(message.usage.input, 8);
        assert_eq!(message.usage.output, 2);
        assert_eq!(message.usage.cache_read, 1);
        assert_eq!(message.usage.total_tokens, 11);
        assert!(matches!(
            message.content.as_slice(),
            [AssistantContent::Text(text)] if text.text == "hello"
        ));
        Ok(())
    }

    #[test]
    fn done_marker_is_terminal_for_responses_decoder() -> Result<(), String> {
        let mut decoder = DataSseDecoder::default();
        let events = decoder
            .push(
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\ndata: [DONE]\n\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(matches!(
            events.as_slice(),
            [DataSseEvent::Data(_), DataSseEvent::Done]
        ));
        Ok(())
    }
}
