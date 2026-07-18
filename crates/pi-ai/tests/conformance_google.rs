//! Source-grounded Google Generative AI and Vertex AI provider conformance.

#[path = "support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use futures::{FutureExt, StreamExt, future::BoxFuture};
use pi_ai::{
    AssistantMessageEvent, Context, Model, Provider, ProviderError, StreamOptions,
    auth::{AMBIENT_AUTH_MARKER, is_ambient_auth_marker},
    providers::{GoogleGenerativeAi, GoogleVertex, VertexTokenProvider, VertexTokenRequest},
};
use serde::Deserialize;
use serde_json::{Map, Value};
use support::{
    golden::{self, GoldenError},
    http::{CapturedRequest, HttpHarnessError, LocalHttpServer, ResponseChunk, ResponseSpec},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const FIXTURE_ROOT: &str = "tests/fixtures/providers";
const TRANSPORT_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "accept-encoding",
    "connection",
    "user-agent",
];

#[derive(Debug, Error)]
enum ConformanceError {
    #[error(transparent)]
    Golden(#[from] GoldenError),
    #[error(transparent)]
    Http(#[from] HttpHarnessError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    HeaderName(#[from] axum::http::header::InvalidHeaderName),
    #[error(transparent)]
    HeaderValue(#[from] axum::http::header::InvalidHeaderValue),
    #[error(transparent)]
    Status(#[from] axum::http::status::InvalidStatusCode),
    #[error("{0}")]
    Message(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    path: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    test: Option<String>,
    lines: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureOptions {
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, Option<String>>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    extra: Map<String, Value>,
    #[serde(default)]
    abort_after_start: bool,
    #[serde(default)]
    vertex_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseChunkSpec {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureResponse {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    keep_open: bool,
    chunks: Vec<ResponseChunkSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    name: String,
    provenance: Provenance,
    model: Model,
    context: Context,
    options: FixtureOptions,
    request: ExpectedRequest,
    response: FixtureResponse,
    expected_events: Vec<Value>,
    expected_terminal: String,
    #[serde(default)]
    normalizations: Vec<String>,
}

#[derive(Debug)]
struct TokenSource {
    token: String,
    calls: AtomicUsize,
}

impl VertexTokenProvider for TokenSource {
    fn token(&self, request: VertexTokenRequest) -> BoxFuture<'_, Result<String, ProviderError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let token = self.token.clone();
        async move {
            if request.scope != "https://www.googleapis.com/auth/cloud-platform" {
                return Err(ProviderError::new(format!(
                    "unexpected Vertex scope: {}",
                    request.scope
                )));
            }
            if token.is_empty() {
                return Err(ProviderError::new("empty injected Vertex token"));
            }
            Ok(token)
        }
        .boxed()
    }
}

#[derive(Clone, Copy)]
enum ApiShape {
    GenerativeAi,
    Vertex,
}

#[tokio::test]
async fn google_generative_ai_conformance() -> Result<(), ConformanceError> {
    run_fixture_dir("google-generative-ai", ApiShape::GenerativeAi).await
}

#[tokio::test]
async fn google_vertex_conformance() -> Result<(), ConformanceError> {
    run_fixture_dir("google-vertex", ApiShape::Vertex).await
}

#[tokio::test]
#[ignore = "credential-gated live Gemini smoke; set GEMINI_API_KEY"]
async fn live_gemini_smoke() -> Result<(), ConformanceError> {
    let key = env::var("GEMINI_API_KEY").map_err(|_| {
        ConformanceError::Message("GEMINI_API_KEY is required for live_gemini_smoke".into())
    })?;
    if key.is_empty() {
        return Err(ConformanceError::Message("GEMINI_API_KEY is empty".into()));
    }
    let provider = GoogleGenerativeAi::new(reqwest::Client::new());
    let model = Model {
        id: "gemini-2.5-flash".into(),
        name: "Gemini 2.5 Flash".into(),
        api: "google-generative-ai".into(),
        provider: "google".into(),
        base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![pi_ai::ModelInput::Text],
        cost: pi_ai::ModelCost::default(),
        context_window: 1_048_576,
        max_tokens: 64,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    };
    let context = Context {
        system_prompt: None,
        messages: vec![pi_ai::Message::User(pi_ai::UserMessage::new(
            pi_ai::UserMessageContent::Text("Reply with exactly: ok".into()),
            0,
        ))],
        tools: None,
    };
    let events = provider
        .stream(
            &model,
            context,
            StreamOptions {
                api_key: Some(key),
                max_tokens: Some(16),
                ..StreamOptions::default()
            },
        )
        .collect::<Vec<_>>()
        .await;
    ensure_live_stream_shape("Gemini", &events)
}

#[tokio::test]
#[ignore = "credential-gated live Vertex smoke; set VERTEX_ACCESS_TOKEN + GOOGLE_CLOUD_PROJECT"]
async fn live_vertex_smoke() -> Result<(), ConformanceError> {
    let token = env::var("VERTEX_ACCESS_TOKEN").map_err(|_| {
        ConformanceError::Message("VERTEX_ACCESS_TOKEN is required for live_vertex_smoke".into())
    })?;
    if token.is_empty() {
        return Err(ConformanceError::Message(
            "VERTEX_ACCESS_TOKEN is empty".into(),
        ));
    }
    let project = env::var("GOOGLE_CLOUD_PROJECT").map_err(|_| {
        ConformanceError::Message("GOOGLE_CLOUD_PROJECT is required for live_vertex_smoke".into())
    })?;
    if project.is_empty() {
        return Err(ConformanceError::Message(
            "GOOGLE_CLOUD_PROJECT is empty".into(),
        ));
    }
    let location = match env::var("GOOGLE_CLOUD_LOCATION") {
        Ok(value) if !value.is_empty() => value,
        _ => "us-central1".into(),
    };
    let source = Arc::new(TokenSource {
        token,
        calls: AtomicUsize::new(0),
    });
    let provider = GoogleVertex::new(reqwest::Client::new(), Some(source));
    let model = Model {
        id: "gemini-2.5-flash".into(),
        name: "Gemini 2.5 Flash".into(),
        api: "google-vertex".into(),
        provider: "google-vertex".into(),
        base_url: "https://{location}-aiplatform.googleapis.com".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![pi_ai::ModelInput::Text],
        cost: pi_ai::ModelCost::default(),
        context_window: 1_048_576,
        max_tokens: 64,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    };
    let context = Context {
        system_prompt: None,
        messages: vec![pi_ai::Message::User(pi_ai::UserMessage::new(
            pi_ai::UserMessageContent::Text("Reply with exactly: ok".into()),
            0,
        ))],
        tools: None,
    };
    let mut options = StreamOptions {
        api_key: Some(AMBIENT_AUTH_MARKER.into()),
        max_tokens: Some(16),
        ..StreamOptions::default()
    };
    options
        .extra
        .insert("project".into(), Value::String(project));
    options
        .extra
        .insert("location".into(), Value::String(location));
    let events = provider
        .stream(&model, context, options)
        .collect::<Vec<_>>()
        .await;
    ensure_live_stream_shape("Vertex", &events)
}

fn ensure_live_stream_shape(
    label: &str,
    events: &[Result<AssistantMessageEvent, ProviderError>],
) -> Result<(), ConformanceError> {
    if events.is_empty() {
        return Err(ConformanceError::Message(format!(
            "live {label} produced no events"
        )));
    }
    match events.first() {
        Some(Ok(AssistantMessageEvent::Start { .. })) => {}
        Some(Ok(_)) => {
            return Err(ConformanceError::Message(format!(
                "live {label} first event must be start"
            )));
        }
        Some(Err(error)) => {
            return Err(ConformanceError::Message(format!(
                "live {label} infrastructure error: {error}"
            )));
        }
        None => {
            return Err(ConformanceError::Message(format!(
                "live {label} produced no events"
            )));
        }
    }

    let mut terminal_count = 0_usize;
    for event in events.iter().skip(1) {
        match event {
            Ok(AssistantMessageEvent::Start { .. }) => {
                return Err(ConformanceError::Message(format!(
                    "live {label} emitted more than one start"
                )));
            }
            Ok(AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }) => {
                terminal_count = terminal_count.saturating_add(1);
            }
            Ok(_) => {
                if terminal_count > 0 {
                    return Err(ConformanceError::Message(format!(
                        "live {label} emitted events after terminal"
                    )));
                }
            }
            Err(error) => {
                return Err(ConformanceError::Message(format!(
                    "live {label} infrastructure error: {error}"
                )));
            }
        }
    }
    if terminal_count == 1 {
        Ok(())
    } else {
        Err(ConformanceError::Message(format!(
            "live {label} expected exactly one done|error terminal, got {terminal_count}"
        )))
    }
}

async fn run_fixture_dir(dir: &str, shape: ApiShape) -> Result<(), ConformanceError> {
    let path = fixture_path(dir);
    let records = golden::load_jsonl(&path)?;
    if records.is_empty() {
        return Err(ConformanceError::Message(format!(
            "fixture {} has no cases",
            path.display()
        )));
    }
    for (index, record) in records.into_iter().enumerate() {
        let case: Case = serde_json::from_value(record).map_err(|error| {
            ConformanceError::Message(format!(
                "invalid case {index} in {}: {error}",
                path.display()
            ))
        })?;
        run_case(shape, &case)
            .await
            .map_err(|error| ConformanceError::Message(format!("{}: {error}", case.name)))?;
    }
    Ok(())
}

async fn run_case(shape: ApiShape, case: &Case) -> Result<(), ConformanceError> {
    validate_provenance(case)?;
    let response = response_spec(&case.response)?;
    let server = LocalHttpServer::start([response]).await?;
    let mut model = case.model.clone();
    model.base_url = server.base_url();

    let options = stream_options(&case.options);
    let events = match shape {
        ApiShape::GenerativeAi => {
            let provider = GoogleGenerativeAi::new(loopback_client()?);
            provider
                .stream(&model, case.context.clone(), options)
                .collect::<Vec<_>>()
                .await
        }
        ApiShape::Vertex => {
            let token_provider = case.options.vertex_token.as_ref().map(|token| {
                Arc::new(TokenSource {
                    token: token.clone(),
                    calls: AtomicUsize::new(0),
                }) as Arc<dyn VertexTokenProvider>
            });
            let provider = GoogleVertex::new(loopback_client()?, token_provider);
            provider
                .stream(&model, case.context.clone(), options)
                .collect::<Vec<_>>()
                .await
        }
    };

    let requests = server.shutdown().await?;
    assert_request(case, &requests)?;
    assert_events(case, &events)?;
    Ok(())
}

fn validate_provenance(case: &Case) -> Result<(), ConformanceError> {
    if case.provenance.path.is_empty() || case.provenance.lines.is_empty() {
        return Err(ConformanceError::Message(
            "provenance.path and provenance.lines are required".into(),
        ));
    }
    if case.provenance.symbol.is_none() && case.provenance.test.is_none() {
        return Err(ConformanceError::Message(
            "provenance requires symbol or test".into(),
        ));
    }
    let source = workspace_root().join(&case.provenance.path);
    if !source.is_file() {
        return Err(ConformanceError::Message(format!(
            "missing provenance source {}",
            case.provenance.path
        )));
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn fixture_path(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(dir)
        .join("cases.jsonl")
}

fn loopback_client() -> Result<reqwest::Client, ConformanceError> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| ConformanceError::Message(error.to_string()))
}

fn stream_options(options: &FixtureOptions) -> StreamOptions {
    let mut stream = StreamOptions {
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        api_key: options.api_key.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        env: options.env.clone(),
        extra: options.extra.clone(),
        ..StreamOptions::default()
    };
    if options.abort_after_start {
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        stream.signal = Some(signal);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
        });
    }
    stream
}

fn response_spec(response: &FixtureResponse) -> Result<ResponseSpec, ConformanceError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &response.headers {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    let mut chunks = Vec::with_capacity(response.chunks.len());
    for chunk in &response.chunks {
        let bytes = chunk
            .text
            .as_deref()
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        let built = if let Some(delay_ms) = chunk.delay_ms {
            ResponseChunk::delayed(bytes, Duration::from_millis(delay_ms))
        } else {
            ResponseChunk::immediate(bytes)
        };
        chunks.push(built);
    }
    Ok(ResponseSpec {
        status: StatusCode::from_u16(response.status)?,
        headers,
        chunks,
        keep_open: response.keep_open,
    })
}

fn assert_request(case: &Case, requests: &[CapturedRequest]) -> Result<(), ConformanceError> {
    if requests.len() != 1 {
        return Err(ConformanceError::Message(format!(
            "expected 1 captured request, got {}",
            requests.len()
        )));
    }
    let request = &requests[0];
    let expected_method = Method::from_bytes(case.request.method.as_bytes())
        .map_err(|error| ConformanceError::Message(format!("invalid expected method: {error}")))?;
    if request.method != expected_method {
        return Err(ConformanceError::Message(format!(
            "method mismatch: got {} expected {}",
            request.method, case.request.method
        )));
    }
    if request.path != case.request.path {
        return Err(ConformanceError::Message(format!(
            "path mismatch: got {} expected {}",
            request.path, case.request.path
        )));
    }
    if request.query.as_deref() != case.request.query.as_deref() {
        return Err(ConformanceError::Message(format!(
            "query mismatch: got {:?} expected {:?}",
            request.query, case.request.query
        )));
    }

    let actual_headers = filter_headers(&request.headers);
    if actual_headers != case.request.headers {
        return Err(ConformanceError::Message(format!(
            "headers mismatch:\n actual={actual_headers:?}\n expected={:?}",
            case.request.headers
        )));
    }

    if case
        .options
        .api_key
        .as_deref()
        .is_some_and(is_ambient_auth_marker)
        && (actual_headers
            .get("x-goog-api-key")
            .is_some_and(|value| value == AMBIENT_AUTH_MARKER)
            || actual_headers
                .get("authorization")
                .is_some_and(|value| value.contains(AMBIENT_AUTH_MARKER)))
    {
        return Err(ConformanceError::Message(
            "ambient auth sentinel must never be forwarded".into(),
        ));
    }

    let body: Value = if request.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&request.body)?
    };
    if body != case.request.body {
        return Err(ConformanceError::Message(format!(
            "body mismatch:\n actual={}\n expected={}",
            serde_json::to_string_pretty(&body)?,
            serde_json::to_string_pretty(&case.request.body)?
        )));
    }
    Ok(())
}

fn filter_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut filtered = BTreeMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if TRANSPORT_HEADERS.contains(&key.as_str()) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            filtered
                .entry(key)
                .and_modify(|existing: &mut String| {
                    existing.push_str(", ");
                    existing.push_str(value);
                })
                .or_insert_with(|| value.to_owned());
        }
    }
    filtered
}

fn assert_events(
    case: &Case,
    events: &[Result<AssistantMessageEvent, ProviderError>],
) -> Result<(), ConformanceError> {
    if events.iter().any(Result::is_err) {
        return Err(ConformanceError::Message(format!(
            "infrastructure provider errors are forbidden in conformance: {events:?}"
        )));
    }
    let mut actual = events
        .iter()
        .map(|event| match event {
            Ok(event) => serde_json::to_value(event)
                .map_err(|error| ConformanceError::Message(error.to_string())),
            Err(error) => Err(ConformanceError::Message(error.to_string())),
        })
        .collect::<Result<Vec<_>, _>>()?;
    for value in &mut actual {
        golden::normalize_timestamps(value);
        normalize_dynamic_fields(value, &case.normalizations);
    }
    let mut expected = case.expected_events.clone();
    for value in &mut expected {
        golden::normalize_timestamps(value);
        normalize_dynamic_fields(value, &case.normalizations);
    }
    if actual != expected {
        return Err(ConformanceError::Message(format!(
            "events mismatch:\n actual={}\n expected={}",
            serde_json::to_string_pretty(&actual)?,
            serde_json::to_string_pretty(&expected)?
        )));
    }

    let start_count = actual
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("start"))
        .count();
    let done_count = actual
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("done"))
        .count();
    let error_count = actual
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("error"))
        .count();
    if start_count != 1 {
        return Err(ConformanceError::Message(format!(
            "expected exactly one start, got {start_count}"
        )));
    }
    match case.expected_terminal.as_str() {
        "done" => {
            if done_count != 1 || error_count != 0 {
                return Err(ConformanceError::Message(format!(
                    "expected one done and zero errors; done={done_count} error={error_count}"
                )));
            }
        }
        "error" => {
            if error_count != 1 || done_count != 0 {
                return Err(ConformanceError::Message(format!(
                    "expected one error and zero done; done={done_count} error={error_count}"
                )));
            }
        }
        other => {
            return Err(ConformanceError::Message(format!(
                "unknown expectedTerminal: {other}"
            )));
        }
    }
    Ok(())
}

fn normalize_dynamic_fields(value: &mut Value, normalizations: &[String]) {
    for rule in normalizations {
        if let Some(path) = rule.strip_prefix("path:") {
            normalize_path(value, path);
        }
    }
}

fn normalize_path(value: &mut Value, path: &str) {
    let mut current = value;
    let segments = path.split('.').collect::<Vec<_>>();
    for (index, segment) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        let (name, arr_index) = parse_segment(segment);
        match current {
            Value::Object(map) => {
                let Some(next) = map.get_mut(name) else {
                    return;
                };
                current = next;
            }
            _ => return,
        }
        if let Some(arr_index) = arr_index {
            let Value::Array(items) = current else {
                return;
            };
            let Some(next) = items.get_mut(arr_index) else {
                return;
            };
            current = next;
        }
        if is_last {
            *current = Value::String("<normalized>".into());
        }
    }
}

fn parse_segment(segment: &str) -> (&str, Option<usize>) {
    if let Some((name, rest)) = segment.split_once('[')
        && let Some(index) = rest.strip_suffix(']')
        && let Ok(index) = index.parse::<usize>()
    {
        return (name, Some(index));
    }
    (segment, None)
}
