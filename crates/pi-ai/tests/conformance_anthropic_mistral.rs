//! Provider conformance for Anthropic Messages and Mistral Conversations.
//!
//! Fixture schema (Main authority):
//! `{name, provenance, model, context, options, request, response, expectedEvents,
//!  expectedTerminal, normalizations}`.
//!
//! Live smoke is `#[ignore]` and only runs with explicit `--ignored` invocation plus
//! `ANTHROPIC_API_KEY`. Offline tests never leave loopback.

#[path = "support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use futures::{StreamExt, stream::BoxStream};
use pi_ai::{
    Provider, ProviderError, StreamOptions,
    providers::{AnthropicMessages, MistralConversations},
    types::{AssistantMessageEvent, Context, Model},
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value};
use support::{
    golden::{load_jsonl, normalize_timestamps},
    http::{CapturedRequest, LocalHttpServer, ResponseChunk, ResponseSpec},
};
use tokio_util::sync::CancellationToken;

const FIXTURE_ROOT: &str = "tests/fixtures/providers";
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_TIMEOUT: Duration = Duration::from_secs(5);

const TRANSPORT_HEADER_DENYLIST: &[&str] = &[
    "host",
    "content-length",
    "accept-encoding",
    "connection",
    "user-agent",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaseFile {
    name: String,
    provenance: Provenance,
    model: Model,
    context: Context,
    options: CaseOptions,
    request: ExpectedRequest,
    response: CaseResponse,
    expected_events: Vec<Value>,
    expected_terminal: String,
    #[serde(default)]
    normalizations: Vec<String>,
    #[serde(default)]
    abort_after_start: bool,
}

#[derive(Debug, Deserialize)]
struct Provenance {
    path: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    test: Option<String>,
    lines: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaseOptions {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cache_retention: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    extra: Map<String, Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct ExpectedRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaseResponse {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    keep_open: bool,
    chunks: Vec<CaseChunk>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaseChunk {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    base64: Option<String>,
    #[serde(default)]
    delay_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum AdapterKind {
    Anthropic,
    Mistral,
}

type ProviderStream = BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>;

#[tokio::test]
async fn anthropic_messages_conformance() -> Result<(), String> {
    run_fixture_dir("anthropic-messages", AdapterKind::Anthropic).await
}

#[tokio::test]
async fn mistral_conversations_conformance() -> Result<(), String> {
    run_fixture_dir("mistral-conversations", AdapterKind::Mistral).await
}

/// Credential-gated live smoke for Anthropic Messages.
///
/// Discoverable offline:
/// `ANTHROPIC_API_KEY=... cargo test -p pi-ai --test conformance_anthropic_mistral live_anthropic_messages_smoke -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live Anthropic network smoke; requires ANTHROPIC_API_KEY and --ignored"]
async fn live_anthropic_messages_smoke() -> Result<(), String> {
    let api_key = require_live_api_key()?;
    let model = live_anthropic_model();
    let context = live_smoke_context();
    let provider = AnthropicMessages::new(Client::new());
    let stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some(api_key),
            max_tokens: Some(16),
            ..StreamOptions::default()
        },
    );
    assert_live_smoke_stream(stream).await
}

async fn run_fixture_dir(dir_name: &str, kind: AdapterKind) -> Result<(), String> {
    let path = fixture_path(dir_name)?;
    let records = load_jsonl(&path).map_err(|error| error.to_string())?;
    if records.is_empty() {
        return Err(format!("no cases in {}", path.display()));
    }
    for (index, record) in records.into_iter().enumerate() {
        let case = parse_case(&path, index, record)?;
        run_case(kind, &case)
            .await
            .map_err(|error| format!("{} case `{}`: {error}", dir_name, case.name))?;
    }
    Ok(())
}

fn parse_case(path: &Path, index: usize, record: Value) -> Result<CaseFile, String> {
    serde_json::from_value(record).map_err(|error| {
        format!(
            "{} line {}: invalid case JSON: {error}",
            path.display(),
            index + 1
        )
    })
}

async fn run_case(kind: AdapterKind, case: &CaseFile) -> Result<(), String> {
    validate_provenance(case)?;
    let server = start_case_server(&case.response).await?;
    let events = stream_case_events(kind, case, &server.base_url()).await?;
    let captured = take_single_request(server).await?;
    assert_request(case, &captured)?;
    assert_events_match(case, events)?;
    Ok(())
}

async fn start_case_server(response: &CaseResponse) -> Result<LocalHttpServer, String> {
    let spec = response_spec_from_case(response)?;
    LocalHttpServer::start([spec])
        .await
        .map_err(|error| error.to_string())
}

async fn stream_case_events(
    kind: AdapterKind,
    case: &CaseFile,
    base_url: &str,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let mut model = case.model.clone();
    model.base_url = base_url.to_owned();
    let options = stream_options_from_case(&case.options, case.abort_after_start)?;
    let cancel = options.signal.clone();
    let stream = open_provider_stream(kind, &model, case.context.clone(), options);
    collect_stream_events(
        stream,
        cancel,
        case.abort_after_start,
        &case.expected_terminal,
    )
    .await
}

fn open_provider_stream(
    kind: AdapterKind,
    model: &Model,
    context: Context,
    options: StreamOptions,
) -> ProviderStream {
    let client = Client::new();
    match kind {
        AdapterKind::Anthropic => AnthropicMessages::new(client)
            .stream(model, context, options)
            .boxed(),
        AdapterKind::Mistral => MistralConversations::new(client)
            .stream(model, context, options)
            .boxed(),
    }
}

async fn collect_stream_events(
    mut stream: ProviderStream,
    cancel: Option<CancellationToken>,
    abort_after_start: bool,
    expected_terminal: &str,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let collect = async {
        let mut events = Vec::new();
        let mut saw_start = false;
        let mut saw_terminal = false;
        while let Some(item) = stream.next().await {
            let event = item.map_err(|error| error.to_string())?;
            if matches!(event, AssistantMessageEvent::Start { .. }) {
                saw_start = true;
                if abort_after_start && let Some(token) = cancel.as_ref() {
                    token.cancel();
                }
            }
            if is_terminal_event(&event) {
                if saw_terminal {
                    return Err("stream emitted more than one done|error".to_owned());
                }
                saw_terminal = true;
                events.push(event);
                continue;
            }
            if saw_terminal {
                return Err("stream emitted events after terminal done|error".to_owned());
            }
            events.push(event);
        }
        validate_collected_events(&events, saw_start, expected_terminal)?;
        Ok(events)
    };

    match tokio::time::timeout(STREAM_TIMEOUT, collect).await {
        Ok(result) => result,
        Err(_) => Err(format!("stream timed out after {STREAM_TIMEOUT:?}")),
    }
}

fn is_terminal_event(event: &AssistantMessageEvent) -> bool {
    matches!(
        event,
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
    )
}

fn validate_collected_events(
    events: &[AssistantMessageEvent],
    saw_start: bool,
    expected_terminal: &str,
) -> Result<(), String> {
    if !saw_start {
        return Err("stream missing required start event".to_owned());
    }
    let Some(last) = events.last() else {
        return Err("stream produced no events".to_owned());
    };
    let terminal = match last {
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
        _ => return Err("stream did not end with exactly one done|error".to_owned()),
    };
    if terminal != expected_terminal {
        return Err(format!(
            "expected terminal `{expected_terminal}`, got `{terminal}`"
        ));
    }
    let terminal_count = events
        .iter()
        .filter(|event| is_terminal_event(event))
        .count();
    if terminal_count != 1 {
        return Err(format!(
            "expected exactly one done|error, found {terminal_count}"
        ));
    }
    Ok(())
}

async fn take_single_request(server: LocalHttpServer) -> Result<CapturedRequest, String> {
    server
        .wait_for_requests(1, CAPTURE_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    let mut requests = server.shutdown().await.map_err(|error| error.to_string())?;
    if requests.len() != 1 {
        return Err(format!(
            "expected exactly one captured request, got {}",
            requests.len()
        ));
    }
    Ok(requests.remove(0))
}

fn assert_events_match(case: &CaseFile, events: Vec<AssistantMessageEvent>) -> Result<(), String> {
    let mut actual_events = events
        .into_iter()
        .map(|event| {
            let value = serde_json::to_value(event).map_err(|error| error.to_string())?;
            canonicalize_json(&value)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected_events = case
        .expected_events
        .iter()
        .map(canonicalize_json)
        .collect::<Result<Vec<_>, _>>()?;
    for event in &mut actual_events {
        normalize_event(event, &case.normalizations);
    }
    for event in &mut expected_events {
        normalize_event(event, &case.normalizations);
    }
    if actual_events != expected_events {
        return Err(format!(
            "event JSON mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&expected_events),
            pretty(&actual_events)
        ));
    }
    Ok(())
}

fn validate_provenance(case: &CaseFile) -> Result<(), String> {
    if case.provenance.path.trim().is_empty() || case.provenance.lines.trim().is_empty() {
        return Err("provenance.path and provenance.lines are required".into());
    }
    if case.provenance.symbol.is_none() && case.provenance.test.is_none() {
        return Err("provenance requires symbol or test".into());
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join(&case.provenance.path);
    if !path.is_file() {
        return Err(format!(
            "provenance path missing on disk: {}",
            case.provenance.path
        ));
    }
    Ok(())
}

fn response_spec_from_case(response: &CaseResponse) -> Result<ResponseSpec, String> {
    let status = StatusCode::from_u16(response.status)
        .map_err(|error| format!("invalid response status: {error}"))?;
    let headers = response_headers(&response.headers)?;
    let chunks = response_chunks(&response.chunks)?;
    Ok(ResponseSpec {
        status,
        headers,
        chunks,
        keep_open: response.keep_open,
    })
}

fn response_headers(headers: &BTreeMap<String, String>) -> Result<HeaderMap, String> {
    let mut mapped = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid response header name {name:?}: {error}"))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid response header value for {name}: {error}"))?;
        mapped.append(header_name, header_value);
    }
    Ok(mapped)
}

fn response_chunks(chunks: &[CaseChunk]) -> Result<Vec<ResponseChunk>, String> {
    let mut mapped = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let bytes = chunk_bytes(chunk)?;
        let mapped_chunk = match chunk.delay_ms {
            Some(delay_ms) => ResponseChunk::delayed(bytes, Duration::from_millis(delay_ms)),
            None => ResponseChunk::immediate(bytes),
        };
        mapped.push(mapped_chunk);
    }
    Ok(mapped)
}

fn chunk_bytes(chunk: &CaseChunk) -> Result<Vec<u8>, String> {
    if let Some(text) = &chunk.text {
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(encoded) = &chunk.base64 {
        use base64::Engine as _;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("invalid base64 chunk: {error}"));
    }
    Err("response chunk needs text or base64".into())
}

fn stream_options_from_case(
    options: &CaseOptions,
    abort_after_start: bool,
) -> Result<StreamOptions, String> {
    Ok(StreamOptions {
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        signal: abort_after_start.then(CancellationToken::new),
        api_key: options.api_key.clone(),
        transport: None,
        cache_retention: parse_cache_retention(options.cache_retention.as_deref())?,
        session_id: options.session_id.clone(),
        on_payload: None,
        on_response: None,
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        websocket_connect_timeout_ms: None,
        max_retries: None,
        max_retry_delay_ms: None,
        metadata: options.metadata.clone(),
        env: options.env.clone(),
        extra: options.extra.clone(),
    })
}

fn parse_cache_retention(
    value: Option<&str>,
) -> Result<Option<pi_ai::types::CacheRetention>, String> {
    match value {
        None => Ok(None),
        Some("none") => Ok(Some(pi_ai::types::CacheRetention::None)),
        Some("short") => Ok(Some(pi_ai::types::CacheRetention::Short)),
        Some("long") => Ok(Some(pi_ai::types::CacheRetention::Long)),
        Some(other) => Err(format!("unknown cacheRetention {other:?}")),
    }
}

fn assert_request(case: &CaseFile, captured: &CapturedRequest) -> Result<(), String> {
    assert_request_target(case, captured)?;
    assert_request_headers(case, captured)?;
    assert_request_body(case, captured)?;
    Ok(())
}

fn assert_request_target(case: &CaseFile, captured: &CapturedRequest) -> Result<(), String> {
    if captured.method.as_str() != case.request.method {
        return Err(format!(
            "method mismatch: expected {}, got {}",
            case.request.method, captured.method
        ));
    }
    if captured.path != case.request.path {
        return Err(format!(
            "path mismatch: expected {}, got {}",
            case.request.path, captured.path
        ));
    }
    if captured.query != case.request.query {
        return Err(format!(
            "query mismatch: expected {:?}, got {:?}",
            case.request.query, captured.query
        ));
    }
    Ok(())
}

fn assert_request_headers(case: &CaseFile, captured: &CapturedRequest) -> Result<(), String> {
    let actual_headers = filter_headers(&captured.headers);
    let expected_headers = normalize_header_map(&case.request.headers);
    if actual_headers != expected_headers {
        return Err(format!(
            "headers mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&expected_headers),
            pretty(&actual_headers)
        ));
    }
    Ok(())
}

fn assert_request_body(case: &CaseFile, captured: &CapturedRequest) -> Result<(), String> {
    let actual_body: Value = if captured.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&captured.body)
            .map_err(|error| format!("captured body is not JSON: {error}"))?
    };
    if actual_body != case.request.body {
        return Err(format!(
            "body mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&case.request.body),
            pretty(&actual_body)
        ));
    }
    Ok(())
}

fn filter_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut filtered = BTreeMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if TRANSPORT_HEADER_DENYLIST.contains(&key.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        filtered.insert(key, value.to_owned());
    }
    filtered
}

fn normalize_header_map(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

fn normalize_event(event: &mut Value, _normalizations: &[String]) {
    // Only exact `timestamp` fields are nondeterministic across runs.
    normalize_timestamps(event);
}

fn canonicalize_json(value: &Value) -> Result<Value, String> {
    // Round-trip through text so fixture-loaded and freshly-serialized floats share
    // one JSON number representation before equality comparison.
    let text = serde_json::to_string(value).map_err(|error| error.to_string())?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

fn fixture_path(dir_name: &str) -> Result<PathBuf, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(dir_name)
        .join("cases.jsonl");
    if !path.is_file() {
        return Err(format!("missing fixture file {}", path.display()));
    }
    Ok(path)
}

fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "<serialize error>".into())
}

fn require_live_api_key() -> Result<String, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
        "ANTHROPIC_API_KEY is required for live_anthropic_messages_smoke".to_owned()
    })?;
    if api_key.trim().is_empty() {
        return Err("ANTHROPIC_API_KEY is empty".into());
    }
    Ok(api_key)
}

fn live_anthropic_model() -> Model {
    Model {
        id: "claude-haiku-4-5".into(),
        name: "Claude Haiku 4.5".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![pi_ai::types::ModelInput::Text],
        cost: pi_ai::types::ModelCost::default(),
        context_window: 200_000,
        max_tokens: 64,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    }
}

fn live_smoke_context() -> Context {
    Context {
        system_prompt: None,
        messages: vec![pi_ai::types::Message::User(pi_ai::types::UserMessage::new(
            pi_ai::types::UserMessageContent::Text("Reply with the single word pong.".into()),
            1,
        ))],
        tools: None,
    }
}

async fn assert_live_smoke_stream(mut stream: ProviderStream) -> Result<(), String> {
    let mut saw_start = false;
    let mut terminal = None;
    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| error.to_string())?;
        match event {
            AssistantMessageEvent::Start { .. } => saw_start = true,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                terminal = Some(event);
                break;
            }
            _ => {}
        }
    }
    if !saw_start {
        return Err("live smoke missing start event".into());
    }
    match terminal {
        Some(AssistantMessageEvent::Done { .. }) => Ok(()),
        Some(AssistantMessageEvent::Error { error, .. }) => Err(format!(
            "live smoke terminal error: {}",
            error.error_message.unwrap_or_default()
        )),
        _ => Err("live smoke missing terminal done/error".into()),
    }
}
