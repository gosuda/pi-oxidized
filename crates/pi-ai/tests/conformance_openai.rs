//! Provider conformance for `OpenAI` Completions, `OpenAI` Responses, and Azure Responses.
//!
//! Fixture schema (Main authority):
//! `{name, provenance, model, context, options, request, response, expectedEvents,
//!  expectedTerminal, normalizations}`.
//!
//! Live smoke is `#[ignore]` and only runs with explicit `--ignored` invocation plus
//! `OPENAI_API_KEY`. Offline tests never leave loopback.

#[path = "support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use futures::StreamExt;
use futures::stream::BoxStream;
use pi_ai::{
    Provider, ProviderError, StreamOptions,
    providers::{AzureOpenAiResponses, OpenAiCompletions, OpenAiResponses},
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
    delay_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum AdapterKind {
    Completions,
    Responses,
    AzureResponses,
}

#[tokio::test]
async fn openai_completions_conformance() -> Result<(), String> {
    run_fixture_dir("openai-completions", AdapterKind::Completions).await
}

#[tokio::test]
async fn openai_responses_conformance() -> Result<(), String> {
    run_fixture_dir("openai-responses", AdapterKind::Responses).await
}

#[tokio::test]
async fn azure_openai_responses_conformance() -> Result<(), String> {
    run_fixture_dir("azure-openai-responses", AdapterKind::AzureResponses).await
}

/// Credential-gated live smoke for `OpenAI` Completions.
///
/// Discoverable offline:
/// `OPENAI_API_KEY=... cargo test -p pi-ai --test conformance_openai live_openai_completions_smoke -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live OpenAI network smoke; requires OPENAI_API_KEY and --ignored"]
async fn live_openai_completions_smoke() -> Result<(), String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| "OPENAI_API_KEY is required for live_openai_completions_smoke".to_owned())?;
    if api_key.trim().is_empty() {
        return Err("OPENAI_API_KEY is empty".into());
    }

    let model = Model {
        id: "gpt-4o-mini".into(),
        name: "GPT-4o mini".into(),
        api: "openai-completions".into(),
        provider: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![pi_ai::types::ModelInput::Text],
        cost: pi_ai::types::ModelCost::default(),
        context_window: 128_000,
        max_tokens: 64,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    };
    let context = Context {
        system_prompt: None,
        messages: vec![pi_ai::types::Message::User(pi_ai::types::UserMessage::new(
            pi_ai::types::UserMessageContent::Text("Reply with the single word pong.".into()),
            1,
        ))],
        tools: None,
    };
    let provider = OpenAiCompletions::new(Client::new());
    let mut stream = provider.stream(
        &model,
        context,
        StreamOptions {
            api_key: Some(api_key),
            max_tokens: Some(16),
            ..StreamOptions::default()
        },
    );

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

async fn run_fixture_dir(dir_name: &str, kind: AdapterKind) -> Result<(), String> {
    let path = fixture_path(dir_name)?;
    let records = load_jsonl(&path).map_err(|error| error.to_string())?;
    if records.is_empty() {
        return Err(format!("no cases in {}", path.display()));
    }
    for (index, record) in records.into_iter().enumerate() {
        let case: CaseFile = serde_json::from_value(record).map_err(|error| {
            format!(
                "{} line {}: invalid case JSON: {error}",
                path.display(),
                index + 1
            )
        })?;
        run_case(kind, &case)
            .await
            .map_err(|error| format!("{} case `{}`: {error}", dir_name, case.name))?;
    }
    Ok(())
}

async fn run_case(kind: AdapterKind, case: &CaseFile) -> Result<(), String> {
    validate_provenance(case)?;
    let server = LocalHttpServer::start([response_spec_from_case(&case.response)?])
        .await
        .map_err(|error| error.to_string())?;

    let mut model = case.model.clone();
    // Loopback only: force every request onto the ephemeral local server.
    model.base_url = server.base_url();

    let options = stream_options_from_case(&case.options, case.abort_after_start)?;
    let cancel = options.signal.clone();
    let stream = open_provider_stream(kind, &model, case.context.clone(), options);
    let events = drive_stream(case, stream, &server, cancel).await?;
    let requests = finalize_owned_server(server, case.abort_after_start).await?;
    assert_request(case, &requests[0])?;
    assert_events(case, events)?;
    Ok(())
}

fn open_provider_stream(
    kind: AdapterKind,
    model: &Model,
    context: Context,
    options: StreamOptions,
) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
    let client = Client::new();
    match kind {
        AdapterKind::Completions => OpenAiCompletions::new(client)
            .stream(model, context, options)
            .boxed(),
        AdapterKind::Responses => OpenAiResponses::new(client)
            .stream(model, context, options)
            .boxed(),
        AdapterKind::AzureResponses => AzureOpenAiResponses::new(client)
            .stream(model, context, options)
            .boxed(),
    }
}

async fn drive_stream(
    case: &CaseFile,
    mut stream: BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>,
    server: &LocalHttpServer,
    cancel: Option<CancellationToken>,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let collect = collect_events(case, &mut stream);
    let abort_task = maybe_abort_after_capture(case, server, cancel);

    let (events_result, abort_result) =
        tokio::join!(tokio::time::timeout(STREAM_TIMEOUT, collect), abort_task);
    abort_result?;
    match events_result {
        Ok(result) => result,
        Err(_) => Err(format!("stream timed out after {STREAM_TIMEOUT:?}")),
    }
}

async fn collect_events(
    case: &CaseFile,
    stream: &mut BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let mut events = Vec::new();
    let mut saw_start = false;
    while let Some(item) = stream.next().await {
        let event = item.map_err(|error| error.to_string())?;
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            saw_start = true;
        }
        let is_terminal = matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        );
        events.push(event);
        if is_terminal {
            break;
        }
    }
    validate_terminal_shape(case, saw_start, &events)?;
    Ok(events)
}

async fn maybe_abort_after_capture(
    case: &CaseFile,
    server: &LocalHttpServer,
    cancel: Option<CancellationToken>,
) -> Result<(), String> {
    // Adapters emit `start` before the HTTP request. For abort cases, wait until the
    // loopback capture exists, then cancel while the body is still open.
    if !case.abort_after_start {
        return Ok(());
    }
    server
        .wait_for_requests(1, CAPTURE_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Some(token) = cancel.as_ref() {
        token.cancel();
    }
    Ok(())
}

fn validate_terminal_shape(
    case: &CaseFile,
    saw_start: bool,
    events: &[AssistantMessageEvent],
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
    if terminal != case.expected_terminal {
        return Err(format!(
            "expected terminal `{}`, got `{terminal}`",
            case.expected_terminal
        ));
    }
    let terminal_count = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )
        })
        .count();
    if terminal_count != 1 {
        return Err(format!(
            "expected exactly one done|error, found {terminal_count}"
        ));
    }
    Ok(())
}

async fn finalize_owned_server(
    server: LocalHttpServer,
    abort_after_start: bool,
) -> Result<Vec<CapturedRequest>, String> {
    if !abort_after_start {
        server
            .wait_for_requests(1, CAPTURE_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
    }
    let requests = server.shutdown().await.map_err(|error| error.to_string())?;
    if requests.len() != 1 {
        return Err(format!(
            "expected exactly one captured request, got {}",
            requests.len()
        ));
    }
    Ok(requests)
}

fn assert_events(case: &CaseFile, events: Vec<AssistantMessageEvent>) -> Result<(), String> {
    let mut actual_events = events
        .into_iter()
        .map(|event| serde_json::to_value(event).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected_events = case.expected_events.clone();
    for event in &mut actual_events {
        normalize_event(event, &case.normalizations);
    }
    for event in &mut expected_events {
        normalize_event(event, &case.normalizations);
    }
    if actual_events != expected_events {
        return Err(format!(
            "event JSON mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&expected_events)?,
            pretty(&actual_events)?
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
    let mut headers = HeaderMap::new();
    for (name, value) in &response.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid response header name {name:?}: {error}"))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid response header value for {name}: {error}"))?;
        headers.append(header_name, header_value);
    }
    let mut chunks = Vec::with_capacity(response.chunks.len());
    for chunk in &response.chunks {
        let Some(text) = &chunk.text else {
            return Err("response chunk needs text".into());
        };
        let bytes = text.as_bytes().to_vec();
        let chunk = match chunk.delay_ms {
            Some(delay_ms) => ResponseChunk::delayed(bytes, Duration::from_millis(delay_ms)),
            None => ResponseChunk::immediate(bytes),
        };
        chunks.push(chunk);
    }
    Ok(ResponseSpec {
        status,
        headers,
        chunks,
        keep_open: response.keep_open,
    })
}

fn stream_options_from_case(
    options: &CaseOptions,
    abort_after_start: bool,
) -> Result<StreamOptions, String> {
    let cache_retention = match options.cache_retention.as_deref() {
        None => None,
        Some("none") => Some(pi_ai::types::CacheRetention::None),
        Some("short") => Some(pi_ai::types::CacheRetention::Short),
        Some("long") => Some(pi_ai::types::CacheRetention::Long),
        Some(other) => return Err(format!("unknown cacheRetention {other:?}")),
    };
    Ok(StreamOptions {
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        signal: abort_after_start.then(CancellationToken::new),
        api_key: options.api_key.clone(),
        transport: None,
        cache_retention,
        session_id: options.session_id.clone(),
        on_payload: None,
        on_response: None,
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        websocket_connect_timeout_ms: None,
        max_retries: None,
        max_retry_delay_ms: None,
        metadata: None,
        env: options.env.clone(),
        extra: options.extra.clone(),
    })
}

fn assert_request(case: &CaseFile, captured: &CapturedRequest) -> Result<(), String> {
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

    let actual_headers = filter_headers(&captured.headers);
    let expected_headers = normalize_header_map(&case.request.headers);
    if actual_headers != expected_headers {
        return Err(format!(
            "headers mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&expected_headers)?,
            pretty(&actual_headers)?
        ));
    }

    let actual_body: Value = if captured.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&captured.body)
            .map_err(|error| format!("captured body is not JSON: {error}"))?
    };
    if actual_body != case.request.body {
        return Err(format!(
            "body mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&case.request.body)?,
            pretty(&actual_body)?
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

fn normalize_event(event: &mut Value, normalizations: &[String]) {
    // Always strip exact `timestamp` fields; fixtures list this in normalizations.
    normalize_timestamps(event);
    let _ = normalizations;
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

fn pretty(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}
