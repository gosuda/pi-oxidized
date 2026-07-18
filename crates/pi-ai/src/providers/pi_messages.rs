//! Native pi-messages HTTP and SSE adapter.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Request, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::shared::{parse_streaming_json, truncate_error_body};
use super::stream_state::ProviderEventSender;
use super::transport::{HttpTransport, SseLineBuffer, TransportError};
use crate::provider::{Provider, ProviderError, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, Context,
    DiagnosticCode, DiagnosticErrorInfo, DoneReason, ErrorReason, Model, StopReason, TextContent,
    ThinkingContent, ToolCall, Usage,
};

const EVENT_CHANNEL_CAPACITY: NonZeroUsize = NonZeroUsize::MIN;

/// Streams the native pi message protocol over HTTP.
#[derive(Clone, Debug)]
pub struct PiMessages {
    transport: HttpTransport,
}

impl PiMessages {
    /// Create an adapter using an already configured HTTP client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for PiMessages {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let adapter = self.clone();
        let model = model.clone();
        let (sender, stream) = ProviderEventSender::channel(EVENT_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            let mut converter = EventConverter::new(&model);
            if sender.start(converter.snapshot()).await.is_err() {
                return;
            }

            if let Err(failure) = adapter
                .run(&model, context, &options, &sender, &mut converter)
                .await
            {
                let reason = if failure.aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                let error = converter.fail(reason, failure.message, failure.diagnostic);
                let _result = sender.error(reason, error).await;
            }
        });
        stream
    }
}

impl PiMessages {
    async fn run(
        &self,
        model: &Model,
        context: Context,
        options: &StreamOptions,
        sender: &ProviderEventSender,
        converter: &mut EventConverter,
    ) -> Result<(), AdapterFailure> {
        let api_key = options
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                AdapterFailure::error(format!(
                    "No API key provided for provider \"{}\"",
                    model.provider
                ))
            })?;
        let url = endpoint(model, debug_enabled(options))?;
        let mut payload = build_payload(model, &context, options);
        if let Some(callback) = &options.on_payload {
            callback(&mut payload, model)
                .await
                .map_err(|error| AdapterFailure::error(error.to_string()))?;
        }
        let request = self.build_request(model, options, api_key, url.clone(), &payload)?;
        let response = self
            .transport
            .execute(
                request,
                model,
                options.signal.as_ref(),
                options.on_response.as_ref(),
            )
            .await
            .map_err(|error| map_transport_error(error, options))?;

        if !response.status().is_success() {
            return Err(self.response_failure(response, model, &url, options).await);
        }

        self.consume(response, model, options, sender, converter)
            .await
    }

    fn build_request(
        &self,
        model: &Model,
        options: &StreamOptions,
        api_key: &str,
        url: Url,
        payload: &Value,
    ) -> Result<Request, AdapterFailure> {
        let headers = request_headers(model, options, api_key)?;
        let mut builder = self.transport.post(url).headers(headers).json(payload);
        if let Some(timeout_ms) = options.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        }
        builder
            .build()
            .map_err(|error| AdapterFailure::error(format!("failed to build request: {error}")))
    }

    async fn consume(
        &self,
        response: Response,
        model: &Model,
        options: &StreamOptions,
        sender: &ProviderEventSender,
        converter: &mut EventConverter,
    ) -> Result<(), AdapterFailure> {
        let mut body = response.bytes_stream();
        let mut decoder = PiMessagesSseDecoder::default();
        loop {
            let next = if let Some(signal) = &options.signal {
                tokio::select! {
                    () = signal.cancelled() => return Err(AdapterFailure::aborted("request cancelled")),
                    item = body.next() => item,
                }
            } else {
                body.next().await
            };

            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|error| {
                failure_for_options(format!("response body failed: {error}"), options)
            })?;
            for data in decoder
                .push(&chunk)
                .map_err(|error| AdapterFailure::error(error.to_string()))?
            {
                if process_data(&data, &model.provider, sender, converter).await? {
                    return Ok(());
                }
            }
        }

        for data in decoder
            .finish()
            .map_err(|error| AdapterFailure::error(error.to_string()))?
        {
            if process_data(&data, &model.provider, sender, converter).await? {
                return Ok(());
            }
        }
        let AssistantMessageEvent::Error { reason, error } =
            converter.missing_terminal(&model.provider)
        else {
            return Err(AdapterFailure::error(
                "pi-messages missing-terminal conversion failed",
            ));
        };
        sender
            .error(reason, error)
            .await
            .map_err(|error| AdapterFailure::error(error.to_string()))
    }

    async fn response_failure(
        &self,
        response: Response,
        model: &Model,
        url: &Url,
        options: &StreamOptions,
    ) -> AdapterFailure {
        let status = response.status();
        let body = match HttpTransport::read_error_body(response, options.signal.as_ref()).await {
            Ok(body) => body,
            Err(error) => return map_transport_error(error, options),
        };
        let parsed = serde_json::from_str::<Value>(&body).ok();
        let error = parsed.as_ref().and_then(|value| value.get("error"));
        let message = error
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .map_or_else(|| truncate_error_body(&body), ToOwned::to_owned);
        let code = error
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str);
        let status_text = status.canonical_reason().unwrap_or("");
        let code_suffix = code.map_or_else(String::new, |code| format!(" ({code})"));
        let formatted = format!(
            "{} {}: {}{}",
            status.as_u16(),
            status_text,
            message,
            code_suffix
        );

        let mut details = Map::new();
        details.insert("version".into(), Value::from(1));
        details.insert("provider".into(), Value::String(model.provider.clone()));
        details.insert("model".into(), Value::String(model.id.clone()));
        details.insert("url".into(), Value::String(url.to_string()));
        details.insert("status".into(), Value::from(status.as_u16()));
        details.insert("statusText".into(), Value::String(status_text.into()));
        if let Some(error) = error {
            details.insert("error".into(), error.clone());
        } else {
            details.insert("body".into(), Value::String(truncate_error_body(&body)));
        }
        details.insert("timestampMs".into(), Value::from(timestamp_ms()));
        let diagnostic = AssistantMessageDiagnostic {
            kind: "pi_messages_response_failure".into(),
            timestamp: timestamp_ms(),
            error: Some(DiagnosticErrorInfo {
                name: Some("PiMessagesResponseError".into()),
                message: formatted.clone(),
                stack: None,
                code: code.map(|code| DiagnosticCode::String(code.into())),
            }),
            details: Some(details),
        };
        AdapterFailure::error(formatted).with_diagnostic(diagnostic)
    }
}

async fn process_data(
    data: &str,
    provider: &str,
    sender: &ProviderEventSender,
    converter: &mut EventConverter,
) -> Result<bool, AdapterFailure> {
    let event: PiMessagesEvent = serde_json::from_str(data).map_err(|error| {
        AdapterFailure::error(format!("invalid {provider} stream event: {error}"))
    })?;
    match converter.apply(event)? {
        ConvertedEvent::Ignore => Ok(false),
        ConvertedEvent::Event(event) => {
            sender
                .event(*event)
                .await
                .map_err(|error| AdapterFailure::error(error.to_string()))?;
            Ok(false)
        }
        ConvertedEvent::Done(reason, message) => {
            sender
                .done(reason, *message)
                .await
                .map_err(|error| AdapterFailure::error(error.to_string()))?;
            Ok(true)
        }
        ConvertedEvent::Error(reason, error) => {
            sender
                .error(reason, *error)
                .await
                .map_err(|error| AdapterFailure::error(error.to_string()))?;
            Ok(true)
        }
    }
}

fn endpoint(model: &Model, debug: bool) -> Result<Url, AdapterFailure> {
    let base = model.base_url.trim_end_matches('/');
    let mut url = Url::parse(&format!("{base}/messages"))
        .map_err(|error| AdapterFailure::error(format!("invalid pi-messages URL: {error}")))?;
    if debug {
        url.query_pairs_mut().append_pair("debug", "1");
    }
    Ok(url)
}

fn debug_enabled(options: &StreamOptions) -> bool {
    options
        .extra
        .get("debug")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut request_options = Map::new();
    if let Some(temperature) = options.temperature {
        request_options.insert("temperature".into(), Value::from(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        request_options.insert("maxTokens".into(), Value::from(max_tokens));
    }
    if let Some(reasoning) = options.extra.get("reasoning") {
        request_options.insert("reasoning".into(), reasoning.clone());
    }
    if let Some(cache_retention) = resolve_cache_retention(options)
        && let Ok(value) = serde_json::to_value(cache_retention)
    {
        request_options.insert("cacheRetention".into(), value);
    }
    if let Some(session_id) = &options.session_id {
        request_options.insert("sessionId".into(), Value::String(session_id.clone()));
    }
    if let Some(tool_choice) = options.extra.get("toolChoice") {
        request_options.insert("toolChoice".into(), tool_choice.clone());
    }

    serde_json::json!({
        "model": model.id,
        "context": context,
        "options": request_options,
    })
}

fn resolve_cache_retention(options: &StreamOptions) -> Option<crate::types::CacheRetention> {
    options.cache_retention.or_else(|| {
        options
            .env
            .as_ref()
            .and_then(|env| env.get("PI_CACHE_RETENTION"))
            .filter(|value| value.as_str() == "long")
            .map(|_| crate::types::CacheRetention::Long)
    })
}

fn request_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<HeaderMap, AdapterFailure> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|error| AdapterFailure::error(format!("invalid API key header: {error}")))?;
    headers.insert(AUTHORIZATION, authorization);

    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header(&mut headers, name, value)?;
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            let name = parse_header_name(name)?;
            if let Some(value) = value {
                let value = HeaderValue::from_str(value).map_err(|error| {
                    AdapterFailure::error(format!("invalid header value for {name}: {error}"))
                })?;
                headers.insert(name, value);
            } else {
                headers.remove(name);
            }
        }
    }
    Ok(headers)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), AdapterFailure> {
    let name = parse_header_name(name)?;
    let value = HeaderValue::from_str(value).map_err(|error| {
        AdapterFailure::error(format!("invalid header value for {name}: {error}"))
    })?;
    headers.insert(name, value);
    Ok(())
}

fn parse_header_name(name: &str) -> Result<HeaderName, AdapterFailure> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| AdapterFailure::error(format!("invalid header name {name:?}: {error}")))
}

fn map_transport_error(error: TransportError, options: &StreamOptions) -> AdapterFailure {
    match error {
        TransportError::Cancelled => AdapterFailure::aborted("request cancelled"),
        TransportError::Request(error) => {
            failure_for_options(format!("request failed: {error}"), options)
        }
        TransportError::Callback(error) => {
            failure_for_options(format!("response callback failed: {error}"), options)
        }
        TransportError::Body(error) => {
            failure_for_options(format!("response body failed: {error}"), options)
        }
    }
}

fn failure_for_options(message: String, options: &StreamOptions) -> AdapterFailure {
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        AdapterFailure::aborted(message)
    } else {
        AdapterFailure::error(message)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct AdapterFailure {
    message: String,
    aborted: bool,
    diagnostic: Option<Box<AssistantMessageDiagnostic>>,
}

impl AdapterFailure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
            diagnostic: None,
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: true,
            diagnostic: None,
        }
    }

    fn with_diagnostic(mut self, diagnostic: AssistantMessageDiagnostic) -> Self {
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }
}

impl From<ProtocolError> for AdapterFailure {
    fn from(error: ProtocolError) -> Self {
        Self::error(error.to_string())
    }
}

/// pi-messages deliberately uses only the first `data:` line in each SSE event.
#[derive(Debug, Default)]
struct PiMessagesSseDecoder {
    lines: SseLineBuffer,
    first_data: Option<String>,
}

impl PiMessagesSseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, PiMessagesDecodeError> {
        let mut events = Vec::new();
        for line in self.lines.push(chunk) {
            if let Some(event) = self.push_line(&line)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<String>, PiMessagesDecodeError> {
        let mut events = Vec::new();
        for line in self.lines.finish() {
            if let Some(event) = self.push_line(&line)? {
                events.push(event);
            }
        }
        if let Some(event) = self.dispatch() {
            events.push(event);
        }
        Ok(events)
    }

    fn push_line(&mut self, line: &[u8]) -> Result<Option<String>, PiMessagesDecodeError> {
        if line.is_empty() {
            return Ok(self.dispatch());
        }
        if self.first_data.is_none() && line.starts_with(b"data:") {
            let data = std::str::from_utf8(&line[5..])?.trim().to_owned();
            self.first_data = Some(data);
        }
        Ok(None)
    }

    fn dispatch(&mut self) -> Option<String> {
        self.first_data
            .take()
            .filter(|data| !data.is_empty() && data != "[DONE]")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("pi-messages SSE data is not valid UTF-8")]
struct PiMessagesDecodeError(#[from] std::str::Utf8Error);

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum PiMessagesEvent {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "text_start")]
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: u64,
    },
    #[serde(rename = "text_delta")]
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        delta: String,
    },
    #[serde(rename = "text_end")]
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        content: String,
        #[serde(rename = "contentSignature")]
        content_signature: Option<String>,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: u64,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        delta: String,
    },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        content: String,
        #[serde(rename = "contentSignature")]
        content_signature: Option<String>,
        redacted: Option<bool>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        delta: String,
    },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        #[serde(rename = "contentIndex")]
        content_index: u64,
        #[serde(rename = "toolCall")]
        tool_call: Box<ToolCall>,
    },
    #[serde(rename = "done")]
    Done {
        reason: DoneReason,
        usage: Usage,
        #[serde(rename = "responseId")]
        response_id: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
    #[serde(rename = "error")]
    Error {
        reason: ErrorReason,
        usage: Usage,
        #[serde(rename = "errorMessage")]
        error_message: Option<String>,
        #[serde(rename = "responseId")]
        response_id: Option<String>,
        rewrite: Option<PiMessagesRewriteImpact>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PiMessagesRewriteImpact {
    policy_id: String,
    policy_version: u64,
    changed: bool,
    token_count_change: i64,
    message_count_change: i64,
    system_prompt_changed: bool,
}

enum ConvertedEvent {
    Ignore,
    Event(Box<AssistantMessageEvent>),
    Done(DoneReason, Box<AssistantMessage>),
    Error(ErrorReason, Box<AssistantMessage>),
}

struct EventConverter {
    partial: AssistantMessage,
    tool_json: BTreeMap<u64, String>,
    server_started: bool,
    terminal: bool,
}

impl EventConverter {
    fn new(model: &Model) -> Self {
        Self {
            partial: AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                timestamp_ms(),
            ),
            tool_json: BTreeMap::new(),
            server_started: false,
            terminal: false,
        }
    }

    fn snapshot(&self) -> AssistantMessage {
        self.partial.clone()
    }

    fn apply(&mut self, event: PiMessagesEvent) -> Result<ConvertedEvent, ProtocolError> {
        if self.terminal {
            return Err(ProtocolError::AfterTerminal);
        }
        if matches!(event, PiMessagesEvent::Start) {
            if self.server_started {
                return Err(ProtocolError::DuplicateStart);
            }
            self.server_started = true;
            return Ok(ConvertedEvent::Ignore);
        }
        if !self.server_started {
            return Err(ProtocolError::BeforeStart);
        }

        match event {
            PiMessagesEvent::Start => Ok(ConvertedEvent::Ignore),
            text @ (PiMessagesEvent::TextStart { .. }
            | PiMessagesEvent::TextDelta { .. }
            | PiMessagesEvent::TextEnd { .. }) => self.apply_text(text),
            thinking @ (PiMessagesEvent::ThinkingStart { .. }
            | PiMessagesEvent::ThinkingDelta { .. }
            | PiMessagesEvent::ThinkingEnd { .. }) => self.apply_thinking(thinking),
            tool @ (PiMessagesEvent::ToolCallStart { .. }
            | PiMessagesEvent::ToolCallDelta { .. }
            | PiMessagesEvent::ToolCallEnd { .. }) => self.apply_tool(tool),
            terminal @ (PiMessagesEvent::Done { .. } | PiMessagesEvent::Error { .. }) => {
                self.apply_terminal(terminal)
            }
        }
    }

    fn apply_text(&mut self, event: PiMessagesEvent) -> Result<ConvertedEvent, ProtocolError> {
        match event {
            PiMessagesEvent::TextStart { content_index } => {
                self.start_block(content_index, AssistantContent::Text(TextContent::new("")))?;
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::TextStart {
                        content_index,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::TextDelta {
                content_index,
                delta,
            } => {
                self.text_mut(content_index)?.text.push_str(&delta);
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::TextDelta {
                        content_index,
                        delta,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::TextEnd {
                content_index,
                content,
                content_signature,
            } => {
                let block = self.text_mut(content_index)?;
                block.text.clone_from(&content);
                block.text_signature = content_signature;
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::TextEnd {
                        content_index,
                        content,
                        partial: self.snapshot(),
                    },
                )))
            }
            _ => Err(ProtocolError::WrongBlockKind(0)),
        }
    }

    fn apply_thinking(&mut self, event: PiMessagesEvent) -> Result<ConvertedEvent, ProtocolError> {
        match event {
            PiMessagesEvent::ThinkingStart { content_index } => {
                self.start_block(
                    content_index,
                    AssistantContent::Thinking(ThinkingContent::new("")),
                )?;
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ThinkingStart {
                        content_index,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::ThinkingDelta {
                content_index,
                delta,
            } => {
                self.thinking_mut(content_index)?.thinking.push_str(&delta);
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ThinkingDelta {
                        content_index,
                        delta,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::ThinkingEnd {
                content_index,
                content,
                content_signature,
                redacted,
            } => {
                let block = self.thinking_mut(content_index)?;
                block.thinking.clone_from(&content);
                block.thinking_signature = content_signature;
                block.redacted = redacted;
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        content,
                        partial: self.snapshot(),
                    },
                )))
            }
            _ => Err(ProtocolError::WrongBlockKind(0)),
        }
    }

    fn apply_tool(&mut self, event: PiMessagesEvent) -> Result<ConvertedEvent, ProtocolError> {
        match event {
            PiMessagesEvent::ToolCallStart {
                content_index,
                id,
                tool_name,
            } => {
                self.start_block(
                    content_index,
                    AssistantContent::ToolCall(ToolCall::new(id, tool_name, Map::new())),
                )?;
                self.tool_json.insert(content_index, String::new());
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ToolCallStart {
                        content_index,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::ToolCallDelta {
                content_index,
                delta,
            } => {
                let json = self
                    .tool_json
                    .get_mut(&content_index)
                    .ok_or(ProtocolError::MissingToolJson(content_index))?;
                json.push_str(&delta);
                let arguments = parse_streaming_json(json);
                self.tool_call_mut(content_index)?.arguments = arguments;
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ToolCallDelta {
                        content_index,
                        delta,
                        partial: self.snapshot(),
                    },
                )))
            }
            PiMessagesEvent::ToolCallEnd {
                content_index,
                tool_call,
            } => {
                let block = self.content_mut(content_index)?;
                if !matches!(block, AssistantContent::ToolCall(_)) {
                    return Err(ProtocolError::WrongBlockKind(content_index));
                }
                *block = AssistantContent::ToolCall(*tool_call.clone());
                self.tool_json.remove(&content_index);
                Ok(ConvertedEvent::Event(Box::new(
                    AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call: *tool_call,
                        partial: self.snapshot(),
                    },
                )))
            }
            _ => Err(ProtocolError::WrongBlockKind(0)),
        }
    }

    fn apply_terminal(&mut self, event: PiMessagesEvent) -> Result<ConvertedEvent, ProtocolError> {
        match event {
            PiMessagesEvent::Done {
                reason,
                usage,
                response_id,
                rewrite,
            } => {
                self.terminal = true;
                self.partial.stop_reason = match reason {
                    DoneReason::Stop => StopReason::Stop,
                    DoneReason::Length => StopReason::Length,
                    DoneReason::ToolUse => StopReason::ToolUse,
                };
                self.partial.usage = usage;
                self.partial.response_id = response_id;
                append_rewrite_diagnostic(&mut self.partial, rewrite)?;
                Ok(ConvertedEvent::Done(reason, Box::new(self.snapshot())))
            }
            PiMessagesEvent::Error {
                reason,
                usage,
                error_message,
                response_id,
                rewrite,
            } => {
                self.terminal = true;
                self.partial.stop_reason = match reason {
                    ErrorReason::Aborted => StopReason::Aborted,
                    ErrorReason::Error => StopReason::Error,
                };
                self.partial.usage = usage;
                self.partial.error_message = error_message;
                self.partial.response_id = response_id;
                append_rewrite_diagnostic(&mut self.partial, rewrite)?;
                Ok(ConvertedEvent::Error(reason, Box::new(self.snapshot())))
            }
            _ => Err(ProtocolError::WrongBlockKind(0)),
        }
    }

    fn fail(
        &mut self,
        reason: ErrorReason,
        message: String,
        diagnostic: Option<Box<AssistantMessageDiagnostic>>,
    ) -> AssistantMessage {
        self.terminal = true;
        self.partial.stop_reason = match reason {
            ErrorReason::Aborted => StopReason::Aborted,
            ErrorReason::Error => StopReason::Error,
        };
        self.partial.error_message = Some(message);
        if let Some(diagnostic) = diagnostic {
            self.partial
                .diagnostics
                .get_or_insert_with(Vec::new)
                .push(*diagnostic);
        }
        self.snapshot()
    }

    fn missing_terminal(&mut self, provider: &str) -> AssistantMessageEvent {
        let message = format!("{provider} stream ended without a terminal event");
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: self.fail(ErrorReason::Error, message, None),
        }
    }

    fn start_block(&mut self, index: u64, content: AssistantContent) -> Result<(), ProtocolError> {
        let expected = u64::try_from(self.partial.content.len())
            .map_err(|_| ProtocolError::ContentIndexOverflow)?;
        if index != expected {
            return Err(ProtocolError::UnexpectedContentIndex { index, expected });
        }
        self.partial.content.push(content);
        Ok(())
    }

    fn content_mut(&mut self, index: u64) -> Result<&mut AssistantContent, ProtocolError> {
        let index = usize::try_from(index).map_err(|_| ProtocolError::ContentIndexOverflow)?;
        self.partial
            .content
            .get_mut(index)
            .ok_or(ProtocolError::MissingContent(index))
    }

    fn text_mut(&mut self, index: u64) -> Result<&mut TextContent, ProtocolError> {
        match self.content_mut(index)? {
            AssistantContent::Text(content) => Ok(content),
            _ => Err(ProtocolError::WrongBlockKind(index)),
        }
    }

    fn thinking_mut(&mut self, index: u64) -> Result<&mut ThinkingContent, ProtocolError> {
        match self.content_mut(index)? {
            AssistantContent::Thinking(content) => Ok(content),
            _ => Err(ProtocolError::WrongBlockKind(index)),
        }
    }

    fn tool_call_mut(&mut self, index: u64) -> Result<&mut ToolCall, ProtocolError> {
        match self.content_mut(index)? {
            AssistantContent::ToolCall(content) => Ok(content),
            _ => Err(ProtocolError::WrongBlockKind(index)),
        }
    }
}

fn append_rewrite_diagnostic(
    message: &mut AssistantMessage,
    rewrite: Option<PiMessagesRewriteImpact>,
) -> Result<(), ProtocolError> {
    let Some(rewrite) = rewrite else {
        return Ok(());
    };
    let details = serde_json::to_value(rewrite)
        .map_err(|error| ProtocolError::Rewrite(Box::new(error)))?
        .as_object()
        .cloned()
        .ok_or(ProtocolError::RewriteObject)?;
    message
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(AssistantMessageDiagnostic {
            kind: "pi_messages_rewrite".into(),
            timestamp: timestamp_ms(),
            error: None,
            details: Some(details),
        });
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum ProtocolError {
    #[error("pi-messages event arrived before server start")]
    BeforeStart,
    #[error("pi-messages server sent duplicate start")]
    DuplicateStart,
    #[error("pi-messages event arrived after terminal event")]
    AfterTerminal,
    #[error("pi-messages content index {index} did not match next index {expected}")]
    UnexpectedContentIndex { index: u64, expected: u64 },
    #[error("pi-messages content index is too large")]
    ContentIndexOverflow,
    #[error("pi-messages content block {0} does not exist")]
    MissingContent(usize),
    #[error("pi-messages content block {0} has the wrong type")]
    WrongBlockKind(u64),
    #[error("pi-messages tool JSON state for block {0} does not exist")]
    MissingToolJson(u64),
    #[error("pi-messages rewrite diagnostic is invalid: {0}")]
    Rewrite(Box<serde_json::Error>),
    #[error("pi-messages rewrite diagnostic is not an object")]
    RewriteObject,
}

fn timestamp_ms() -> i64 {
    let millis = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => 0,
    };
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput, UserMessage, UserMessageContent};
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "radius-model".into(),
            name: "Radius".into(),
            api: "pi-messages".into(),
            provider: "radius".into(),
            base_url: "http://127.0.0.1:9/".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4_096,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn empty_usage_json() -> Value {
        json!({
            "input": 3,
            "output": 2,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 5,
            "cost": {
                "input": 0.0,
                "output": 0.0,
                "cacheRead": 0.0,
                "cacheWrite": 0.0,
                "total": 0.0
            }
        })
    }

    fn rewrite_json() -> Value {
        json!({
            "policyId": "policy",
            "policyVersion": 2,
            "changed": true,
            "tokenCountChange": -1,
            "messageCountChange": 0,
            "systemPromptChanged": false
        })
    }

    #[test]
    fn decoder_uses_only_first_data_line_at_every_chunk_split()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture =
            b"event: ignored\r\ndata: {\"type\":\"start\"}\r\ndata: {\"type\":\"error\"}\r\n\r\ndata: [DONE]\n\n";
        for split in 0..=fixture.len() {
            let mut decoder = PiMessagesSseDecoder::default();
            let mut actual = decoder.push(&fixture[..split])?;
            actual.extend(decoder.push(&fixture[split..])?);
            actual.extend(decoder.finish()?);
            assert_eq!(actual, vec!["{\"type\":\"start\"}"], "split {split}");
        }
        Ok(())
    }

    #[test]
    fn request_payload_passes_native_context_and_options_through() {
        let context = Context {
            system_prompt: Some("system".into()),
            messages: vec![crate::types::Message::User(UserMessage::new(
                UserMessageContent::Text("hello".into()),
                7,
            ))],
            tools: None,
        };
        let mut options = StreamOptions {
            temperature: Some(0.25),
            max_tokens: Some(321),
            session_id: Some("session-1".into()),
            ..StreamOptions::default()
        };
        options.extra.insert("reasoning".into(), json!("high"));
        options.extra.insert("toolChoice".into(), json!("required"));

        assert_eq!(
            build_payload(&model(), &context, &options),
            json!({
                "model": "radius-model",
                "context": {
                    "systemPrompt": "system",
                    "messages": [{"role": "user", "content": "hello", "timestamp": 7}]
                },
                "options": {
                    "temperature": 0.25,
                    "maxTokens": 321,
                    "reasoning": "high",
                    "sessionId": "session-1",
                    "toolChoice": "required"
                }
            })
        );
    }

    #[test]
    fn converter_applies_tool_json_and_server_terminal() -> Result<(), Box<dyn std::error::Error>> {
        let mut converter = EventConverter::new(&model());
        assert!(matches!(
            converter.apply(serde_json::from_value(json!({"type": "start"}))?)?,
            ConvertedEvent::Ignore
        ));
        let _start = converter.apply(serde_json::from_value(json!({
            "type": "toolcall_start", "contentIndex": 0, "id": "call-1", "toolName": "read"
        }))?)?;
        let delta = converter.apply(serde_json::from_value(json!({
            "type": "toolcall_delta", "contentIndex": 0, "delta": "{\"path\":\"README"
        }))?)?;
        let ConvertedEvent::Event(event) = delta else {
            return Err("expected tool delta".into());
        };
        let AssistantMessageEvent::ToolCallDelta { partial, .. } = *event else {
            return Err("expected tool delta event".into());
        };
        assert_eq!(
            serde_json::to_value(&partial.content[0])?["arguments"],
            json!({"path": "README"})
        );

        let terminal = converter.apply(serde_json::from_value(json!({
            "type": "done",
            "reason": "toolUse",
            "usage": empty_usage_json(),
            "responseId": "response-1",
            "rewrite": rewrite_json()
        }))?)?;
        let ConvertedEvent::Done(DoneReason::ToolUse, message) = terminal else {
            return Err("expected done".into());
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.response_id.as_deref(), Some("response-1"));
        assert_eq!(message.diagnostics.as_ref().map(Vec::len), Some(1));
        Ok(())
    }

    #[test]
    fn eof_without_server_terminal_is_an_error_event() -> Result<(), Box<dyn std::error::Error>> {
        let mut converter = EventConverter::new(&model());
        let _start = converter.apply(serde_json::from_value(json!({"type": "start"}))?)?;
        let _text_start = converter.apply(serde_json::from_value(json!({
            "type": "text_start", "contentIndex": 0
        }))?)?;
        let _delta = converter.apply(serde_json::from_value(json!({
            "type": "text_delta", "contentIndex": 0, "delta": "partial"
        }))?)?;

        let AssistantMessageEvent::Error { reason, error } = converter.missing_terminal("radius")
        else {
            return Err("expected error event".into());
        };
        assert_eq!(reason, ErrorReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        assert_eq!(
            error.error_message.as_deref(),
            Some("radius stream ended without a terminal event")
        );
        assert_eq!(serde_json::to_value(&error.content[0])?["text"], "partial");
        Ok(())
    }

    #[test]
    fn request_has_messages_path_headers_and_debug_query() -> Result<(), Box<dyn std::error::Error>>
    {
        let adapter = PiMessages::new(Client::new());
        let mut options = StreamOptions::default();
        options.extra.insert("debug".into(), Value::Bool(true));
        let url = endpoint(&model(), debug_enabled(&options))?;
        let request = adapter.build_request(
            &model(),
            &options,
            "secret",
            url,
            &json!({"model": "radius-model"}),
        )?;
        assert_eq!(request.url().path(), "/messages");
        assert_eq!(request.url().query(), Some("debug=1"));
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer secret");
        assert_eq!(request.headers()[ACCEPT], "text/event-stream");
        Ok(())
    }
}
