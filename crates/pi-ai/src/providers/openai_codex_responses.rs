//! Native `ChatGPT` Codex Responses adapter with SSE and cached WebSocket transports.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::pending;
use std::io::Cursor;
use std::num::NonZeroUsize;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    USER_AGENT,
};
use reqwest::{Client, Response};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WebSocketMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::shared::responses::{
    ConvertMessagesOptions, ConvertToolsOptions, ProcessOptions, ResponsesStreamProcessor,
    convert_messages, convert_tools,
};
use super::shared::truncate_error_body;
use super::stream_state::ProviderEventSender;
use super::transport::{DataSseDecoder, DataSseEvent, HttpTransport, TransportError};
use crate::provider::{Provider, ProviderError, ProviderResponse, StreamOptionKey, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, DiagnosticCode,
    DiagnosticErrorInfo, ErrorReason, Message, Model, ModelThinkingLevel, StopReason, Tool,
    Transport,
};

const API: &str = "openai-codex-responses";
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_MAX_RETRIES: u32 = 0;
const BASE_DELAY_MS: u64 = 1_000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
const REQUEST_COMPRESSION_ZSTD_LEVEL: i32 = 3;
const OPENAI_BETA_SSE: &str = "responses=experimental";
const OPENAI_BETA_WEBSOCKET: &str = "responses_websockets=2026-02-06";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const WEBSOCKET_CACHE_IDLE_TTL: Duration = Duration::from_mins(5);
const WEBSOCKET_CACHE_MAX_AGE: Duration = Duration::from_mins(55);
const EVENT_CHANNEL_CAPACITY: usize = 64;

static CODEX_TOOL_CALL_PROVIDERS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    ["openai", "openai-codex", "opencode"]
        .into_iter()
        .map(str::to_owned)
        .collect()
});
static WEBSOCKET_CACHE: LazyLock<Mutex<HashMap<String, SessionSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static WEBSOCKET_SSE_FALLBACK_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Native implementation of the `ChatGPT` Codex Responses API.
#[derive(Clone, Debug)]
pub struct OpenAiCodexResponses {
    transport: HttpTransport,
}

impl OpenAiCodexResponses {
    /// Construct the adapter around a configured HTTP client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for OpenAiCodexResponses {
    fn stream(
        &self,
        model: &Model,
        context: crate::types::Context,
        options: StreamOptions,
    ) -> futures::stream::BoxStream<
        'static,
        Result<crate::types::AssistantMessageEvent, ProviderError>,
    > {
        let Some(capacity) = NonZeroUsize::new(EVENT_CHANNEL_CAPACITY) else {
            return futures::stream::empty().boxed();
        };
        let (sender, stream) = ProviderEventSender::channel(capacity);
        let adapter = self.clone();
        let model = model.clone();
        let work = async move {
            Box::pin(adapter.run(model, context, options, sender)).await;
        };
        tokio::spawn(work);
        stream
    }
}

impl OpenAiCodexResponses {
    async fn run(
        self,
        model: Model,
        context: crate::types::Context,
        options: StreamOptions,
        sender: ProviderEventSender,
    ) {
        let request_service_tier = extra_string(&options, StreamOptionKey::SERVICE_TIER);
        let process_options = ProcessOptions {
            request_service_tier,
            apply_service_tier_pricing: true,
            default_service_tier_uses_request: true,
        };
        let initial =
            AssistantMessage::new(API, model.provider.clone(), model.id.clone(), unix_millis());
        let mut processor = ResponsesStreamProcessor::new(
            model.clone(),
            initial,
            sender.clone(),
            process_options.clone(),
        );

        if processor.start().await.is_err() {
            return;
        }

        let result = Box::pin(self.run_started(
            &model,
            &context,
            &options,
            &sender,
            &process_options,
            &mut processor,
        ))
        .await;
        if let Err(failure) = result {
            let reason = if failure.is_cancelled()
                || options
                    .signal
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
            {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            let message = if reason == ErrorReason::Aborted {
                "Request was aborted".to_owned()
            } else {
                failure.message().to_owned()
            };

            if let Some(diagnostic) = failure.diagnostic {
                let mut final_message = processor.message();
                append_diagnostic(&mut final_message, *diagnostic);
                fail_message(&mut final_message, reason, &message);
                let _ = sender.error(reason, final_message).await;
            } else {
                let _ = processor.fail(reason, message).await;
            }
        }
    }

    async fn run_started(
        &self,
        model: &Model,
        context: &crate::types::Context,
        options: &StreamOptions,
        sender: &ProviderEventSender,
        process_options: &ProcessOptions,
        processor: &mut ResponsesStreamProcessor,
    ) -> Result<(), CodexFailure> {
        let prepared = Box::pin(prepare_codex_request(model, context, options)).await?;
        if prepared.configured_transport != Transport::Sse && !prepared.sticky_sse {
            match Box::pin(self.attempt_websocket_with_fallback(
                model,
                options,
                sender,
                process_options,
                processor,
                &prepared,
            ))
            .await?
            {
                WebsocketAttempt::Completed => return Ok(()),
                WebsocketAttempt::FallBackToSse => {}
            }
        }

        let compressed = compress_request_body_zstd(&prepared.body_json)?;
        let mut sse_headers = prepared.sse_headers;
        sse_headers.insert(CONTENT_ENCODING, HeaderValue::from_static("zstd"));
        Box::pin(self.process_sse(model, options, compressed, sse_headers, processor)).await
    }

    async fn attempt_websocket_with_fallback(
        &self,
        model: &Model,
        options: &StreamOptions,
        sender: &ProviderEventSender,
        process_options: &ProcessOptions,
        processor: &mut ResponsesStreamProcessor,
        prepared: &PreparedCodexRequest,
    ) -> Result<WebsocketAttempt, CodexFailure> {
        let mut retried_connection_limit = false;
        loop {
            let attempt = Box::pin(self.process_websocket(
                model,
                options,
                &prepared.body,
                &prepared.websocket_headers,
                prepared.configured_transport,
                processor,
            ))
            .await;
            match attempt {
                Ok(()) => return Ok(WebsocketAttempt::Completed),
                Err(failure) => {
                    let decision = fallback_decision(
                        failure.class,
                        failure.semantic_events,
                        retried_connection_limit,
                    );
                    match decision {
                        FallbackDecision::RetryWebSocket => {
                            retried_connection_limit = true;
                        }
                        FallbackDecision::UseSse => {
                            let diagnostic = transport_diagnostic(
                                prepared.configured_transport,
                                true,
                                false,
                                prepared.body_json.len(),
                                &failure,
                            );
                            mark_sticky_fallback(options.session_id.as_deref());
                            let mut message = processor.message();
                            append_diagnostic(&mut message, diagnostic);
                            *processor = ResponsesStreamProcessor::new(
                                model.clone(),
                                message,
                                sender.clone(),
                                process_options.clone(),
                            );
                            return Ok(WebsocketAttempt::FallBackToSse);
                        }
                        FallbackDecision::Fail => {
                            return if failure.class == FailureClass::Transport {
                                let diagnostic = transport_diagnostic(
                                    prepared.configured_transport,
                                    false,
                                    failure.semantic_events,
                                    prepared.body_json.len(),
                                    &failure,
                                );
                                Err(failure.with_diagnostic(diagnostic))
                            } else {
                                Err(failure)
                            };
                        }
                    }
                }
            }
        }
    }

    async fn process_sse(
        &self,
        model: &Model,
        options: &StreamOptions,
        body: Vec<u8>,
        headers: HeaderMap,
        processor: &mut ResponsesStreamProcessor,
    ) -> Result<(), CodexFailure> {
        let endpoint = resolve_codex_url(&model.base_url);
        let max_retries = options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        let mut last_failure = None;

        for attempt in 0..=max_retries {
            ensure_not_cancelled(options.signal.as_ref())?;
            let request = self
                .transport
                .post(&endpoint)
                .headers(headers.clone())
                .body(body.clone())
                .build()
                .map_err(|error| {
                    CodexFailure::semantic(format!("Failed to build Codex request: {error}"))
                })?;

            let response_result =
                execute_with_header_timeout(&self.transport, request, model, options).await;
            let response = match response_result {
                Ok(response) => response,
                Err(failure) => {
                    if attempt < max_retries && failure.retryable_network() {
                        sleep_with_cancellation(
                            exponential_delay(attempt),
                            options.signal.as_ref(),
                        )
                        .await?;
                        last_failure = Some(failure);
                        continue;
                    }
                    return Err(failure);
                }
            };

            if response.status().is_success() {
                return process_sse_response(response, options.signal.as_ref(), processor).await;
            }

            match classify_sse_http_error(response, options, attempt, max_retries).await? {
                SseHttpOutcome::Retry(failure) => {
                    last_failure = Some(failure);
                }
                SseHttpOutcome::Fail(failure) => return Err(failure),
            }
        }

        Err(last_failure.unwrap_or_else(|| CodexFailure::transport("Failed after retries")))
    }

    async fn process_websocket(
        &self,
        model: &Model,
        options: &StreamOptions,
        full_body: &Value,
        headers: &HeaderMap,
        configured_transport: Transport,
        processor: &mut ResponsesStreamProcessor,
    ) -> Result<(), CodexFailure> {
        let url = resolve_codex_websocket_url(&model.base_url)?;
        let mut acquired = acquire_websocket(
            &url,
            headers,
            options.session_id.as_deref(),
            options.signal.as_ref(),
            options
                .websocket_connect_timeout_ms
                .unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS),
        )
        .await?;

        if let Some(callback) = options.on_response.as_ref()
            && let Some(response) = acquired.handshake.take()
        {
            let metadata = websocket_response_metadata(&response);
            if let Err(error) = callback(&metadata, model).await {
                acquired.release(false).await;
                return Err(CodexFailure::semantic(format!(
                    "response callback failed: {error}"
                )));
            }
        }

        let use_cached_context = matches!(
            configured_transport,
            Transport::WebsocketCached | Transport::Auto
        );
        let request_body = if use_cached_context {
            build_cached_websocket_request_body(&mut acquired.connection, full_body)
        } else {
            full_body.clone()
        };
        let frame = websocket_frame(&request_body)?;
        if let Err(error) = acquired
            .connection
            .socket
            .send(WebSocketMessage::text(frame))
            .await
        {
            acquired.release(false).await;
            return Err(CodexFailure::transport(format!(
                "WebSocket send failed: {error}"
            )));
        }

        let mut semantic_events = false;
        let stream_result = read_websocket_events(
            &mut acquired.connection.socket,
            options.signal.as_ref(),
            options.timeout_ms,
            processor,
            &mut semantic_events,
        )
        .await;

        match stream_result {
            Ok(()) => {
                if use_cached_context {
                    update_continuation(&mut acquired.connection, full_body, model, processor);
                }
                acquired.release(true).await;
                Ok(())
            }
            Err(mut failure) => {
                failure.semantic_events = semantic_events;
                acquired.connection.continuation = None;
                acquired.release(false).await;
                Err(failure)
            }
        }
    }
}

struct PreparedCodexRequest {
    body: Value,
    body_json: Vec<u8>,
    sse_headers: HeaderMap,
    websocket_headers: HeaderMap,
    configured_transport: Transport,
    sticky_sse: bool,
}

enum WebsocketAttempt {
    Completed,
    FallBackToSse,
}

async fn prepare_codex_request(
    model: &Model,
    context: &crate::types::Context,
    options: &StreamOptions,
) -> Result<PreparedCodexRequest, CodexFailure> {
    ensure_not_cancelled(options.signal.as_ref())?;
    let token = options.api_key.as_deref().ok_or_else(|| {
        CodexFailure::semantic(format!("No API key for provider: {}", model.provider))
    })?;
    let account_id = extract_account_id(token)?;
    let mut body = build_request_body(model, context, options);
    if let Some(callback) = options.on_payload.as_ref() {
        callback(&mut body, model)
            .await
            .map_err(|error| CodexFailure::semantic(format!("payload callback failed: {error}")))?;
    }
    if !body.is_object() {
        return Err(CodexFailure::semantic(
            "Codex request payload callback must return a JSON object",
        ));
    }

    let session_key = options.session_id.as_deref().map(clamp_cache_key);
    let websocket_request_id = session_key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let sse_headers = build_sse_headers(
        model.headers.as_ref(),
        options.headers.as_ref(),
        &account_id,
        token,
        session_key.as_deref(),
    )?;
    let websocket_headers = build_websocket_headers(
        model.headers.as_ref(),
        options.headers.as_ref(),
        &account_id,
        token,
        &websocket_request_id,
    )?;
    let body_json = serde_json::to_vec(&body).map_err(|error| {
        CodexFailure::semantic(format!("Failed to encode Codex request: {error}"))
    })?;
    let configured_transport = options.transport.unwrap_or(Transport::Auto);
    let sticky_sse = configured_transport != Transport::Sse
        && sticky_fallback_active(options.session_id.as_deref());
    Ok(PreparedCodexRequest {
        body,
        body_json,
        sse_headers,
        websocket_headers,
        configured_transport,
        sticky_sse,
    })
}

enum SseHttpOutcome {
    Retry(CodexFailure),
    Fail(CodexFailure),
}

async fn classify_sse_http_error(
    response: Response,
    options: &StreamOptions,
    attempt: u32,
    max_retries: u32,
) -> Result<SseHttpOutcome, CodexFailure> {
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let error_text = match read_response_text(response, options.signal.as_ref()).await {
        Ok(error_text) => error_text,
        Err(failure) if attempt < max_retries && failure.retryable_network() => {
            sleep_with_cancellation(exponential_delay(attempt), options.signal.as_ref()).await?;
            return Ok(SseHttpOutcome::Retry(failure));
        }
        Err(failure) => return Err(failure),
    };
    if attempt < max_retries && is_retryable_error(status, &error_text) {
        let delay = retry_after_delay_ms(&response_headers, SystemTime::now()).map_or_else(
            || exponential_delay(attempt),
            |delay| {
                if status == 429 {
                    cap_retry_delay(delay, options.max_retry_delay_ms)
                } else {
                    delay
                }
            },
        );
        sleep_with_cancellation(delay, options.signal.as_ref()).await?;
        return Ok(SseHttpOutcome::Retry(CodexFailure::transport(format!(
            "Codex HTTP {status}: {error_text}"
        ))));
    }
    Ok(SseHttpOutcome::Fail(CodexFailure::semantic(
        parse_error_response(status, &error_text),
    )))
}

async fn execute_with_header_timeout(
    transport: &HttpTransport,
    request: reqwest::Request,
    model: &Model,
    options: &StreamOptions,
) -> Result<Response, CodexFailure> {
    let execute = transport.execute(
        request,
        model,
        options.signal.as_ref(),
        options.on_response.as_ref(),
    );
    if let Some(timeout_ms) = options.timeout_ms.filter(|value| *value > 0) {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), execute).await {
            Ok(result) => map_transport_result(result),
            Err(_) => Err(CodexFailure::transport(format!(
                "Codex SSE response headers timed out after {timeout_ms}ms"
            ))),
        }
    } else {
        map_transport_result(execute.await)
    }
}

fn map_transport_result(
    result: Result<Response, TransportError>,
) -> Result<Response, CodexFailure> {
    result.map_err(|error| match error {
        TransportError::Cancelled => CodexFailure::cancelled(),
        TransportError::Request(error) | TransportError::Body(error) => {
            CodexFailure::transport(format!("Codex request failed: {error}"))
        }
        TransportError::Callback(error) => {
            CodexFailure::semantic(format!("response callback failed: {error}"))
        }
    })
}

async fn process_sse_response(
    response: Response,
    signal: Option<&CancellationToken>,
    processor: &mut ResponsesStreamProcessor,
) -> Result<(), CodexFailure> {
    let mut body = response.bytes_stream();
    let mut decoder = DataSseDecoder::default();
    while let Some(chunk) = next_body_chunk(&mut body, signal).await? {
        let chunk = chunk.map_err(|error| {
            CodexFailure::transport(format!("Codex SSE body read failed: {error}"))
        })?;
        for event in decoder
            .push(&chunk)
            .map_err(|error| CodexFailure::protocol(error.to_string()))?
        {
            if process_sse_event(event, processor).await? {
                return Ok(());
            }
        }
    }
    for event in decoder
        .finish()
        .map_err(|error| CodexFailure::protocol(error.to_string()))?
    {
        if process_sse_event(event, processor).await? {
            return Ok(());
        }
    }
    processor
        .finish()
        .map_err(|error| CodexFailure::protocol(error.to_string()))
}

async fn next_body_chunk<S>(
    body: &mut S,
    signal: Option<&CancellationToken>,
) -> Result<Option<S::Item>, CodexFailure>
where
    S: futures::Stream + Unpin,
{
    tokio::select! {
        () = cancellation(signal) => Err(CodexFailure::cancelled()),
        chunk = body.next() => Ok(chunk),
    }
}

async fn process_sse_event(
    event: DataSseEvent,
    processor: &mut ResponsesStreamProcessor,
) -> Result<bool, CodexFailure> {
    let DataSseEvent::Data(data) = event else {
        return Ok(false);
    };
    let parsed: Value = serde_json::from_str(&data)
        .map_err(|error| CodexFailure::protocol(format!("Invalid Codex SSE JSON: {error}")))?;
    let Some(mapped) = map_codex_event(parsed)? else {
        return Ok(false);
    };
    processor
        .handle(mapped)
        .await
        .map_err(|error| CodexFailure::semantic(error.to_string()))
}

fn map_codex_event(mut event: Value) -> Result<Option<Value>, CodexFailure> {
    let Some(event_type) = event.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(None);
    };
    if event_type == "error" {
        let (code, message) = extract_event_error(&event);
        let detail = message
            .or_else(|| code.clone())
            .unwrap_or_else(|| event.to_string());
        return Err(CodexFailure::api(code, format!("Codex error: {detail}")));
    }
    if event_type == "response.failed" {
        let response_error = event.pointer("/response/error").unwrap_or(&Value::Null);
        let code = response_error
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message = response_error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex response failed")
            .to_owned();
        return Err(CodexFailure::api(code, message));
    }
    if matches!(
        event_type.as_str(),
        "response.done" | "response.completed" | "response.incomplete"
    ) {
        event["type"] = Value::String("response.completed".into());
        if let Some(status) = event.pointer("/response/status").and_then(Value::as_str)
            && !matches!(
                status,
                "completed" | "incomplete" | "failed" | "cancelled" | "queued" | "in_progress"
            )
            && let Some(response) = event.get_mut("response").and_then(Value::as_object_mut)
        {
            response.remove("status");
        }
    }
    Ok(Some(event))
}

fn extract_event_error(event: &Value) -> (Option<String>, Option<String>) {
    let nested = event.get("error").unwrap_or(&Value::Null);
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| nested.get("code").and_then(Value::as_str))
        .map(str::to_owned);
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| nested.get("message").and_then(Value::as_str))
        .map(str::to_owned);
    (code, message)
}

type CodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WebSocketHandshakeResponse = tokio_tungstenite::tungstenite::handshake::client::Response;

struct Continuation {
    request_body: Value,
    response_id: String,
    response_items: Vec<Value>,
}

struct LiveConnection {
    socket: CodexSocket,
    created_at: Instant,
    last_used: Instant,
    id: Uuid,
    continuation: Option<Continuation>,
}

struct SessionSlot {
    reservation: Option<Uuid>,
    connection: Option<LiveConnection>,
}

struct AcquiredWebSocket {
    connection: LiveConnection,
    handshake: Option<WebSocketHandshakeResponse>,
    cache_owner: Option<(String, Uuid)>,
}

impl AcquiredWebSocket {
    async fn release(mut self, keep: bool) {
        let reusable = keep;
        let Some((session_id, reservation)) = self.cache_owner.take() else {
            let _ = self.connection.socket.close(None).await;
            return;
        };
        if !reusable {
            let _ = self.connection.socket.close(None).await;
            release_reservation(&session_id, reservation, None);
            return;
        }
        self.connection.last_used = Instant::now();
        let connection_id = self.connection.id;
        release_reservation(&session_id, reservation, Some(self.connection));
        schedule_idle_expiry(session_id, connection_id);
    }
}

async fn acquire_websocket(
    url: &str,
    headers: &HeaderMap,
    session_id: Option<&str>,
    signal: Option<&CancellationToken>,
    connect_timeout_ms: u64,
) -> Result<AcquiredWebSocket, CodexFailure> {
    let reservation = Uuid::new_v4();
    let mut stale = None;
    let mut cached = None;
    let cache_owner = session_id.and_then(|session_id| {
        let Ok(mut cache) = WEBSOCKET_CACHE.lock() else {
            return None;
        };
        let slot = cache.entry(session_id.to_owned()).or_insert(SessionSlot {
            reservation: None,
            connection: None,
        });
        if slot.reservation.is_some() {
            return None;
        }
        slot.reservation = Some(reservation);
        if let Some(connection) = slot.connection.take() {
            if connection.created_at.elapsed() >= WEBSOCKET_CACHE_MAX_AGE
                || connection.last_used.elapsed() >= WEBSOCKET_CACHE_IDLE_TTL
            {
                stale = Some(connection);
            } else {
                cached = Some(connection);
            }
        }
        Some((session_id.to_owned(), reservation))
    });

    if let Some(mut connection) = stale {
        let _ = connection.socket.close(None).await;
    }
    if let Some(connection) = cached {
        return Ok(AcquiredWebSocket {
            connection,
            handshake: None,
            cache_owner,
        });
    }

    match connect_websocket(url, headers, signal, connect_timeout_ms).await {
        Ok((socket, response)) => Ok(AcquiredWebSocket {
            connection: LiveConnection {
                socket,
                created_at: Instant::now(),
                last_used: Instant::now(),
                id: Uuid::new_v4(),
                continuation: None,
            },
            handshake: Some(response),
            cache_owner,
        }),
        Err(failure) => {
            if let Some((session_id, reservation)) = cache_owner {
                release_reservation(&session_id, reservation, None);
            }
            Err(failure)
        }
    }
}

async fn connect_websocket(
    url: &str,
    headers: &HeaderMap,
    signal: Option<&CancellationToken>,
    connect_timeout_ms: u64,
) -> Result<(CodexSocket, WebSocketHandshakeResponse), CodexFailure> {
    ensure_not_cancelled(signal)?;
    let mut request = url.into_client_request().map_err(|error| {
        CodexFailure::transport(format!("Invalid Codex WebSocket URL: {error}"))
    })?;
    for (name, value) in headers {
        let ws_name =
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_str().as_bytes())
                .map_err(|error| {
                    CodexFailure::semantic(format!("Invalid WebSocket header name: {error}"))
                })?;
        let ws_value =
            tokio_tungstenite::tungstenite::http::HeaderValue::from_bytes(value.as_bytes())
                .map_err(|error| {
                    CodexFailure::semantic(format!("Invalid WebSocket header value: {error}"))
                })?;
        request.headers_mut().insert(ws_name, ws_value);
    }

    let connect = connect_async(request);
    if connect_timeout_ms == 0 {
        tokio::select! {
            () = cancellation(signal) => Err(CodexFailure::cancelled()),
            result = connect => result.map_err(|error| CodexFailure::transport(format!("WebSocket connect failed: {error}"))),
        }
    } else {
        tokio::select! {
            () = cancellation(signal) => Err(CodexFailure::cancelled()),
            result = tokio::time::timeout(Duration::from_millis(connect_timeout_ms), connect) => {
                match result {
                    Ok(result) => result.map_err(|error| CodexFailure::transport(format!("WebSocket connect failed: {error}"))),
                    Err(_) => Err(CodexFailure::transport(format!("WebSocket connect timeout after {connect_timeout_ms}ms"))),
                }
            }
        }
    }
}

fn release_reservation(session_id: &str, reservation: Uuid, connection: Option<LiveConnection>) {
    let Ok(mut cache) = WEBSOCKET_CACHE.lock() else {
        return;
    };
    let remove = if let Some(slot) = cache.get_mut(session_id) {
        if slot.reservation == Some(reservation) {
            slot.reservation = None;
            slot.connection = connection;
        }
        slot.reservation.is_none() && slot.connection.is_none()
    } else {
        false
    };
    if remove {
        cache.remove(session_id);
    }
}

fn schedule_idle_expiry(session_id: String, connection_id: Uuid) {
    tokio::spawn(async move {
        tokio::time::sleep(WEBSOCKET_CACHE_IDLE_TTL).await;
        let Ok(mut cache) = WEBSOCKET_CACHE.lock() else {
            return;
        };
        let should_remove = cache.get(&session_id).is_some_and(|slot| {
            slot.reservation.is_none()
                && slot.connection.as_ref().is_some_and(|connection| {
                    connection.id == connection_id
                        && connection.last_used.elapsed() >= WEBSOCKET_CACHE_IDLE_TTL
                })
        });
        if should_remove {
            cache.remove(&session_id);
        }
    });
}

async fn read_websocket_events(
    socket: &mut CodexSocket,
    signal: Option<&CancellationToken>,
    idle_timeout_ms: Option<u64>,
    processor: &mut ResponsesStreamProcessor,
    semantic_events: &mut bool,
) -> Result<(), CodexFailure> {
    loop {
        let message = next_websocket_message(socket, signal, idle_timeout_ms).await?;
        let Some(message) = message else {
            return Err(CodexFailure::transport(
                "WebSocket stream closed before response.completed",
            ));
        };
        match message {
            WebSocketMessage::Text(text) => {
                if process_websocket_json(text.as_str(), processor, semantic_events).await? {
                    return Ok(());
                }
            }
            WebSocketMessage::Binary(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|error| {
                    CodexFailure::protocol(format!("Invalid Codex WebSocket UTF-8: {error}"))
                })?;
                if process_websocket_json(text, processor, semantic_events).await? {
                    return Ok(());
                }
            }
            WebSocketMessage::Close(frame) => {
                let detail = frame.map_or_else(
                    || "WebSocket closed".to_owned(),
                    |frame| {
                        let reason = if frame.reason.is_empty() && u16::from(frame.code) == 1009 {
                            "message too big".to_owned()
                        } else {
                            frame.reason.to_string()
                        };
                        format!("WebSocket closed {} {reason}", u16::from(frame.code))
                            .trim()
                            .to_owned()
                    },
                );
                return Err(CodexFailure::transport(detail));
            }
            WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {}
        }
    }
}

async fn next_websocket_message(
    socket: &mut CodexSocket,
    signal: Option<&CancellationToken>,
    idle_timeout_ms: Option<u64>,
) -> Result<Option<WebSocketMessage>, CodexFailure> {
    let idle = async {
        if let Some(timeout_ms) = idle_timeout_ms.filter(|value| *value > 0) {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        } else {
            pending::<()>().await;
        }
    };
    tokio::pin!(idle);
    tokio::select! {
        () = cancellation(signal) => Err(CodexFailure::cancelled()),
        () = &mut idle => Err(CodexFailure::transport(format!(
            "WebSocket idle timeout after {}ms",
            idle_timeout_ms.unwrap_or(0)
        ))),
        message = socket.next() => match message {
            Some(Ok(message)) => Ok(Some(message)),
            Some(Err(error)) => Err(CodexFailure::transport(format!("WebSocket read failed: {error}"))),
            None => Ok(None),
        },
    }
}

async fn process_websocket_json(
    text: &str,
    processor: &mut ResponsesStreamProcessor,
    semantic_events: &mut bool,
) -> Result<bool, CodexFailure> {
    let parsed: Value = serde_json::from_str(text).map_err(|error| {
        CodexFailure::protocol(format!("Invalid Codex WebSocket JSON: {error}"))
    })?;
    let Some(mapped) = map_codex_event(parsed)? else {
        return Ok(false);
    };
    *semantic_events = true;
    processor
        .handle(mapped)
        .await
        .map_err(|error| CodexFailure::semantic(error.to_string()))
}

fn build_cached_websocket_request_body(connection: &mut LiveConnection, body: &Value) -> Value {
    let Some(continuation) = connection.continuation.as_ref() else {
        return body.clone();
    };
    let Some(delta) = cached_websocket_input_delta(body, continuation) else {
        connection.continuation = None;
        return body.clone();
    };
    if continuation.response_id.is_empty() {
        connection.continuation = None;
        return body.clone();
    }
    let mut request = body.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert(
            "previous_response_id".into(),
            Value::String(continuation.response_id.clone()),
        );
        object.insert("input".into(), Value::Array(delta));
    }
    request
}

fn cached_websocket_input_delta(body: &Value, continuation: &Continuation) -> Option<Vec<Value>> {
    if body_without_input(body) != body_without_input(&continuation.request_body) {
        return None;
    }
    let current = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut baseline = continuation
        .request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    baseline.extend(continuation.response_items.clone());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

fn body_without_input(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
    }
    body
}

fn update_continuation(
    connection: &mut LiveConnection,
    request_body: &Value,
    model: &Model,
    processor: &ResponsesStreamProcessor,
) {
    let message = processor.message();
    let Some(response_id) = message.response_id.clone() else {
        connection.continuation = None;
        return;
    };
    let context = crate::types::Context {
        system_prompt: None,
        messages: vec![Message::Assistant(message)],
        tools: None,
    };
    let response_items = convert_messages(
        model,
        &context,
        &CODEX_TOOL_CALL_PROVIDERS,
        &ConvertMessagesOptions {
            include_system_prompt: false,
            deferred_tools: BTreeMap::new(),
        },
    )
    .into_iter()
    .filter(|item| item.get("type").and_then(Value::as_str) != Some("function_call_output"))
    .collect();
    connection.continuation = Some(Continuation {
        request_body: request_body.clone(),
        response_id,
        response_items,
    });
}

fn websocket_frame(body: &Value) -> Result<String, CodexFailure> {
    let mut object = body
        .as_object()
        .cloned()
        .ok_or_else(|| CodexFailure::semantic("Codex WebSocket payload must be a JSON object"))?;
    object.insert("type".into(), Value::String("response.create".into()));
    serde_json::to_string(&Value::Object(object)).map_err(|error| {
        CodexFailure::semantic(format!("Failed to encode WebSocket request: {error}"))
    })
}

fn build_request_body(
    model: &Model,
    context: &crate::types::Context,
    options: &StreamOptions,
) -> Value {
    let (immediate_tools, deferred_tools) =
        split_deferred_tools(context, compat_bool(model, "supportsToolSearch", false));
    let input = convert_messages(
        model,
        context,
        &CODEX_TOOL_CALL_PROVIDERS,
        &ConvertMessagesOptions {
            include_system_prompt: false,
            deferred_tools,
        },
    );
    let mut body = codex_base_request_fields(model, context, options, input);
    apply_codex_optional_request_fields(&mut body, model, options, &immediate_tools);
    Value::Object(body)
}

fn codex_base_request_fields(
    model: &Model,
    context: &crate::types::Context,
    options: &StreamOptions,
    input: Vec<Value>,
) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    body.insert(
        "instructions".into(),
        Value::String(
            context
                .system_prompt
                .clone()
                .filter(|prompt| !prompt.is_empty())
                .unwrap_or_else(|| "You are a helpful assistant.".into()),
        ),
    );
    body.insert("input".into(), Value::Array(input));
    body.insert(
        "text".into(),
        json!({"verbosity": extra_string(options, StreamOptionKey::TEXT_VERBOSITY)
            .unwrap_or_else(|| "low".into())}),
    );
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    body.insert(
        "tool_choice".into(),
        Value::String(
            extra_string(options, StreamOptionKey::TOOL_CHOICE).unwrap_or_else(|| "auto".into()),
        ),
    );
    body.insert("parallel_tool_calls".into(), Value::Bool(true));
    body
}

fn apply_codex_optional_request_fields(
    body: &mut Map<String, Value>,
    model: &Model,
    options: &StreamOptions,
    immediate_tools: &[Tool],
) {
    if let Some(session_id) = options.session_id.as_deref() {
        body.insert(
            "prompt_cache_key".into(),
            Value::String(clamp_cache_key(session_id)),
        );
    }
    if let Some(temperature) = options.temperature.and_then(serde_json::Number::from_f64) {
        body.insert("temperature".into(), Value::Number(temperature));
    }
    if let Some(service_tier) = extra_string(options, StreamOptionKey::SERVICE_TIER) {
        body.insert("service_tier".into(), Value::String(service_tier));
    }
    if !immediate_tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_tools(
                immediate_tools,
                ConvertToolsOptions {
                    strict: None,
                    defer_loading: false,
                },
            )),
        );
    }
    if let Some(reasoning_effort) = extra_string(options, StreamOptionKey::REASONING_EFFORT) {
        let mapped = map_reasoning_effort(model, &reasoning_effort);
        body.insert(
            "reasoning".into(),
            json!({
                "effort": mapped,
                "summary": extra_string(options, StreamOptionKey::REASONING_SUMMARY)
                    .unwrap_or_else(|| "auto".into()),
            }),
        );
    }
}

fn split_deferred_tools(
    context: &crate::types::Context,
    enabled: bool,
) -> (Vec<Tool>, BTreeMap<String, Tool>) {
    let mut unique = Vec::<Tool>::new();
    for tool in context.tools.as_deref().unwrap_or_default() {
        if let Some(existing) = unique.iter_mut().find(|entry| entry.name == tool.name) {
            *existing = tool.clone();
        } else {
            unique.push(tool.clone());
        }
    }
    if !enabled {
        return (unique, BTreeMap::new());
    }

    let mut used = BTreeSet::new();
    let mut deferred_names = BTreeSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(tool_call) = block {
                        used.insert(tool_call.name.clone());
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in result.added_tool_names.as_deref().unwrap_or_default() {
                    if !used.contains(name) {
                        deferred_names.insert(name.clone());
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = BTreeMap::new();
    for tool in unique {
        if deferred_names.contains(&tool.name) {
            deferred.insert(tool.name.clone(), tool);
        } else {
            immediate.push(tool);
        }
    }
    (immediate, deferred)
}

fn map_reasoning_effort(model: &Model, effort: &str) -> String {
    let level = match effort {
        "none" => ModelThinkingLevel::Off,
        "minimal" => ModelThinkingLevel::Minimal,
        "low" => ModelThinkingLevel::Low,
        "medium" => ModelThinkingLevel::Medium,
        "high" => ModelThinkingLevel::High,
        "xhigh" => ModelThinkingLevel::Xhigh,
        "max" => ModelThinkingLevel::Max,
        other => return other.to_owned(),
    };
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|mapping| mapping.get(&level))
    {
        // An explicit null mapping behaves like a missing entry: the
        // reference `??` fallback resolves to the literal effort, and a
        // null `off` resolves to the literal "none".
        Some(Some(mapped)) => mapped.clone(),
        _ => {
            if effort == "none" {
                "none"
            } else {
                effort
            }
            .to_owned()
        }
    }
}

fn compat_bool(model: &Model, name: &str, default: bool) -> bool {
    model
        .compat
        .as_ref()
        .and_then(|compat| compat.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn extra_string(options: &StreamOptions, key: StreamOptionKey) -> Option<String> {
    options
        .extra_value(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url.trim()
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_owned()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn resolve_codex_websocket_url(base_url: &str) -> Result<String, CodexFailure> {
    let url = resolve_codex_url(base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if url.starts_with("wss://") || url.starts_with("ws://") {
        Ok(url)
    } else {
        Err(CodexFailure::semantic(format!(
            "Unsupported Codex base URL: {base_url}"
        )))
    }
}

fn extract_account_id(token: &str) -> Result<String, CodexFailure> {
    let mut parts = token.split('.');
    let Some(_header) = parts.next() else {
        return Err(CodexFailure::semantic(
            "Failed to extract accountId from token",
        ));
    };
    let Some(payload) = parts.next() else {
        return Err(CodexFailure::semantic(
            "Failed to extract accountId from token",
        ));
    };
    if parts.next().is_none() || parts.next().is_some() {
        return Err(CodexFailure::semantic(
            "Failed to extract accountId from token",
        ));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload))
        .map_err(|_| CodexFailure::semantic("Failed to extract accountId from token"))?;
    let claims: Value = serde_json::from_slice(&decoded)
        .map_err(|_| CodexFailure::semantic("Failed to extract accountId from token"))?;
    claims
        .get(JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CodexFailure::semantic("Failed to extract accountId from token"))
}

fn build_sse_headers(
    model_headers: Option<&BTreeMap<String, String>>,
    option_headers: Option<&BTreeMap<String, Option<String>>>,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> Result<HeaderMap, CodexFailure> {
    let mut headers = build_base_headers(model_headers, option_headers, account_id, token)?;
    headers.insert("openai-beta", HeaderValue::from_static(OPENAI_BETA_SSE));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(session_id) = session_id {
        insert_header(&mut headers, "session-id", session_id)?;
        insert_header(&mut headers, "x-client-request-id", session_id)?;
    }
    Ok(headers)
}

fn build_websocket_headers(
    model_headers: Option<&BTreeMap<String, String>>,
    option_headers: Option<&BTreeMap<String, Option<String>>>,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> Result<HeaderMap, CodexFailure> {
    let mut headers = build_base_headers(model_headers, option_headers, account_id, token)?;
    headers.remove(ACCEPT);
    headers.remove(CONTENT_TYPE);
    headers.remove("openai-beta");
    headers.insert(
        "openai-beta",
        HeaderValue::from_static(OPENAI_BETA_WEBSOCKET),
    );
    insert_header(&mut headers, "x-client-request-id", request_id)?;
    insert_header(&mut headers, "session-id", request_id)?;
    Ok(headers)
}

fn build_base_headers(
    model_headers: Option<&BTreeMap<String, String>>,
    option_headers: Option<&BTreeMap<String, Option<String>>>,
    account_id: &str,
    token: &str,
) -> Result<HeaderMap, CodexFailure> {
    let mut headers = HeaderMap::new();
    if let Some(model_headers) = model_headers {
        for (name, value) in model_headers {
            insert_header(&mut headers, name, value)?;
        }
    }
    if let Some(option_headers) = option_headers {
        for (name, value) in option_headers {
            let header_name = parse_header_name(name)?;
            if let Some(value) = value {
                headers.insert(header_name, parse_header_value(value)?);
            } else {
                headers.remove(header_name);
            }
        }
    }
    insert_header(
        &mut headers,
        AUTHORIZATION.as_str(),
        &format!("Bearer {token}"),
    )?;
    insert_header(&mut headers, "chatgpt-account-id", account_id)?;
    headers.insert("originator", HeaderValue::from_static("pi"));
    insert_header(
        &mut headers,
        USER_AGENT.as_str(),
        &format!("pi ({}; {})", std::env::consts::OS, std::env::consts::ARCH),
    )?;
    Ok(headers)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), CodexFailure> {
    headers.insert(parse_header_name(name)?, parse_header_value(value)?);
    Ok(())
}

fn parse_header_name(name: &str) -> Result<HeaderName, CodexFailure> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| CodexFailure::semantic(format!("Invalid header name {name:?}: {error}")))
}

fn parse_header_value(value: &str) -> Result<HeaderValue, CodexFailure> {
    HeaderValue::from_str(value)
        .map_err(|error| CodexFailure::semantic(format!("Invalid header value: {error}")))
}

fn clamp_cache_key(value: &str) -> String {
    value.chars().take(64).collect()
}

fn compress_request_body_zstd(body: &[u8]) -> Result<Vec<u8>, CodexFailure> {
    zstd::stream::encode_all(Cursor::new(body), REQUEST_COMPRESSION_ZSTD_LEVEL).map_err(|error| {
        CodexFailure::semantic(format!("Failed to compress Codex request: {error}"))
    })
}

fn websocket_response_metadata(response: &WebSocketHandshakeResponse) -> ProviderResponse {
    let mut headers = BTreeMap::new();
    for (name, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers
                .entry(name.as_str().to_owned())
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(value);
                })
                .or_insert_with(|| value.to_owned());
        }
    }
    ProviderResponse {
        status: response.status().as_u16(),
        headers,
    }
}

async fn read_response_text(
    response: Response,
    signal: Option<&CancellationToken>,
) -> Result<String, CodexFailure> {
    let raw = HttpTransport::read_error_body(response, signal)
        .await
        .map_err(|error| match error {
            TransportError::Cancelled => CodexFailure::cancelled(),
            TransportError::Request(error) | TransportError::Body(error) => {
                CodexFailure::transport(format!("Codex error body read failed: {error}"))
            }
            TransportError::Callback(error) => {
                CodexFailure::semantic(format!("response callback failed: {error}"))
            }
        })?;
    Ok(truncate_error_body(&raw))
}

fn parse_error_response(status: u16, raw: &str) -> String {
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let code = error
        .and_then(|error| error.get("code").or_else(|| error.get("type")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status == 429
        || [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|needle| code.eq_ignore_ascii_case(needle))
    {
        let plan = error
            .and_then(|error| error.get("plan_type"))
            .and_then(Value::as_str)
            .map(|plan| format!(" ({} plan)", plan.to_lowercase()))
            .unwrap_or_default();
        return format!("You have hit your ChatGPT usage limit{plan}.");
    }
    error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .or_else(|| (!raw.is_empty()).then_some(raw))
        .unwrap_or("Request failed")
        .to_owned()
}

fn is_retryable_error(status: u16, error_text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(error_text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    let normalized = error_text.to_ascii_lowercase();
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "overloaded",
        "service unavailable",
        "service-unavailable",
        "serviceunavailable",
        "upstream connect",
        "connection refused",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_terminal_rate_limit_error(error_text: &str) -> bool {
    let normalized = error_text.to_ascii_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn retry_after_delay_ms(headers: &HeaderMap, now: SystemTime) -> Option<u64> {
    if let Some(value) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
    {
        return Some(nonnegative_f64_millis(value));
    }
    let value = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
    {
        return Some(nonnegative_f64_millis(seconds * 1_000.0));
    }
    let target = parse_http_date(value)?;
    Some(
        target
            .duration_since(now)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    )
}

fn nonnegative_f64_millis(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.floor().to_string().parse::<u64>().unwrap_or(u64::MAX)
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    let mut parts = value.split_ascii_whitespace();
    let _weekday = parts.next()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts.next()?.parse::<i64>().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour = clock.next()?.parse::<i64>().ok()?;
    let minute = clock.next()?.parse::<i64>().ok()?;
    let second = clock.next()?.parse::<i64>().ok()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn cap_retry_delay(delay_ms: u64, configured_max: Option<u64>) -> u64 {
    let max = configured_max.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max > 0 { delay_ms.min(max) } else { delay_ms }
}

fn exponential_delay(attempt: u32) -> u64 {
    BASE_DELAY_MS.saturating_mul(2_u64.saturating_pow(attempt))
}

async fn sleep_with_cancellation(
    delay_ms: u64,
    signal: Option<&CancellationToken>,
) -> Result<(), CodexFailure> {
    tokio::select! {
        () = cancellation(signal) => Err(CodexFailure::cancelled()),
        () = tokio::time::sleep(Duration::from_millis(delay_ms)) => Ok(()),
    }
}

async fn cancellation(signal: Option<&CancellationToken>) {
    if let Some(signal) = signal {
        signal.cancelled().await;
    } else {
        pending::<()>().await;
    }
}

fn ensure_not_cancelled(signal: Option<&CancellationToken>) -> Result<(), CodexFailure> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(CodexFailure::cancelled())
    } else {
        Ok(())
    }
}

fn sticky_fallback_active(session_id: Option<&str>) -> bool {
    let Some(session_id) = session_id else {
        return false;
    };
    WEBSOCKET_SSE_FALLBACK_SESSIONS
        .lock()
        .is_ok_and(|sessions| sessions.contains(session_id))
}

fn mark_sticky_fallback(session_id: Option<&str>) {
    let Some(session_id) = session_id else {
        return;
    };
    if let Ok(mut sessions) = WEBSOCKET_SSE_FALLBACK_SESSIONS.lock() {
        sessions.insert(session_id.to_owned());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureClass {
    Cancelled,
    Transport,
    Api,
    ApiConnectionLimit,
    Protocol,
    Semantic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FallbackDecision {
    RetryWebSocket,
    UseSse,
    Fail,
}

fn fallback_decision(
    class: FailureClass,
    semantic_events: bool,
    connection_limit_already_retried: bool,
) -> FallbackDecision {
    if class == FailureClass::ApiConnectionLimit && !semantic_events {
        if connection_limit_already_retried {
            FallbackDecision::UseSse
        } else {
            FallbackDecision::RetryWebSocket
        }
    } else if class == FailureClass::Transport && !semantic_events {
        FallbackDecision::UseSse
    } else {
        FallbackDecision::Fail
    }
}

#[derive(Debug)]
struct CodexFailure {
    class: FailureClass,
    message: String,
    code: Option<String>,
    semantic_events: bool,
    diagnostic: Option<Box<AssistantMessageDiagnostic>>,
}

impl CodexFailure {
    fn cancelled() -> Self {
        Self::new(FailureClass::Cancelled, "Request was aborted")
    }

    fn transport(message: impl Into<String>) -> Self {
        Self::new(FailureClass::Transport, message)
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::new(FailureClass::Protocol, message)
    }

    fn semantic(message: impl Into<String>) -> Self {
        Self::new(FailureClass::Semantic, message)
    }

    fn api(code: Option<String>, message: impl Into<String>) -> Self {
        let class = if code.as_deref() == Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE) {
            FailureClass::ApiConnectionLimit
        } else {
            FailureClass::Api
        };
        Self {
            class,
            message: message.into(),
            code,
            semantic_events: false,
            diagnostic: None,
        }
    }

    fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            code: None,
            semantic_events: false,
            diagnostic: None,
        }
    }

    fn message(&self) -> &str {
        &self.message
    }

    fn is_cancelled(&self) -> bool {
        self.class == FailureClass::Cancelled
    }

    fn retryable_network(&self) -> bool {
        self.class == FailureClass::Transport
    }

    fn with_diagnostic(mut self, diagnostic: AssistantMessageDiagnostic) -> Self {
        self.diagnostic = Some(Box::new(diagnostic));
        self
    }
}
impl std::fmt::Display for CodexFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexFailure {}

fn transport_diagnostic(
    configured_transport: Transport,
    has_fallback: bool,
    events_emitted: bool,
    request_bytes: usize,
    failure: &CodexFailure,
) -> AssistantMessageDiagnostic {
    let mut details = Map::new();
    details.insert(
        "configuredTransport".into(),
        Value::String(transport_name(configured_transport).into()),
    );
    if has_fallback {
        details.insert("fallbackTransport".into(), Value::String("sse".into()));
    }
    details.insert("eventsEmitted".into(), Value::Bool(events_emitted));
    details.insert(
        "phase".into(),
        Value::String(
            if events_emitted {
                "after_message_stream_start"
            } else {
                "before_message_stream_start"
            }
            .into(),
        ),
    );
    details.insert(
        "requestBytes".into(),
        Value::Number(u64::try_from(request_bytes).unwrap_or(u64::MAX).into()),
    );
    AssistantMessageDiagnostic {
        kind: "provider_transport_failure".into(),
        timestamp: unix_millis(),
        error: Some(DiagnosticErrorInfo {
            name: Some(failure_class_name(failure.class).into()),
            message: failure.message.clone(),
            stack: None,
            code: failure.code.clone().map(DiagnosticCode::String),
        }),
        details: Some(details),
    }
}

fn append_diagnostic(message: &mut AssistantMessage, diagnostic: AssistantMessageDiagnostic) {
    message
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}

fn fail_message(message: &mut AssistantMessage, reason: ErrorReason, error: &str) {
    message.stop_reason = match reason {
        ErrorReason::Aborted => StopReason::Aborted,
        ErrorReason::Error => StopReason::Error,
    };
    message.error_message = Some(error.to_owned());
}

const fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Sse => "sse",
        Transport::Websocket => "websocket",
        Transport::WebsocketCached => "websocket-cached",
        Transport::Auto => "auto",
    }
}

const fn failure_class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::Cancelled => "AbortError",
        FailureClass::Transport => "WebSocketTransportError",
        FailureClass::Api | FailureClass::ApiConnectionLimit => "CodexApiError",
        FailureClass::Protocol => "CodexProtocolError",
        FailureClass::Semantic => "CodexError",
    }
}

fn unix_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_with_claim(account_id: &str) -> String {
        let payload = json!({
            JWT_CLAIM_PATH: {"chatgpt_account_id": account_id}
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap_or_default());
        format!("aaa.{encoded}.bbb")
    }

    #[test]
    fn null_level_mappings_fall_back_to_literal_effort() {
        use crate::types::{ModelCost, ModelInput};

        let mut model = Model {
            id: "gpt-5.5".into(),
            name: "GPT-5.5".into(),
            api: API.into(),
            provider: "openai-codex".into(),
            base_url: DEFAULT_CODEX_BASE_URL.into(),
            reasoning: true,
            thinking_level_map: Some(BTreeMap::from([(ModelThinkingLevel::Off, None)])),
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        };
        // A null `off` resolves to the literal "none", matching `??`.
        assert_eq!(map_reasoning_effort(&model, "none"), "none".to_owned());
        // A null level resolves to the literal effort, not a dropped field.
        assert_eq!(map_reasoning_effort(&model, "high"), "high".to_owned());
        // Present mappings still win over the literal.
        model.thinking_level_map = Some(BTreeMap::from([(
            ModelThinkingLevel::High,
            Some("very-high".to_owned()),
        )]));
        assert_eq!(
            map_reasoning_effort(&model, "high"),
            "very-high".to_owned()
        );
    }
    #[test]
    fn extracts_chatgpt_account_id_from_base64url_jwt() -> Result<(), CodexFailure> {
        assert_eq!(
            extract_account_id(&token_with_claim("acc_test"))?,
            "acc_test"
        );
        assert!(extract_account_id("not-a-jwt").is_err());
        assert!(extract_account_id("a.e30.b").is_err());
        Ok(())
    }

    #[test]
    fn zstd_level_three_body_roundtrips() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"model":"gpt-5.5","store":false,"stream":true}"#;
        let compressed = compress_request_body_zstd(body)?;
        assert_ne!(compressed, body);
        let decoded = zstd::stream::decode_all(Cursor::new(compressed))?;
        assert_eq!(decoded, body);
        Ok(())
    }

    #[test]
    fn fallback_is_pre_semantic_and_connection_limit_retries_once() {
        assert_eq!(
            fallback_decision(FailureClass::Transport, false, false),
            FallbackDecision::UseSse
        );
        assert_eq!(
            fallback_decision(FailureClass::Transport, true, false),
            FallbackDecision::Fail
        );
        assert_eq!(
            fallback_decision(FailureClass::ApiConnectionLimit, false, false),
            FallbackDecision::RetryWebSocket
        );
        assert_eq!(
            fallback_decision(FailureClass::ApiConnectionLimit, false, true),
            FallbackDecision::UseSse
        );
        assert_eq!(
            fallback_decision(FailureClass::Protocol, false, false),
            FallbackDecision::Fail
        );
    }

    #[test]
    fn cached_delta_requires_equal_non_input_and_exact_prefix() {
        let continuation = Continuation {
            request_body: json!({"model":"gpt","store":false,"input":[{"role":"user","content":"one"}]}),
            response_id: "resp_1".into(),
            response_items: vec![json!({"role":"assistant","content":"two"})],
        };
        let eligible = json!({
            "model":"gpt",
            "store":false,
            "input":[
                {"role":"user","content":"one"},
                {"role":"assistant","content":"two"},
                {"role":"user","content":"three"}
            ]
        });
        assert_eq!(
            cached_websocket_input_delta(&eligible, &continuation),
            Some(vec![json!({"role":"user","content":"three"})])
        );
        let changed_options = json!({
            "model":"gpt",
            "store":true,
            "input":eligible["input"].clone()
        });
        assert!(cached_websocket_input_delta(&changed_options, &continuation).is_none());
        let wrong_prefix = json!({
            "model":"gpt",
            "store":false,
            "input":[{"role":"user","content":"different"}]
        });
        assert!(cached_websocket_input_delta(&wrong_prefix, &continuation).is_none());
    }

    #[test]
    fn codex_header_names_match_wire_contract() -> Result<(), CodexFailure> {
        let token = token_with_claim("acc_test");
        let sse = build_sse_headers(None, None, "acc_test", &token, Some("session"))?;
        assert_eq!(
            sse.get("session-id").and_then(|value| value.to_str().ok()),
            Some("session")
        );
        assert_eq!(
            sse.get("x-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("session")
        );
        assert_eq!(
            sse.get("openai-beta").and_then(|value| value.to_str().ok()),
            Some(OPENAI_BETA_SSE)
        );
        assert!(sse.get("session_id").is_none());
        assert!(sse.contains_key("chatgpt-account-id"));
        assert!(sse.contains_key("originator"));

        let request_id = Uuid::new_v4().to_string();
        let websocket = build_websocket_headers(None, None, "acc_test", &token, &request_id)?;
        assert_eq!(
            websocket
                .get("openai-beta")
                .and_then(|value| value.to_str().ok()),
            Some(OPENAI_BETA_WEBSOCKET)
        );
        assert_eq!(
            websocket
                .get("session-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id.as_str())
        );
        assert!(
            Uuid::parse_str(
                websocket
                    .get("x-client-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
            )
            .is_ok()
        );
        Ok(())
    }
}
