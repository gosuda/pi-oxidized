//! Provider conformance for the `OpenAI` Codex Responses adapter.
//!
//! Replays source-derived SSE, WebSocket, and level-3 zstd fixtures against
//! loopback servers using the real adapter. Shared support is read-only; the
//! WebSocket loopback helper is local to this file.

use std::{
    collections::BTreeMap,
    env,
    io::Cursor,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use futures::{SinkExt, StreamExt};
use pi_ai::{
    Context, Model, Provider, StreamOptions, Transport,
    providers::OpenAiCodexResponses,
    types::{AssistantContent, AssistantMessageEvent, ErrorReason},
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message as WsMessage,
        handshake::server::{
            Callback as WsCallback, ErrorResponse as WsErrorResponse, Request as WsRequest,
            Response as WsResponse,
        },
    },
};
use tokio_util::sync::CancellationToken;

#[path = "support/mod.rs"]
mod support;

use support::{
    golden::{load_jsonl, normalize_timestamps},
    http::{LocalHttpServer, ResponseChunk, ResponseSpec},
};

const FIXTURE_REL: &str = "tests/fixtures/providers/openai-codex-responses/cases.jsonl";
const TRANSPORT_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "accept-encoding",
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
];

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test(flavor = "current_thread")]
async fn openai_codex_responses_conformance() -> TestResult {
    let fixture_path = fixture_path()?;
    let records = load_jsonl(&fixture_path)?;
    if records.is_empty() {
        return Err("openai-codex-responses fixture is empty".into());
    }

    for (index, record) in records.into_iter().enumerate() {
        let name = record
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_owned();
        run_case(record)
            .await
            .map_err(|error| format!("case {index} ({name}) failed: {error}"))?;
    }
    Ok(())
}

async fn run_case(record: Value) -> TestResult {
    let name = record
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unnamed")
        .to_owned();
    let mode = record.get("mode").and_then(Value::as_str).unwrap_or("sse");
    match mode {
        "websocket" => run_websocket_case(&name, &record).await,
        "ws-reject-then-sse" => run_ws_fallback_sticky_case(&name, &record).await,
        _ => run_sse_case(&name, &record).await,
    }
}

async fn run_sse_case(name: &str, record: &Value) -> TestResult {
    let expected_terminal = expected_terminal(record)?;
    let abort_after = record
        .get("abortAfterEventType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let advance_ms = record.get("advanceMs").and_then(Value::as_u64).unwrap_or(0);

    let mut specs = vec![response_spec_from_value(
        record.get("response").ok_or("missing response")?,
    )?];
    if let Some(secondary) = record.get("secondaryResponse") {
        specs.push(response_spec_from_value(secondary)?);
    }
    let server = LocalHttpServer::start(specs).await?;
    let base_url = server.base_url();
    let model = model_from_case(record, &base_url)?;
    let context = context_from_case(record)?;
    let options = options_from_case(record, abort_after.is_some());
    let cancel = options.signal.clone();
    let adapter = OpenAiCodexResponses::new(Client::new());

    let events = collect_with_optional_abort_and_advance(
        adapter.stream(&model, context, options),
        cancel,
        abort_after.as_deref(),
        advance_ms,
    )
    .await?;

    let requests = server.shutdown().await?;
    if requests.is_empty() {
        return Err(format!("{name}: expected at least one HTTP request").into());
    }
    let expected_request = record.get("request").ok_or("missing request")?;
    for request in &requests {
        assert_http_request(name, request, expected_request)?;
    }
    assert_stream_shape(name, &events, expected_terminal)?;
    assert_expected_or_source_semantics(name, record, &events, expected_terminal)?;
    Ok(())
}

async fn run_ws_fallback_sticky_case(name: &str, record: &Value) -> TestResult {
    let expected_terminal = expected_terminal(record)?;
    let sticky_second = record
        .get("stickySecondCall")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // LocalHttpServer never upgrades WebSocket connections. The adapter's auto
    // transport first issues a WS handshake GET against the same base URL (which
    // consumes one queued HTTP response), then falls back to SSE POST. After the
    // first fallback, sticky mode skips WS on the next call.
    let sse = response_spec_from_value(record.get("response").ok_or("missing response")?)?;
    let ws_probe = ResponseSpec::bytes(
        StatusCode::BAD_REQUEST,
        b"websocket upgrades are not supported by the SSE fixture server",
    );
    let mut specs = vec![ws_probe.clone(), sse.clone()];
    if sticky_second {
        specs.push(sse);
    }
    let server = LocalHttpServer::start(specs).await?;
    let base_url = server.base_url();
    let model = model_from_case(record, &base_url)?;
    let context = context_from_case(record)?;
    let adapter = OpenAiCodexResponses::new(Client::new());

    let first =
        collect_events(adapter.stream(&model, context.clone(), options_from_case(record, false)))
            .await?;
    assert_stream_shape(name, &first, expected_terminal)?;
    assert_expected_or_source_semantics(name, record, &first, expected_terminal)?;

    if sticky_second {
        let second =
            collect_events(adapter.stream(&model, context, options_from_case(record, false)))
                .await?;
        assert_stream_shape(&format!("{name}/sticky"), &second, expected_terminal)?;
        assert_expected_or_source_semantics(
            &format!("{name}/sticky"),
            record,
            &second,
            expected_terminal,
        )?;
    }

    let requests = server.shutdown().await?;
    let expected_request = record.get("request").ok_or("missing request")?;
    let posts: Vec<_> = requests
        .iter()
        .filter(|request| request.method == axum::http::Method::POST)
        .collect();
    if posts.is_empty() {
        return Err(format!("{name}: expected SSE fallback POST request").into());
    }
    if sticky_second && posts.len() < 2 {
        return Err(format!(
            "{name}: sticky second call expected a second SSE POST, got {}",
            posts.len()
        )
        .into());
    }
    for request in posts {
        assert_http_request(name, request, expected_request)?;
    }
    Ok(())
}

async fn run_websocket_case(name: &str, record: &Value) -> TestResult {
    let expected_terminal = expected_terminal(record)?;
    let normalizations = normalizations(record);
    let frames = record
        .get("wsFrames")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let server = LocalWsServer::start(frames).await?;
    let base_url = server.base_url();
    let model = model_from_case(record, &base_url)?;
    let context = context_from_case(record)?;
    let options = options_from_case(record, false);
    let adapter = OpenAiCodexResponses::new(Client::new());
    let events = collect_events(adapter.stream(&model, context, options)).await?;
    let capture = server.shutdown().await?;
    assert_ws_request(
        name,
        &capture,
        record.get("request").ok_or("missing request")?,
        &normalizations,
    )?;
    assert_stream_shape(name, &events, expected_terminal)?;
    assert_expected_or_source_semantics(name, record, &events, expected_terminal)?;
    Ok(())
}

async fn collect_events(
    mut stream: impl StreamExt<Item = Result<AssistantMessageEvent, pi_ai::ProviderError>> + Unpin,
) -> Result<Vec<AssistantMessageEvent>, Box<dyn std::error::Error + Send + Sync>> {
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        events.push(item.map_err(|error| error.to_string())?);
    }
    Ok(events)
}

async fn collect_with_optional_abort_and_advance(
    mut stream: impl StreamExt<Item = Result<AssistantMessageEvent, pi_ai::ProviderError>> + Unpin,
    cancel: Option<CancellationToken>,
    abort_after: Option<&str>,
    advance_ms: u64,
) -> Result<Vec<AssistantMessageEvent>, Box<dyn std::error::Error + Send + Sync>> {
    let mut events = Vec::new();
    let mut aborted = false;
    let collect = async {
        while let Some(item) = stream.next().await {
            let event = item.map_err(|error| error.to_string())?;
            if !aborted
                && let Some(marker) = abort_after
                && event_type_name(&event) == marker
                && let Some(token) = cancel.as_ref()
            {
                token.cancel();
                aborted = true;
            }
            events.push(event);
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };

    if advance_ms > 0 {
        // Bounded real-time wait: Cargo.toml forbids enabling tokio test-util here.
        let sleeper = tokio::time::sleep(Duration::from_millis(advance_ms));
        tokio::pin!(collect);
        tokio::pin!(sleeper);
        loop {
            tokio::select! {
                result = &mut collect => {
                    result?;
                    break;
                }
                () = &mut sleeper => {
                    // Keep polling the stream after the retry delay elapses.
                }
            }
        }
    } else {
        collect.await?;
    }
    Ok(events)
}

fn assert_stream_shape(
    name: &str,
    events: &[AssistantMessageEvent],
    expected_terminal: &str,
) -> TestResult {
    if events.is_empty() {
        return Err(format!("{name}: stream produced no events").into());
    }
    if !matches!(events.first(), Some(AssistantMessageEvent::Start { .. })) {
        return Err(format!("{name}: first event must be start").into());
    }
    let terminals = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )
        })
        .count();
    if terminals != 1 {
        return Err(format!("{name}: expected exactly one done|error, found {terminals}").into());
    }
    match (expected_terminal, events.last()) {
        ("done", Some(AssistantMessageEvent::Done { .. }))
        | ("error", Some(AssistantMessageEvent::Error { .. })) => Ok(()),
        ("done", other) => Err(format!("{name}: expected done terminal, got {other:?}").into()),
        ("error", other) => Err(format!("{name}: expected error terminal, got {other:?}").into()),
        (other, _) => Err(format!("{name}: unknown expectedTerminal {other}").into()),
    }
}

fn assert_expected_or_source_semantics(
    name: &str,
    record: &Value,
    events: &[AssistantMessageEvent],
    expected_terminal: &str,
) -> TestResult {
    if let Some(expected) = record.get("expectedEvents").and_then(Value::as_array)
        && !expected.is_empty()
    {
        let actual = events_to_normalized_json(events)?;
        if actual != *expected {
            return Err(format!(
                "{name}: expectedEvents mismatch\nactual={}\nexpected={}",
                Value::Array(actual),
                Value::Array(expected.clone())
            )
            .into());
        }
        return Ok(());
    }
    assert_source_semantics(name, record, events, expected_terminal)
}

fn assert_source_semantics(
    name: &str,
    record: &Value,
    events: &[AssistantMessageEvent],
    expected_terminal: &str,
) -> TestResult {
    match expected_terminal {
        "done" => {
            let Some(AssistantMessageEvent::Done { message, .. }) = events.last() else {
                return Err(format!("{name}: missing done message").into());
            };
            let text = message.content.iter().find_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            });
            if text.is_none_or(str::is_empty) {
                return Err(format!("{name}: done text is empty").into());
            }
            if !events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::TextDelta { .. }))
            {
                return Err(format!("{name}: expected text_delta").into());
            }
            Ok(())
        }
        "error" => {
            let Some(AssistantMessageEvent::Error { reason, error }) = events.last() else {
                return Err(format!("{name}: missing error terminal").into());
            };
            if record.get("abortAfterEventType").is_some() {
                if *reason != ErrorReason::Aborted {
                    return Err(format!("{name}: expected Aborted, got {reason:?}").into());
                }
                if error.error_message.as_deref() != Some("Request was aborted") {
                    return Err(format!(
                        "{name}: abort message mismatch: {:?}",
                        error.error_message
                    )
                    .into());
                }
            } else if *reason != ErrorReason::Error {
                return Err(format!("{name}: expected Error reason, got {reason:?}").into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn assert_http_request(
    name: &str,
    captured: &support::http::CapturedRequest,
    expected: &Value,
) -> TestResult {
    let method = expected
        .get("method")
        .and_then(Value::as_str)
        .ok_or("request.method")?;
    if captured.method.as_str() != method {
        return Err(format!("{name}: method {} != {method}", captured.method).into());
    }
    let path = expected
        .get("path")
        .and_then(Value::as_str)
        .ok_or("request.path")?;
    if captured.path != path {
        return Err(format!("{name}: path {} != {path}", captured.path).into());
    }
    match expected.get("query") {
        Some(Value::Null) | None => {
            if captured.query.is_some() {
                return Err(format!("{name}: unexpected query {:?}", captured.query).into());
            }
        }
        Some(Value::String(query)) => {
            if captured.query.as_deref() != Some(query.as_str()) {
                return Err(format!("{name}: query {:?} != {query}", captured.query).into());
            }
        }
        Some(other) => return Err(format!("{name}: invalid query {other}").into()),
    }

    let expected_headers = expected
        .get("headers")
        .and_then(Value::as_object)
        .ok_or("request.headers")?;
    let actual_headers = filter_headers(&captured.headers);
    for (key, value) in expected_headers {
        let expected_value = value
            .as_str()
            .ok_or_else(|| format!("{name}: header {key} must be string"))?;
        let actual = actual_headers
            .get(key)
            .ok_or_else(|| format!("{name}: missing header {key}"))?;
        if actual != expected_value {
            return Err(format!("{name}: header {key}: {actual} != {expected_value}").into());
        }
    }
    if actual_headers.get("chatgpt-account-id").map(String::as_str) != Some("acc_test") {
        return Err(format!("{name}: chatgpt-account-id must be JWT-derived acc_test").into());
    }

    let encoding = actual_headers.get("content-encoding").map(String::as_str);
    let body_json = if encoding == Some("zstd") {
        let decoded = zstd::stream::decode_all(Cursor::new(&captured.body))
            .map_err(|error| format!("{name}: zstd decode failed: {error}"))?;
        serde_json::from_slice::<Value>(&decoded)
            .map_err(|error| format!("{name}: body json after zstd: {error}"))?
    } else {
        serde_json::from_slice::<Value>(&captured.body)
            .map_err(|error| format!("{name}: body json: {error}"))?
    };
    let expected_body = expected.get("body").ok_or("request.body")?;
    if &body_json != expected_body {
        return Err(
            format!("{name}: body mismatch\nactual={body_json}\nexpected={expected_body}").into(),
        );
    }
    Ok(())
}

fn assert_ws_request(
    name: &str,
    capture: &WsCapture,
    expected: &Value,
    normalizations: &[String],
) -> TestResult {
    let expected_headers = expected
        .get("headers")
        .and_then(Value::as_object)
        .ok_or("request.headers")?;
    let actual_headers = filter_headers(&capture.headers);
    for (key, value) in expected_headers {
        if key == "x-client-request-id"
            && normalizations
                .iter()
                .any(|item| item == "x-client-request-id")
        {
            if !actual_headers.contains_key("x-client-request-id") {
                return Err(format!("{name}: missing x-client-request-id").into());
            }
            continue;
        }
        let expected_value = value
            .as_str()
            .ok_or_else(|| format!("{name}: header {key} must be string"))?;
        let actual = actual_headers
            .get(key)
            .ok_or_else(|| format!("{name}: missing header {key}"))?;
        if actual != expected_value {
            return Err(format!("{name}: header {key}: {actual} != {expected_value}").into());
        }
    }
    if actual_headers.get("chatgpt-account-id").map(String::as_str) != Some("acc_test") {
        return Err(format!("{name}: chatgpt-account-id must be JWT-derived acc_test").into());
    }
    if actual_headers.get("openai-beta").map(String::as_str)
        != Some("responses_websockets=2026-02-06")
    {
        return Err(format!("{name}: websocket openai-beta mismatch").into());
    }
    let expected_body = expected.get("body").ok_or("request.body")?;
    if &capture.body != expected_body {
        return Err(format!(
            "{name}: websocket body mismatch\nactual={}\nexpected={expected_body}",
            capture.body
        )
        .into());
    }
    Ok(())
}

fn filter_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if TRANSPORT_HEADERS.contains(&key.as_str()) || key == "user-agent" {
            continue;
        }
        if let Ok(text) = value.to_str() {
            out.insert(key, text.to_owned());
        }
    }
    out
}

fn events_to_normalized_json(
    events: &[AssistantMessageEvent],
) -> Result<Vec<Value>, Box<dyn std::error::Error + Send + Sync>> {
    let mut values = Vec::with_capacity(events.len());
    for event in events {
        let mut value = serde_json::to_value(event)?;
        normalize_timestamps(&mut value);
        values.push(value);
    }
    Ok(values)
}

fn event_type_name(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

fn model_from_case(
    record: &Value,
    base_url: &str,
) -> Result<Model, Box<dyn std::error::Error + Send + Sync>> {
    let mut model_value = record.get("model").cloned().ok_or("missing model")?;
    if let Some(object) = model_value.as_object_mut() {
        object.insert("baseUrl".into(), Value::String(base_url.to_owned()));
    }
    Ok(serde_json::from_value(model_value)?)
}

fn context_from_case(record: &Value) -> Result<Context, Box<dyn std::error::Error + Send + Sync>> {
    Ok(serde_json::from_value(
        record.get("context").cloned().ok_or("missing context")?,
    )?)
}

fn options_from_case(record: &Value, with_cancel: bool) -> StreamOptions {
    let options = record.get("options").cloned().unwrap_or_else(|| json!({}));
    let object = options.as_object().cloned().unwrap_or_default();
    StreamOptions {
        api_key: object
            .get("apiKey")
            .and_then(Value::as_str)
            .map(str::to_owned),
        session_id: object
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        max_retries: object
            .get("maxRetries")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        timeout_ms: object.get("timeoutMs").and_then(Value::as_u64),
        websocket_connect_timeout_ms: object
            .get("websocketConnectTimeoutMs")
            .and_then(Value::as_u64),
        transport: object
            .get("transport")
            .and_then(Value::as_str)
            .map(|value| match value {
                "sse" => Transport::Sse,
                "websocket" => Transport::Websocket,
                "websocket-cached" => Transport::WebsocketCached,
                _ => Transport::Auto,
            }),
        signal: if with_cancel {
            Some(CancellationToken::new())
        } else {
            None
        },
        ..StreamOptions::default()
    }
}

fn response_spec_from_value(
    value: &Value,
) -> Result<ResponseSpec, Box<dyn std::error::Error + Send + Sync>> {
    let status = value
        .get("status")
        .and_then(Value::as_u64)
        .ok_or("response.status")?;
    let status = StatusCode::from_u16(u16::try_from(status)?)?;
    let mut spec = ResponseSpec::new(status);
    if let Some(headers) = value.get("headers").and_then(Value::as_object) {
        for (name, header_value) in headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())?;
            let text = header_value
                .as_str()
                .ok_or_else(|| format!("header {name} must be string"))?;
            spec.headers
                .append(header_name, HeaderValue::from_str(text)?);
        }
    }
    spec.keep_open = value
        .get("keepOpen")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(chunks) = value.get("chunks").and_then(Value::as_array) {
        for chunk in chunks {
            let bytes = chunk
                .get("text")
                .and_then(Value::as_str)
                .map(|text| text.as_bytes().to_vec())
                .unwrap_or_default();
            let delay = chunk
                .get("delayMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis);
            spec.chunks.push(match delay {
                Some(delay) => ResponseChunk::delayed(bytes, delay),
                None => ResponseChunk::immediate(bytes),
            });
        }
    }
    Ok(spec)
}

fn expected_terminal(record: &Value) -> Result<&str, Box<dyn std::error::Error + Send + Sync>> {
    record
        .get("expectedTerminal")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing expectedTerminal".into())
}

fn normalizations(record: &Value) -> Vec<String> {
    record
        .get("normalizations")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn fixture_path() -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    Ok(PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join(FIXTURE_REL))
}

// --- Local WebSocket loopback server (Codex-only) ---

#[derive(Debug)]
struct WsCapture {
    headers: HeaderMap,
    body: Value,
}

struct LocalWsServer {
    address: SocketAddr,
    capture: Arc<Mutex<Option<WsCapture>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl LocalWsServer {
    async fn start(frames: Vec<Value>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let address = listener.local_addr()?;
        let capture = Arc::new(Mutex::new(None));
        let (shutdown, mut shutdown_rx) = oneshot::channel::<()>();
        let capture_task = Arc::clone(&capture);
        let task = tokio::spawn(async move {
            tokio::select! {
                accept = listener.accept() => {
                    if let Ok((stream, _)) = accept {
                        let _result = serve_ws(stream, frames, capture_task).await;
                    }
                }
                _ = &mut shutdown_rx => {}
            }
        });
        Ok(Self {
            address,
            capture,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    async fn shutdown(mut self) -> Result<WsCapture, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let mut guard = self.capture.lock().await;
        guard
            .take()
            .ok_or_else(|| "websocket server captured no request".into())
    }
}

impl Drop for LocalWsServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.take();
    }
}

async fn serve_ws(
    stream: TcpStream,
    frames: Vec<Value>,
    capture: Arc<Mutex<Option<WsCapture>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let headers = Arc::new(std::sync::Mutex::new(HeaderMap::new()));
    let headers_cb = Arc::clone(&headers);
    let callback = CaptureRequestHeaders {
        headers: headers_cb,
    };
    let mut socket = accept_hdr_async(stream, callback).await?;
    let body = match socket.next().await {
        Some(Ok(WsMessage::Text(text))) => serde_json::from_str::<Value>(&text)?,
        Some(Ok(other)) => {
            return Err(format!("expected text websocket frame, got {other:?}").into());
        }
        Some(Err(error)) => return Err(error.into()),
        None => return Err("websocket closed before request body".into()),
    };
    let request_headers = headers
        .lock()
        .map_err(|_| "websocket header lock poisoned")?
        .clone();
    {
        let mut guard = capture.lock().await;
        *guard = Some(WsCapture {
            headers: request_headers,
            body,
        });
    }
    for frame in frames {
        let text = serde_json::to_string(&frame)?;
        socket.send(WsMessage::Text(text.into())).await?;
    }
    let _ = socket.send(WsMessage::Close(None)).await;
    Ok(())
}

struct CaptureRequestHeaders {
    headers: Arc<std::sync::Mutex<HeaderMap>>,
}

impl WsCallback for CaptureRequestHeaders {
    fn on_request(
        self,
        request: &WsRequest,
        response: WsResponse,
    ) -> Result<WsResponse, WsErrorResponse> {
        if let Ok(mut guard) = self.headers.lock() {
            for (name, value) in request.headers() {
                if let (Ok(header_name), Ok(header_value)) = (
                    HeaderName::from_bytes(name.as_str().as_bytes()),
                    HeaderValue::from_bytes(value.as_bytes()),
                ) {
                    guard.append(header_name, header_value);
                }
            }
        }
        Ok(response)
    }
}
