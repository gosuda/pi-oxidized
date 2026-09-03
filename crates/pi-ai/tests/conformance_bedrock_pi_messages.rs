//! Provider conformance for Amazon Bedrock `ConverseStream` and pi-messages.
//!
//! Fixture schema (Main authority):
//! `{name, provenance, model, context, options, request, response, expectedEvents,
//!  expectedTerminal, normalizations}`.
//!
//! Bedrock uses the [`BedrockClientFactory`] seam: each case is served by a
//! loopback HTTP event-stream body while the factory records region/endpoint/
//! model/header ownership. pi-messages uses ordinary HTTP SSE against the same
//! loopback harness.
//!
//! Live smoke is `#[ignore]` and only runs with explicit `--ignored` invocation
//! plus AWS credentials. Offline tests never leave loopback.

#[path = "support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use aws_sdk_bedrockruntime::{
    Client as BedrockClient,
    config::{BehaviorVersion, Credentials, Region},
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use base64::Engine as _;
use futures::{StreamExt, future::BoxFuture, stream::BoxStream};
use pi_ai::{
    Provider, ProviderError, StreamOptions,
    providers::{BedrockClientFactory, BedrockClientRequest, BedrockConverseStream, PiMessages},
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
const STREAM_TIMEOUT: Duration = Duration::from_secs(8);

const TRANSPORT_HEADER_DENYLIST: &[&str] = &[
    "host",
    "content-length",
    "accept-encoding",
    "connection",
    "user-agent",
];

/// AWS `SigV4` / SDK headers that are nondeterministic across runs.
const AWS_DYNAMIC_HEADER_PREFIXES: &[&str] = &["x-amz-", "amz-sdk-"];
const AWS_DYNAMIC_HEADERS: &[&str] = &["authorization"];

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
    /// Optional Bedrock factory ownership expectations.
    #[serde(default)]
    client_request: Option<ExpectedClientRequest>,
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
struct ExpectedClientRequest {
    region: String,
    #[serde(default)]
    endpoint_url: Option<String>,
    model_id: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
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
    Bedrock,
    PiMessages,
}

type EventStream = BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>;

#[tokio::test]
async fn bedrock_converse_stream_conformance() -> Result<(), String> {
    run_fixture_dir("bedrock-converse-stream", AdapterKind::Bedrock).await
}

#[tokio::test]
async fn pi_messages_conformance() -> Result<(), String> {
    run_fixture_dir("pi-messages", AdapterKind::PiMessages).await
}

/// Credential-gated live smoke for Amazon Bedrock `ConverseStream`.
///
/// Discoverable offline:
/// `AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_REGION=... cargo test -p pi-ai --test conformance_bedrock_pi_messages live_bedrock_converse_stream_smoke -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live Bedrock network smoke; requires AWS credentials and --ignored"]
async fn live_bedrock_converse_stream_smoke() -> Result<(), String> {
    let credentials = live_smoke_credentials()?;
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".into());
    let model_id =
        std::env::var("PI_BEDROCK_SMOKE_MODEL").unwrap_or_else(|_| "amazon.nova-micro-v1:0".into());
    let model = live_smoke_model(&model_id, &region);
    let context = live_smoke_context();
    let provider = BedrockConverseStream::new(Arc::new(LiveFactory {
        access_key: credentials.access_key,
        secret_key: credentials.secret_key,
        session_token: credentials.session_token,
    }));
    let mut stream = provider.stream(
        &model,
        context,
        StreamOptions {
            max_tokens: Some(16),
            env: Some(BTreeMap::from([("AWS_REGION".into(), region)])),
            ..StreamOptions::default()
        },
    );
    assert_live_smoke_terminal(&mut stream).await
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
    let server = start_case_server(&case.response).await?;
    let base_url = server.base_url();
    let model = model_for_case(kind, &case.model, &base_url);
    let factory_capture = Arc::new(Mutex::new(None));
    let options = stream_options_from_case(&case.options, case.abort_after_start)?;
    let cancel = options.signal.clone();
    let mut stream = provider_stream(kind, &model, case, options, &base_url, &factory_capture);
    let events = collect_case_events(&mut stream, case, cancel).await?;
    finish_case_captures(kind, case, server, &factory_capture).await?;
    assert_events_match(case, events)
}

async fn start_case_server(response: &CaseResponse) -> Result<LocalHttpServer, String> {
    let response_spec = response_spec_from_case(response)?;
    LocalHttpServer::start([response_spec])
        .await
        .map_err(|error| error.to_string())
}

fn model_for_case(kind: AdapterKind, model: &Model, base_url: &str) -> Model {
    let mut model = model.clone();
    if matches!(kind, AdapterKind::PiMessages) {
        // Loopback only: force every request onto the ephemeral local server.
        base_url.clone_into(&mut model.base_url);
    }
    model
}

fn provider_stream(
    kind: AdapterKind,
    model: &Model,
    case: &CaseFile,
    options: StreamOptions,
    base_url: &str,
    factory_capture: &Arc<Mutex<Option<BedrockClientRequest>>>,
) -> EventStream {
    match kind {
        AdapterKind::PiMessages => PiMessages::new(Client::new())
            .stream(model, case.context.clone(), options)
            .boxed(),
        AdapterKind::Bedrock => {
            let factory = Arc::new(LoopbackBedrockFactory {
                loopback_base: base_url.to_owned(),
                capture: Arc::clone(factory_capture),
            });
            BedrockConverseStream::new(factory)
                .stream(model, case.context.clone(), options)
                .boxed()
        }
    }
}

async fn collect_case_events(
    stream: &mut EventStream,
    case: &CaseFile,
    cancel: Option<CancellationToken>,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let collect = collect_stream_events(stream, case.abort_after_start, cancel.as_ref());
    let events = match tokio::time::timeout(STREAM_TIMEOUT, collect).await {
        Ok(result) => result?,
        Err(_) => return Err(format!("stream timed out after {STREAM_TIMEOUT:?}")),
    };
    validate_stream_terminal(&events, &case.expected_terminal)?;
    Ok(events)
}

async fn collect_stream_events(
    stream: &mut EventStream,
    abort_after_start: bool,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<AssistantMessageEvent>, String> {
    let mut events = Vec::new();
    let mut saw_start = false;
    while let Some(item) = stream.next().await {
        let event = item.map_err(|error| error.to_string())?;
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            saw_start = true;
            if abort_after_start && let Some(token) = cancel {
                token.cancel();
            }
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
    if !saw_start {
        return Err("stream missing required start event".to_owned());
    }
    Ok(events)
}

fn validate_stream_terminal(
    events: &[AssistantMessageEvent],
    expected_terminal: &str,
) -> Result<(), String> {
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

async fn finish_case_captures(
    kind: AdapterKind,
    case: &CaseFile,
    server: LocalHttpServer,
    factory_capture: &Arc<Mutex<Option<BedrockClientRequest>>>,
) -> Result<(), String> {
    // Abort cases may cancel before the HTTP request is fully captured.
    let expect_capture = !case.abort_after_start;
    if expect_capture {
        server
            .wait_for_requests(1, CAPTURE_TIMEOUT)
            .await
            .map_err(|error| error.to_string())?;
    }
    let requests = server.shutdown().await.map_err(|error| error.to_string())?;
    if expect_capture {
        if requests.len() != 1 {
            return Err(format!(
                "expected exactly one captured request, got {}",
                requests.len()
            ));
        }
        assert_request(kind, case, &requests[0])?;
    }
    if let AdapterKind::Bedrock = kind {
        assert_bedrock_client_request(case, factory_capture)?;
    }
    Ok(())
}

fn assert_bedrock_client_request(
    case: &CaseFile,
    factory_capture: &Arc<Mutex<Option<BedrockClientRequest>>>,
) -> Result<(), String> {
    let captured = factory_capture
        .lock()
        .map_err(|_| "factory capture mutex poisoned".to_owned())?
        .clone();
    let Some(expected) = &case.client_request else {
        return Ok(());
    };
    let Some(actual) = captured else {
        return Err("Bedrock factory was not invoked".into());
    };
    assert_client_request(expected, &actual)
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

struct LiveCredentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

fn live_smoke_credentials() -> Result<LiveCredentials, String> {
    let access_key = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
        "AWS_ACCESS_KEY_ID is required for live_bedrock_converse_stream_smoke".to_owned()
    })?;
    let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
        "AWS_SECRET_ACCESS_KEY is required for live_bedrock_converse_stream_smoke".to_owned()
    })?;
    if access_key.trim().is_empty() || secret_key.trim().is_empty() {
        return Err("AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY must be non-empty".into());
    }
    Ok(LiveCredentials {
        access_key,
        secret_key,
        session_token: std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty()),
    })
}

fn live_smoke_model(model_id: &str, region: &str) -> Model {
    Model {
        id: model_id.to_owned(),
        name: "Bedrock smoke".into(),
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        base_url: format!("https://bedrock-runtime.{region}.amazonaws.com"),
        reasoning: false,
        thinking_level_map: None,
        input: vec![pi_ai::types::ModelInput::Text],
        cost: pi_ai::types::ModelCost::default(),
        context_window: 128_000,
        max_tokens: 32,
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

async fn assert_live_smoke_terminal(stream: &mut EventStream) -> Result<(), String> {
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

struct LiveFactory {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

impl BedrockClientFactory for LiveFactory {
    fn create_client(
        &self,
        request: BedrockClientRequest,
    ) -> BoxFuture<'static, Result<BedrockClient, ProviderError>> {
        let access_key = self.access_key.clone();
        let secret_key = self.secret_key.clone();
        let session_token = self.session_token.clone();
        Box::pin(async move {
            let mut builder = aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(request.region));
            if let Some(endpoint) = request.endpoint_url {
                builder = builder.endpoint_url(endpoint);
            }
            let conf = builder
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    session_token,
                    None,
                    "live-smoke",
                ))
                .build();
            Ok(BedrockClient::from_conf(conf))
        })
    }
}

struct LoopbackBedrockFactory {
    loopback_base: String,
    capture: Arc<Mutex<Option<BedrockClientRequest>>>,
}

impl BedrockClientFactory for LoopbackBedrockFactory {
    fn create_client(
        &self,
        request: BedrockClientRequest,
    ) -> BoxFuture<'static, Result<BedrockClient, ProviderError>> {
        let loopback = self.loopback_base.clone();
        let capture = Arc::clone(&self.capture);
        Box::pin(async move {
            if let Ok(mut guard) = capture.lock() {
                *guard = Some(request.clone());
            }
            let conf = aws_sdk_bedrockruntime::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(request.region))
                .endpoint_url(loopback)
                .retry_config(aws_sdk_bedrockruntime::config::retry::RetryConfig::disabled())
                .credentials_provider(Credentials::new(
                    "AKIACONFORMANCE",
                    "conformance-secret",
                    None,
                    None,
                    "conformance-test",
                ))
                .build();
            Ok(BedrockClient::from_conf(conf))
        })
    }
}

fn assert_client_request(
    expected: &ExpectedClientRequest,
    actual: &BedrockClientRequest,
) -> Result<(), String> {
    if actual.region != expected.region {
        return Err(format!(
            "client region mismatch: expected {}, got {}",
            expected.region, actual.region
        ));
    }
    if actual.endpoint_url != expected.endpoint_url {
        return Err(format!(
            "client endpoint_url mismatch: expected {:?}, got {:?}",
            expected.endpoint_url, actual.endpoint_url
        ));
    }
    if actual.model_id != expected.model_id {
        return Err(format!(
            "client model_id mismatch: expected {}, got {}",
            expected.model_id, actual.model_id
        ));
    }
    if actual.headers != expected.headers {
        return Err(format!(
            "client headers mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(&expected.headers),
            pretty(&actual.headers)
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
    let cache_retention = parse_cache_retention(options.cache_retention.as_deref())?;
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

fn assert_request(
    kind: AdapterKind,
    case: &CaseFile,
    captured: &CapturedRequest,
) -> Result<(), String> {
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
    assert_request_headers(kind, &case.request.headers, &captured.headers)?;
    assert_request_body(&case.request.body, &captured.body)
}

fn assert_request_headers(
    kind: AdapterKind,
    expected: &BTreeMap<String, String>,
    actual: &HeaderMap,
) -> Result<(), String> {
    // Fixture `request.headers` lists the stable HTTP surface. Adapter/options
    // headers are owned by `clientRequest.headers` for Bedrock and may also
    // appear on the wire, so expected must be a subset of the filtered actual.
    let actual_headers = filter_headers(kind, actual);
    let expected_headers = normalize_header_map(expected);
    for (key, expected_value) in &expected_headers {
        let Some(actual_value) = actual_headers.get(key) else {
            return Err(format!("missing header {key}"));
        };
        if actual_value != expected_value {
            return Err(format!(
                "header {key} mismatch: expected {expected_value}, got {actual_value}"
            ));
        }
    }
    Ok(())
}

fn assert_request_body(expected: &Value, body: &[u8]) -> Result<(), String> {
    let actual_body: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body)
            .map_err(|error| format!("captured body is not JSON: {error}"))?
    };
    if actual_body != *expected {
        return Err(format!(
            "body mismatch\nexpected:\n{}\nactual:\n{}",
            pretty(expected),
            pretty(&actual_body)
        ));
    }
    Ok(())
}

fn filter_headers(kind: AdapterKind, headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut filtered = BTreeMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if should_skip_header(kind, &key) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        filtered.insert(key, value.to_owned());
    }
    filtered
}

fn should_skip_header(kind: AdapterKind, key: &str) -> bool {
    if TRANSPORT_HEADER_DENYLIST.contains(&key) {
        return true;
    }
    matches!(kind, AdapterKind::Bedrock)
        && (AWS_DYNAMIC_HEADERS.contains(&key)
            || AWS_DYNAMIC_HEADER_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix)))
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
    if needs_diagnostic_normalization(normalizations) {
        normalize_diagnostic_dynamics(event);
    }
}

fn needs_diagnostic_normalization(normalizations: &[String]) -> bool {
    normalizations.is_empty()
        || normalizations.iter().any(|item| {
            matches!(
                item.as_str(),
                "timestamp" | "timestampMs" | "diagnosticDynamics"
            )
        })
}

fn normalize_diagnostic_dynamics(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if matches!(map.get("timestampMs"), Some(Value::Number(_))) {
                map.insert("timestampMs".into(), Value::Number(0.into()));
            }
            if let Some(Value::String(url)) = map.get_mut("url")
                && let Some(normalized) = normalize_loopback_url(url)
            {
                *url = normalized;
            }
            for child in map.values_mut() {
                normalize_diagnostic_dynamics(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_diagnostic_dynamics(item);
            }
        }
        _ => {}
    }
}

fn normalize_loopback_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))
        .or_else(|| url.strip_prefix("http://127.0.0.1"))
        .or_else(|| url.strip_prefix("http://localhost"))?;
    let path = rest.find('/').map_or("", |idx| &rest[idx..]);
    // The harness roots servers under a random /<16-hex> path secret (like
    // the ephemeral port): strip it so fixtures compare harness-agnostic URLs.
    let path = path.strip_prefix('/').map_or(path, |bare| {
        let (head, tail) = bare.split_at(bare.find('/').unwrap_or(bare.len()));
        if head.len() == 16 && head.bytes().all(|b| b.is_ascii_hexdigit()) {
            tail
        } else {
            path
        }
    });
    Some(format!("http://127.0.0.1{path}"))
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
