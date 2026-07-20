//! Native Google Vertex AI `GenerateContent` adapter.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use google_cloud_auth::credentials::{
    self, AccessTokenCredentials, external_account as google_external_account,
    impersonated as google_impersonated, mds as google_mds,
    service_account as google_service_account, user_account as google_user_account,
};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::provider::{Provider, ProviderError, StreamOptions};
use crate::types::{AssistantMessage, AssistantMessageEvent, Context, Model, ModelThinkingLevel};

use super::shared::google::{
    EVENT_CAPACITY, GoogleFailure, GoogleThinkingLevel, build_request_body, consume_response,
    emit_failure,
};
use super::shared::truncate_error_body;
use super::stream_state::ProviderEventSender;
use super::transport::{HttpTransport, TransportError};

const API_VERSION: &str = "v1";
const API_KEY_HEADER: &str = "x-goog-api-key";
const CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Context supplied to an injected Vertex bearer-token source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VertexTokenRequest {
    /// Google Cloud project that owns the request.
    pub project: String,
    /// Vertex location used by the endpoint.
    pub location: String,
    /// OAuth scope required by Vertex AI.
    pub scope: &'static str,
    /// Optional `GOOGLE_APPLICATION_CREDENTIALS` path from a non-process env overlay.
    ///
    /// When set, [`DefaultVertexTokenProvider`] loads this file through the official
    /// `google-cloud-auth` credential builders and never mutates process environment.
    pub application_credentials: Option<String>,
    /// Optional quota project (`GOOGLE_CLOUD_QUOTA_PROJECT`) from overlay/options.
    pub quota_project_id: Option<String>,
}

/// Object-safe source for a caller-managed Vertex OAuth bearer token.
pub trait VertexTokenProvider: Send + Sync {
    /// Return a current bearer token for one Vertex request.
    fn token(&self, request: VertexTokenRequest) -> BoxFuture<'_, Result<String, ProviderError>>;
}

/// Production Vertex bearer-token source backed by `google-cloud-auth` 1.14.0.
///
/// This provider uses the official Application Default Credentials builders and the
/// SDK's built-in token cache/refresh. Explicit overlay credential files are loaded
/// through the documented credential builders (`service_account`, `authorized_user`,
/// `impersonated_service_account`, `external_account`) without process-global environment
/// mutation. When no overlay credential is present, ADC
/// ([`credentials::Builder::default`]) is used, including the metadata-server fallback.
///
/// Inject a different [`VertexTokenProvider`] for tests; use this type as the registry
/// production default.
#[derive(Clone, Default)]
pub struct DefaultVertexTokenProvider {
    /// Optional metadata-server base URL override (tests / advanced MDS-only selection).
    metadata_endpoint: Option<String>,
    /// Cached official SDK credential handles keyed by source + scope + quota project.
    cache: Arc<Mutex<HashMap<CredentialCacheKey, AccessTokenCredentials>>>,
}

impl fmt::Debug for DefaultVertexTokenProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DefaultVertexTokenProvider")
            .field("metadata_endpoint", &self.metadata_endpoint)
            .field(
                "cached_credentials",
                &self.cache.lock().map_or(0, |guard| guard.len()),
            )
            .finish()
    }
}

impl DefaultVertexTokenProvider {
    /// Construct a production provider that uses the official ADC chain by default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a provider that prefers the metadata server at `endpoint` when no
    /// overlay credential file is supplied.
    ///
    /// Uses [`google_cloud_auth::credentials::mds::Builder`] directly — not a custom
    /// metadata client. Intended for tests and specialized deployments.
    #[must_use]
    pub fn with_metadata_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            metadata_endpoint: Some(endpoint.into()),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn credential_cache_key(
        request: &VertexTokenRequest,
        metadata_endpoint: Option<&str>,
    ) -> CredentialCacheKey {
        CredentialCacheKey {
            application_credentials: request.application_credentials.clone(),
            quota_project_id: request.quota_project_id.clone(),
            scope: request.scope.to_owned(),
            metadata_endpoint: metadata_endpoint.map(str::to_owned),
        }
    }

    fn build_credentials(
        &self,
        request: &VertexTokenRequest,
    ) -> Result<AccessTokenCredentials, ProviderError> {
        let scopes = [request.scope];
        if let Some(path) = request
            .application_credentials
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            return build_credentials_from_file(
                Path::new(path),
                &scopes,
                request.quota_project_id.as_deref(),
            );
        }
        if let Some(endpoint) = self.metadata_endpoint.as_deref() {
            return build_mds_credentials(endpoint, &scopes, request.quota_project_id.as_deref());
        }
        build_adc_credentials(&scopes, request.quota_project_id.as_deref())
    }

    async fn access_token(&self, request: &VertexTokenRequest) -> Result<String, ProviderError> {
        let key = Self::credential_cache_key(request, self.metadata_endpoint.as_deref());
        let cached = {
            let guard = self
                .cache
                .lock()
                .map_err(|_| ProviderError::new("Vertex AI credential cache lock is poisoned"))?;
            guard.get(&key).cloned()
        };
        let credentials = if let Some(credentials) = cached {
            credentials
        } else {
            let credentials = self.build_credentials(request)?;
            let mut guard = self
                .cache
                .lock()
                .map_err(|_| ProviderError::new("Vertex AI credential cache lock is poisoned"))?;
            guard
                .entry(key)
                .or_insert_with(|| credentials.clone())
                .clone()
        };
        let access_token = credentials
            .access_token()
            .await
            .map_err(|error| map_credentials_error(&error))?;
        let token = access_token.token.trim();
        if token.is_empty() {
            return Err(ProviderError::new(
                "Vertex AI credentials returned an empty access token",
            ));
        }
        Ok(token.to_owned())
    }
}

impl VertexTokenProvider for DefaultVertexTokenProvider {
    fn token(&self, request: VertexTokenRequest) -> BoxFuture<'_, Result<String, ProviderError>> {
        Box::pin(async move { self.access_token(&request).await })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CredentialCacheKey {
    application_credentials: Option<String>,
    quota_project_id: Option<String>,
    scope: String,
    metadata_endpoint: Option<String>,
}

fn build_adc_credentials(
    scopes: &[&str],
    quota_project_id: Option<&str>,
) -> Result<AccessTokenCredentials, ProviderError> {
    let mut builder = credentials::Builder::default().with_scopes(scopes.iter().copied());
    if let Some(quota_project_id) = quota_project_id {
        builder = builder.with_quota_project_id(quota_project_id);
    }
    builder
        .build_access_token_credentials()
        .map_err(|error| map_build_error(&error))
}

fn build_mds_credentials(
    endpoint: &str,
    scopes: &[&str],
    quota_project_id: Option<&str>,
) -> Result<AccessTokenCredentials, ProviderError> {
    let mut builder = google_mds::Builder::default()
        .with_endpoint(endpoint)
        .with_scopes(scopes.iter().copied());
    if let Some(quota_project_id) = quota_project_id {
        builder = builder.with_quota_project_id(quota_project_id);
    }
    builder
        .build_access_token_credentials()
        .map_err(|error| map_build_error(&error))
}

fn build_credentials_from_file(
    path: &Path,
    scopes: &[&str],
    quota_project_id: Option<&str>,
) -> Result<AccessTokenCredentials, ProviderError> {
    let contents = fs::read_to_string(path).map_err(|error| {
        ProviderError::new(format!(
            "failed to read Google application credentials from {}: {error}",
            display_path(path)
        ))
    })?;
    let json: Value = serde_json::from_str(&contents).map_err(|error| {
        ProviderError::new(format!(
            "failed to parse Google application credentials from {}: {error}",
            display_path(path)
        ))
    })?;
    build_credentials_from_json(json, scopes, quota_project_id)
}

fn build_credentials_from_json(
    json: Value,
    scopes: &[&str],
    quota_project_id: Option<&str>,
) -> Result<AccessTokenCredentials, ProviderError> {
    let credential_type = json
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProviderError::new(
                "Google application credentials JSON is missing a non-empty \"type\" field",
            )
        })?;
    match credential_type {
        "service_account" => {
            let mut builder = google_service_account::Builder::new(json).with_access_specifier(
                google_service_account::AccessSpecifier::from_scopes(scopes.iter().copied()),
            );
            if let Some(quota_project_id) = quota_project_id {
                builder = builder.with_quota_project_id(quota_project_id);
            }
            builder
                .build_access_token_credentials()
                .map_err(|error| map_build_error(&error))
        }
        "authorized_user" => {
            let mut builder =
                google_user_account::Builder::new(json).with_scopes(scopes.iter().copied());
            if let Some(quota_project_id) = quota_project_id {
                builder = builder.with_quota_project_id(quota_project_id);
            }
            builder
                .build_access_token_credentials()
                .map_err(|error| map_build_error(&error))
        }
        "impersonated_service_account" => {
            let mut builder =
                google_impersonated::Builder::new(json).with_scopes(scopes.iter().copied());
            if let Some(quota_project_id) = quota_project_id {
                builder = builder.with_quota_project_id(quota_project_id);
            }
            builder
                .build_access_token_credentials()
                .map_err(|error| map_build_error(&error))
        }
        "external_account" => {
            let mut builder =
                google_external_account::Builder::new(json).with_scopes(scopes.iter().copied());
            if let Some(quota_project_id) = quota_project_id {
                builder = builder.with_quota_project_id(quota_project_id);
            }
            builder
                .build_access_token_credentials()
                .map_err(|error| map_build_error(&error))
        }
        other => Err(ProviderError::new(format!(
            "unsupported Google application credentials type `{other}`"
        ))),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn map_build_error(error: &google_cloud_auth::build_errors::Error) -> ProviderError {
    // Never forward raw credential JSON or private-key material that a source error might
    // carry; surface only the stable builder classification.
    let kind = if error.is_loading() {
        "loading"
    } else if error.is_parsing() {
        "parsing"
    } else if error.is_unknown_type() {
        "unknown_type"
    } else if error.is_missing_field() {
        "missing_field"
    } else if error.is_not_supported() {
        "not_supported"
    } else {
        "build"
    };
    ProviderError::new(format!("Vertex AI credentials build failed ({kind})"))
}

fn map_credentials_error(error: &google_cloud_auth::errors::CredentialsError) -> ProviderError {
    if error.is_transient() {
        ProviderError::new("Vertex AI token acquisition failed (transient)")
    } else {
        ProviderError::new("Vertex AI token acquisition failed")
    }
}

/// Streams Vertex AI's `GenerateContent` SSE API.
#[derive(Clone)]
pub struct GoogleVertex {
    transport: HttpTransport,
    token_provider: Option<Arc<dyn VertexTokenProvider>>,
    tool_call_counter: Arc<AtomicU64>,
}

impl fmt::Debug for GoogleVertex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleVertex")
            .field("transport", &self.transport)
            .field("has_token_provider", &self.token_provider.is_some())
            .finish_non_exhaustive()
    }
}

impl GoogleVertex {
    /// Construct an adapter with an optional caller-managed bearer-token source.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        token_provider: Option<Arc<dyn VertexTokenProvider>>,
    ) -> Self {
        Self {
            transport: HttpTransport::new(client),
            token_provider,
            tool_call_counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Provider for GoogleVertex {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let model = model.clone();
        let transport = self.transport.clone();
        let token_provider = self.token_provider.clone();
        let tool_call_counter = Arc::clone(&self.tool_call_counter);
        let (sender, stream) = ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        tokio::spawn(async move {
            let mut output = AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            if sender.start(output.clone()).await.is_err() {
                return;
            }
            if let Err(failure) = run_request(
                &transport,
                VertexRequestInput {
                    token_provider,
                    model: model.clone(),
                    context,
                    options,
                },
                &sender,
                &mut output,
                &tool_call_counter,
            )
            .await
            {
                emit_failure(&sender, &mut output, failure).await;
            }
        });
        stream
    }
}

struct VertexRequestInput {
    token_provider: Option<Arc<dyn VertexTokenProvider>>,
    model: Model,
    context: Context,
    options: StreamOptions,
}

async fn run_request(
    transport: &HttpTransport,
    input: VertexRequestInput,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    tool_call_counter: &AtomicU64,
) -> Result<(), GoogleFailure> {
    let VertexRequestInput {
        token_provider,
        model,
        context,
        options,
    } = input;
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(GoogleFailure::aborted());
    }
    let auth = resolve_auth(&options, token_provider.as_ref()).await?;
    let thinking = thinking_config(&model, &options)?;
    let mut payload = build_request_body(&model, &context, &options, thinking);
    if let Some(callback) = &options.on_payload {
        callback(&mut payload, &model)
            .await
            .map_err(|error| GoogleFailure::error(error.to_string()))?;
    }

    let endpoint = endpoint(&model, &options, &auth)?;
    let headers = build_headers(&model, &options, &auth)?;
    let mut request = transport.post(endpoint).headers(headers).json(&payload);
    if let Some(timeout_ms) = options.timeout_ms {
        request = request.timeout(Duration::from_millis(timeout_ms));
    }
    let request = request.build().map_err(|error| {
        GoogleFailure::error(format!("failed to build Google Vertex request: {error}"))
    })?;
    let response = transport
        .execute(
            request,
            &model,
            options.signal.as_ref(),
            options.on_response.as_ref(),
        )
        .await
        .map_err(map_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = HttpTransport::read_error_body(response, options.signal.as_ref())
            .await
            .map_err(map_transport_error)?;
        return Err(GoogleFailure::error(format!(
            "Google Vertex API error ({}): {}",
            status.as_u16(),
            truncate_error_body(&body)
        )));
    }

    consume_response(
        response,
        &model,
        &options,
        sender,
        output,
        tool_call_counter,
    )
    .await
}

fn map_transport_error(error: TransportError) -> GoogleFailure {
    match error {
        TransportError::Cancelled => GoogleFailure::aborted(),
        TransportError::Request(error) | TransportError::Body(error) => {
            GoogleFailure::error(format!("Google Vertex request failed: {error}"))
        }
        TransportError::Callback(error) => GoogleFailure::error(error.to_string()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum VertexAuth {
    ApiKey(String),
    Bearer {
        token: String,
        project: String,
        location: String,
    },
}

async fn resolve_auth(
    options: &StreamOptions,
    token_provider: Option<&Arc<dyn VertexTokenProvider>>,
) -> Result<VertexAuth, GoogleFailure> {
    if let Some(api_key) = resolve_api_key(options) {
        return Ok(VertexAuth::ApiKey(api_key));
    }
    let project = resolve_project(options)?;
    let location = resolve_location(options)?;
    let provider = token_provider
        .ok_or_else(|| GoogleFailure::error("Vertex AI bearer-token provider is not configured"))?;
    let request = VertexTokenRequest {
        project: project.clone(),
        location: location.clone(),
        scope: CLOUD_PLATFORM_SCOPE,
        application_credentials: env_string(options, "GOOGLE_APPLICATION_CREDENTIALS"),
        quota_project_id: env_string(options, "GOOGLE_CLOUD_QUOTA_PROJECT")
            .or_else(|| option_string(options, "quotaProject")),
    };
    let token = if let Some(signal) = &options.signal {
        tokio::select! {
            () = signal.cancelled() => return Err(GoogleFailure::aborted()),
            result = provider.token(request) => result,
        }
    } else {
        provider.token(request).await
    }
    .map_err(|error| GoogleFailure::error(error.to_string()))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(GoogleFailure::error(
            "Vertex AI bearer-token provider returned an empty token",
        ));
    }
    Ok(VertexAuth::Bearer {
        token: token.to_owned(),
        project,
        location,
    })
}

fn resolve_api_key(options: &StreamOptions) -> Option<String> {
    let api_key = options.api_key.as_deref()?.trim();
    if api_key.is_empty()
        || api_key == CREDENTIALS_MARKER
        || (api_key.starts_with('<') && api_key.ends_with('>') && api_key.len() > 2)
    {
        None
    } else {
        Some(api_key.to_owned())
    }
}

fn resolve_project(options: &StreamOptions) -> Result<String, GoogleFailure> {
    option_string(options, "project")
        .or_else(|| env_string(options, "GOOGLE_CLOUD_PROJECT"))
        .or_else(|| env_string(options, "GCLOUD_PROJECT"))
        .ok_or_else(|| {
            GoogleFailure::error(
                "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options.",
            )
        })
}

fn resolve_location(options: &StreamOptions) -> Result<String, GoogleFailure> {
    option_string(options, "location")
        .or_else(|| env_string(options, "GOOGLE_CLOUD_LOCATION"))
        .ok_or_else(|| {
            GoogleFailure::error(
                "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.",
            )
        })
}

fn option_string(options: &StreamOptions, key: &str) -> Option<String> {
    options
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn env_string(options: &StreamOptions, key: &str) -> Option<String> {
    options
        .env
        .as_ref()
        .and_then(|env| env.get(key))
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn endpoint(
    model: &Model,
    options: &StreamOptions,
    auth: &VertexAuth,
) -> Result<String, GoogleFailure> {
    let model_path = vertex_model_path(&model.id)?;
    let custom_base = resolve_custom_base_url(&model.base_url);
    let configured_version =
        option_string(options, "apiVersion").unwrap_or_else(|| API_VERSION.into());

    let (base, include_version, prefix) = if let Some(base) = custom_base {
        let include_version = !base_url_includes_api_version(base);
        (base, include_version, String::new())
    } else {
        match auth {
            VertexAuth::ApiKey(_) => ("https://aiplatform.googleapis.com", true, String::new()),
            VertexAuth::Bearer {
                project, location, ..
            } => {
                let base = format!("https://{location}-aiplatform.googleapis.com");
                let prefix = if model_path.starts_with("projects/") {
                    String::new()
                } else {
                    format!("projects/{project}/locations/{location}/")
                };
                return finish_endpoint(&base, true, &configured_version, &prefix, &model_path);
            }
        }
    };
    finish_endpoint(
        base,
        include_version,
        &configured_version,
        &prefix,
        &model_path,
    )
}

fn finish_endpoint(
    base: &str,
    include_version: bool,
    api_version: &str,
    prefix: &str,
    model_path: &str,
) -> Result<String, GoogleFailure> {
    reqwest::Url::parse(base)
        .map_err(|error| GoogleFailure::error(format!("invalid Vertex base URL: {error}")))?;
    let version = if include_version && !api_version.is_empty() {
        format!("{}/", api_version.trim_matches('/'))
    } else {
        String::new()
    };
    Ok(format!(
        "{}/{version}{prefix}{model_path}:streamGenerateContent?alt=sse",
        base.trim_end_matches('/')
    ))
}

fn resolve_custom_base_url(base_url: &str) -> Option<&str> {
    let base_url = base_url.trim();
    (!base_url.is_empty() && !base_url.contains("{location}")).then_some(base_url)
}

fn base_url_includes_api_version(base_url: &str) -> bool {
    let parsed = reqwest::Url::parse(base_url).ok();
    let path = parsed.as_ref().map_or(base_url, |url| url.path());
    path.split('/').any(|segment| {
        let Some(version) = segment.strip_prefix('v') else {
            return false;
        };
        let digit_count = version.chars().take_while(char::is_ascii_digit).count();
        if digit_count == 0 {
            return false;
        }
        let suffix = &version[digit_count..];
        suffix.is_empty()
            || suffix
                .strip_prefix("beta")
                .is_some_and(|value| value.chars().all(|character| character.is_ascii_digit()))
    })
}

fn vertex_model_path(model_id: &str) -> Result<String, GoogleFailure> {
    if model_id.is_empty()
        || model_id.contains("..")
        || model_id.contains('?')
        || model_id.contains('&')
    {
        return Err(GoogleFailure::error("invalid Vertex model identifier"));
    }
    if model_id.starts_with("publishers/")
        || model_id.starts_with("projects/")
        || model_id.starts_with("models/")
    {
        return Ok(model_id.to_owned());
    }
    if let Some((publisher, model)) = model_id.split_once('/') {
        return Ok(format!("publishers/{publisher}/models/{model}"));
    }
    Ok(format!("publishers/google/models/{model_id}"))
}

fn build_headers(
    model: &Model,
    options: &StreamOptions,
    auth: &VertexAuth,
) -> Result<HeaderMap, GoogleFailure> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    match auth {
        VertexAuth::ApiKey(api_key) => insert_header(&mut headers, API_KEY_HEADER, api_key)?,
        VertexAuth::Bearer { token, .. } => {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| GoogleFailure::error(format!("invalid bearer token: {error}")))?;
            headers.insert(AUTHORIZATION, value);
        }
    }
    apply_custom_headers(&mut headers, model, options)?;
    Ok(headers)
}

fn apply_custom_headers(
    headers: &mut HeaderMap,
    model: &Model,
    options: &StreamOptions,
) -> Result<(), GoogleFailure> {
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header(headers, name, value)?;
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| GoogleFailure::error(format!("invalid header name: {error}")))?;
            if let Some(value) = value {
                headers.insert(
                    name,
                    HeaderValue::from_str(value).map_err(|error| {
                        GoogleFailure::error(format!("invalid header value: {error}"))
                    })?,
                );
            } else {
                headers.remove(name);
            }
        }
    }
    Ok(())
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), GoogleFailure> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| GoogleFailure::error(format!("invalid header name: {error}")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|error| GoogleFailure::error(format!("invalid header value: {error}")))?;
    headers.insert(name, value);
    Ok(())
}

fn thinking_config(model: &Model, options: &StreamOptions) -> Result<Option<Value>, GoogleFailure> {
    if !model.reasoning {
        return Ok(None);
    }
    if let Some(thinking) = options.extra.get("thinking").and_then(Value::as_object) {
        let enabled = thinking
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            return Ok(Some(disabled_thinking_config(model)));
        }
        let mut config = Map::new();
        config.insert("includeThoughts".to_owned(), Value::Bool(true));
        if let Some(level) = thinking.get("level").and_then(Value::as_str) {
            let level = GoogleThinkingLevel::parse(level).ok_or_else(|| {
                GoogleFailure::error(format!("unsupported Google thinking level: {level}"))
            })?;
            config.insert(
                "thinkingLevel".to_owned(),
                Value::String(level.as_str().to_owned()),
            );
        } else if let Some(budget) = thinking.get("budgetTokens").and_then(Value::as_i64) {
            config.insert("thinkingBudget".to_owned(), Value::from(budget));
        }
        return Ok(Some(Value::Object(config)));
    }

    let Some(requested) = options.extra.get("reasoning").and_then(Value::as_str) else {
        return Ok(None);
    };
    let effort = clamp_effort(model, requested)?;
    if effort == Effort::Off {
        return Ok(Some(disabled_thinking_config(model)));
    }
    let mut config = Map::new();
    config.insert("includeThoughts".to_owned(), Value::Bool(true));
    if is_gemini3_pro(model) || is_gemini3_flash(model) {
        config.insert(
            "thinkingLevel".to_owned(),
            Value::String(thinking_level(effort, model).as_str().to_owned()),
        );
    } else {
        config.insert(
            "thinkingBudget".to_owned(),
            Value::from(thinking_budget(model, effort, options)),
        );
    }
    Ok(Some(Value::Object(config)))
}

fn disabled_thinking_config(model: &Model) -> Value {
    if is_gemini3_pro(model) {
        json!({"thinkingLevel": "LOW"})
    } else if is_gemini3_flash(model) {
        json!({"thinkingLevel": "MINIMAL"})
    } else {
        json!({"thinkingBudget": 0})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Effort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    const fn model_level(self) -> ModelThinkingLevel {
        match self {
            Self::Off => ModelThinkingLevel::Off,
            Self::Minimal => ModelThinkingLevel::Minimal,
            Self::Low => ModelThinkingLevel::Low,
            Self::Medium => ModelThinkingLevel::Medium,
            Self::High => ModelThinkingLevel::High,
            Self::Xhigh => ModelThinkingLevel::Xhigh,
            Self::Max => ModelThinkingLevel::Max,
        }
    }
}

fn clamp_effort(model: &Model, requested: &str) -> Result<Effort, GoogleFailure> {
    let requested = Effort::parse(requested).ok_or_else(|| {
        GoogleFailure::error(format!("unsupported reasoning effort: {requested}"))
    })?;
    let available = Effort::ALL
        .into_iter()
        .filter(|effort| effort_supported(model, *effort))
        .collect::<Vec<_>>();
    if available.contains(&requested) {
        return Ok(requested);
    }
    let index = Effort::ALL
        .iter()
        .position(|effort| *effort == requested)
        .unwrap_or(0);
    Effort::ALL[index..]
        .iter()
        .chain(Effort::ALL[..index].iter().rev())
        .find(|effort| available.contains(effort))
        .copied()
        .ok_or_else(|| GoogleFailure::error("model does not support reasoning"))
}

fn effort_supported(model: &Model, effort: Effort) -> bool {
    if !model.reasoning {
        return effort == Effort::Off;
    }
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|levels| levels.get(&effort.model_level()));
    if mapped == Some(&None) {
        return false;
    }
    !matches!(effort, Effort::Xhigh | Effort::Max) || mapped.is_some()
}

fn thinking_level(effort: Effort, model: &Model) -> GoogleThinkingLevel {
    if is_gemini3_pro(model) {
        match effort {
            Effort::Minimal | Effort::Low => GoogleThinkingLevel::Low,
            _ => GoogleThinkingLevel::High,
        }
    } else {
        match effort {
            Effort::Minimal => GoogleThinkingLevel::Minimal,
            Effort::Low => GoogleThinkingLevel::Low,
            Effort::Medium => GoogleThinkingLevel::Medium,
            _ => GoogleThinkingLevel::High,
        }
    }
}

fn thinking_budget(model: &Model, effort: Effort, options: &StreamOptions) -> i64 {
    let key = match effort {
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        _ => "high",
    };
    if let Some(custom) = options
        .extra
        .get("thinkingBudgets")
        .and_then(Value::as_object)
        .and_then(|budgets| budgets.get(key))
        .and_then(Value::as_i64)
    {
        return custom;
    }
    if model.id.contains("2.5-pro") {
        match effort {
            Effort::Minimal => 128,
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            _ => 32_768,
        }
    } else if model.id.contains("2.5-flash") {
        match effort {
            Effort::Minimal => 128,
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            _ => 24_576,
        }
    } else {
        -1
    }
}

fn is_gemini3_pro(model: &Model) -> bool {
    model_family_matches(&model.id, "pro")
}

fn is_gemini3_flash(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    model_family_matches(&id, "flash")
        || matches!(
            id.as_str(),
            "gemini-flash-latest" | "gemini-flash-lite-latest"
        )
}

fn model_family_matches(model_id: &str, family: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let Some(rest) = id.strip_prefix("gemini-3") else {
        return false;
    };
    let rest = if let Some(rest) = rest.strip_prefix('.') {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        &rest[digits..]
    } else {
        rest
    };
    rest.starts_with(&format!("-{family}"))
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration as StdDuration;

    use futures::{FutureExt, StreamExt};
    use tempfile::NamedTempFile;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::types::{ModelCost, ModelInput};

    fn model() -> Model {
        Model {
            id: "gemini-3-flash-preview".into(),
            name: "Gemini".into(),
            api: "google-vertex".into(),
            provider: "google-vertex".into(),
            base_url: "https://{location}-aiplatform.googleapis.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[derive(Debug)]
    struct TokenSource {
        calls: AtomicUsize,
    }

    impl VertexTokenProvider for TokenSource {
        fn token(
            &self,
            request: VertexTokenRequest,
        ) -> BoxFuture<'_, Result<String, ProviderError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            async move {
                assert_eq!(request.scope, CLOUD_PLATFORM_SCOPE);
                assert_eq!(request.project, "project-1");
                assert_eq!(request.location, "us-central1");
                Ok("token-1".into())
            }
            .boxed()
        }
    }

    fn token_request(application_credentials: Option<String>) -> VertexTokenRequest {
        VertexTokenRequest {
            project: "project-1".into(),
            location: "us-central1".into(),
            scope: CLOUD_PLATFORM_SCOPE,
            application_credentials,
            quota_project_id: Some("quota-project".into()),
        }
    }

    fn write_temp_credentials(body: &str) -> Result<NamedTempFile, String> {
        let mut file = NamedTempFile::new().map_err(|error| error.to_string())?;
        file.write_all(body.as_bytes())
            .map_err(|error| error.to_string())?;
        file.flush().map_err(|error| error.to_string())?;
        Ok(file)
    }

    fn generate_pkcs8_private_key_pem() -> Result<String, String> {
        let output = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-outform",
                "PEM",
            ])
            .output()
            .map_err(|error| format!("openssl genpkey failed to start: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "openssl genpkey exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        String::from_utf8(output.stdout).map_err(|error| error.to_string())
    }

    fn service_account_json(private_key_pem: &str) -> String {
        json!({
            "type": "service_account",
            "project_id": "project-1",
            "private_key_id": "test-private-key-id",
            "private_key": private_key_pem,
            "client_email": "vertex-tests@project-1.iam.gserviceaccount.com",
            "client_id": "1234567890",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token",
            "universe_domain": "googleapis.com",
        })
        .to_string()
    }

    fn spawn_json_token_server(body: &str) -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let body = body.to_owned();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0_u8; 16_384];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(format!("http://{address}"))
    }

    fn spawn_hanging_token_server() -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(StdDuration::from_secs(30));
            }
        });
        Ok(format!("http://{address}"))
    }

    fn bearer_options() -> StreamOptions {
        let mut options = StreamOptions::default();
        options.extra.insert("project".into(), json!("project-1"));
        options
            .extra
            .insert("location".into(), json!("us-central1"));
        options
    }

    #[tokio::test]
    async fn real_api_key_bypasses_token_source_and_project_location() -> Result<(), GoogleFailure>
    {
        let source = Arc::new(TokenSource {
            calls: AtomicUsize::new(0),
        });
        let mut options = StreamOptions {
            api_key: Some("  AIza-real  ".into()),
            ..StreamOptions::default()
        };
        options.extra.insert("project".into(), json!("ignored"));
        let trait_source: Arc<dyn VertexTokenProvider> = source.clone();
        assert_eq!(
            resolve_auth(&options, Some(&trait_source)).await?,
            VertexAuth::ApiKey("AIza-real".into())
        );
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn placeholder_key_uses_injected_bearer_token() -> Result<(), GoogleFailure> {
        let source = Arc::new(TokenSource {
            calls: AtomicUsize::new(0),
        });
        let mut options = bearer_options();
        options.api_key = Some("<authenticated>".into());
        let trait_source: Arc<dyn VertexTokenProvider> = source.clone();
        assert_eq!(
            resolve_auth(&options, Some(&trait_source)).await?,
            VertexAuth::Bearer {
                token: "token-1".into(),
                project: "project-1".into(),
                location: "us-central1".into(),
            }
        );
        assert_eq!(source.calls.load(Ordering::Relaxed), 1);
        Ok(())
    }

    #[test]
    fn endpoints_cover_bearer_api_key_and_custom_collection_scope() -> Result<(), GoogleFailure> {
        let model = model();
        let options = bearer_options();
        let bearer = VertexAuth::Bearer {
            token: "token".into(),
            project: "project-1".into(),
            location: "us-central1".into(),
        };
        assert_eq!(
            endpoint(&model, &options, &bearer)?,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/project-1/locations/us-central1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            endpoint(&model, &options, &VertexAuth::ApiKey("key".into()))?,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );

        let mut custom = model;
        custom.base_url = "http://127.0.0.1:8080/proxy/v1".into();
        assert_eq!(
            endpoint(&custom, &options, &bearer)?,
            "http://127.0.0.1:8080/proxy/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        custom.base_url = "http://127.0.0.1:8080/proxy".into();
        assert_eq!(
            endpoint(&custom, &options, &bearer)?,
            "http://127.0.0.1:8080/proxy/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
        );
        Ok(())
    }

    #[test]
    fn auth_headers_never_mix_api_key_and_bearer() -> Result<(), GoogleFailure> {
        let options = StreamOptions::default();
        let api_key = build_headers(&model(), &options, &VertexAuth::ApiKey("key".into()))?;
        assert_eq!(
            api_key.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()),
            Some("key")
        );
        assert!(api_key.get(AUTHORIZATION).is_none());

        let bearer = build_headers(
            &model(),
            &options,
            &VertexAuth::Bearer {
                token: "token".into(),
                project: "project".into(),
                location: "location".into(),
            },
        )?;
        assert_eq!(
            bearer.get(AUTHORIZATION).and_then(|v| v.to_str().ok()),
            Some("Bearer token")
        );
        assert!(bearer.get(API_KEY_HEADER).is_none());
        Ok(())
    }

    #[test]
    fn vertex_thinking_does_not_apply_gemma4_exception() -> Result<(), GoogleFailure> {
        let mut gemma = model();
        gemma.id = "gemma-4-26b".into();
        let mut options = StreamOptions::default();
        options
            .extra
            .insert("thinking".into(), json!({"enabled": false}));
        assert_eq!(
            thinking_config(&gemma, &options)?,
            Some(json!({"thinkingBudget": 0}))
        );
        options.extra.clear();
        options.extra.insert("reasoning".into(), json!("minimal"));
        assert_eq!(
            thinking_config(&gemma, &options)?,
            Some(json!({"includeThoughts": true, "thinkingBudget": -1}))
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_token_source_emits_start_then_one_error() {
        let provider = GoogleVertex::new(reqwest::Client::new(), None);
        let events = provider
            .stream(&model(), Context::default(), bearer_options())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(AssistantMessageEvent::Error { .. })))
                .count(),
            1
        );
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn default_vertex_token_provider_is_constructible_for_registry() {
        let provider = DefaultVertexTokenProvider::default();
        let debug = format!("{provider:?}");
        assert!(debug.contains("DefaultVertexTokenProvider"));
        assert!(!debug.to_lowercase().contains("secret"));
    }

    #[tokio::test]
    async fn resolve_auth_forwards_overlay_credential_path_and_quota_project()
    -> Result<(), GoogleFailure> {
        #[derive(Debug)]
        struct CaptureSource {
            seen: Mutex<Option<VertexTokenRequest>>,
        }

        impl VertexTokenProvider for CaptureSource {
            fn token(
                &self,
                request: VertexTokenRequest,
            ) -> BoxFuture<'_, Result<String, ProviderError>> {
                if let Ok(mut guard) = self.seen.lock() {
                    *guard = Some(request.clone());
                }
                async move { Ok("captured-token".into()) }.boxed()
            }
        }

        let source = Arc::new(CaptureSource {
            seen: Mutex::new(None),
        });
        let mut options = bearer_options();
        options.api_key = Some("<authenticated>".into());
        let mut env = BTreeMap::new();
        env.insert(
            "GOOGLE_APPLICATION_CREDENTIALS".into(),
            "/tmp/overlay-adc.json".into(),
        );
        env.insert("GOOGLE_CLOUD_QUOTA_PROJECT".into(), "quota-from-env".into());
        options.env = Some(env);
        let trait_source: Arc<dyn VertexTokenProvider> = source.clone();
        assert_eq!(
            resolve_auth(&options, Some(&trait_source)).await?,
            VertexAuth::Bearer {
                token: "captured-token".into(),
                project: "project-1".into(),
                location: "us-central1".into(),
            }
        );
        let seen = source
            .seen
            .lock()
            .map_err(|_| GoogleFailure::error("lock poisoned"))?
            .clone()
            .ok_or_else(|| GoogleFailure::error("token request not captured"))?;
        assert_eq!(
            seen.application_credentials.as_deref(),
            Some("/tmp/overlay-adc.json")
        );
        assert_eq!(seen.quota_project_id.as_deref(), Some("quota-from-env"));
        assert_eq!(seen.scope, CLOUD_PLATFORM_SCOPE);
        Ok(())
    }

    #[tokio::test]
    async fn default_provider_service_account_overlay_mints_jwt_bearer_token()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let private_key = generate_pkcs8_private_key_pem()?;
        let credentials = write_temp_credentials(&service_account_json(&private_key))?;
        let path = credentials
            .path()
            .to_str()
            .ok_or("temp credentials path is not utf-8")?
            .to_owned();

        let provider = DefaultVertexTokenProvider::new();
        let token = provider
            .token(token_request(Some(path.clone())))
            .await
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(token.matches('.').count(), 2, "JWT has three segments");
        assert!(!token.contains("BEGIN PRIVATE KEY"));
        assert!(!token.is_empty());

        // Second call reuses the official SDK handle (cached by provider key).
        let again = provider
            .token(token_request(Some(path)))
            .await
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(again.matches('.').count(), 2);

        let headers = build_headers(
            &model(),
            &StreamOptions::default(),
            &VertexAuth::Bearer {
                token: again.clone(),
                project: "project-1".into(),
                location: "us-central1".into(),
            },
        )
        .map_err(|error| error.to_string())?;
        let authorization = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("missing authorization header")?;
        assert!(authorization.starts_with("Bearer "));
        assert!(authorization.ends_with(&again));
        assert!(headers.get(API_KEY_HEADER).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn default_provider_user_account_overlay_uses_token_uri_without_env_mutation()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let token_body = json!({
            "access_token": "user-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let base = spawn_json_token_server(&token_body)?;
        let token_uri = format!("{base}/token");
        let credentials = write_temp_credentials(
            &json!({
                "type": "authorized_user",
                "client_id": "test-client-id.apps.googleusercontent.com",
                "client_secret": "test-client-secret",
                "refresh_token": "test-refresh-token",
                "token_uri": token_uri,
            })
            .to_string(),
        )?;
        let path = credentials
            .path()
            .to_str()
            .ok_or("temp credentials path is not utf-8")?
            .to_owned();

        let before = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
        let provider = DefaultVertexTokenProvider::new();
        let token = provider
            .token(token_request(Some(path)))
            .await
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(token, "user-access-token");
        assert_eq!(std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"), before);
        Ok(())
    }

    #[tokio::test]
    async fn default_provider_metadata_endpoint_selects_mds_builder()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let token_body = json!({
            "access_token": "metadata-access-token",
            "expires_in": 3600,
            "token_type": "Bearer",
        })
        .to_string();
        let endpoint = spawn_json_token_server(&token_body)?;
        let provider = DefaultVertexTokenProvider::with_metadata_endpoint(endpoint);
        let token = provider
            .token(token_request(None))
            .await
            .map_err(|error| error.message().to_owned())?;
        assert_eq!(token, "metadata-access-token");
        Ok(())
    }

    #[tokio::test]
    async fn default_provider_errors_are_secret_redacted()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let credentials = write_temp_credentials(
            &json!({
                "type": "authorized_user",
                "client_id": "test-client-id.apps.googleusercontent.com",
                "client_secret": "client-secret-material",
                "refresh_token": "refresh-token-material",
                "token_uri": "http://127.0.0.1:1/token",
            })
            .to_string(),
        )?;
        let path = credentials
            .path()
            .to_str()
            .ok_or("temp credentials path is not utf-8")?
            .to_owned();
        let provider = DefaultVertexTokenProvider::new();
        let Err(error) = provider.token(token_request(Some(path))).await else {
            return Err("token acquisition must fail against closed port".into());
        };
        let message = error.message();
        assert!(message.contains("Vertex AI token acquisition failed"));
        assert!(!message.contains("client-secret-material"));
        assert!(!message.contains("refresh-token-material"));
        let debug = format!("{error:?}");
        assert!(!debug.contains("client-secret-material"));
        assert!(!debug.contains("refresh-token-material"));
        Ok(())
    }

    #[tokio::test]
    async fn default_provider_token_is_cancellable()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = spawn_hanging_token_server()?;
        let credentials = write_temp_credentials(
            &json!({
                "type": "authorized_user",
                "client_id": "test-client-id.apps.googleusercontent.com",
                "client_secret": "test-client-secret",
                "refresh_token": "test-refresh-token",
                "token_uri": format!("{endpoint}/token"),
            })
            .to_string(),
        )?;
        let path = credentials
            .path()
            .to_str()
            .ok_or("temp credentials path is not utf-8")?
            .to_owned();

        let provider = DefaultVertexTokenProvider::new();
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let join = tokio::spawn(async move {
            let request = token_request(Some(path));
            tokio::select! {
                () = cancel_for_task.cancelled() => Err("cancelled".to_owned()),
                result = provider.token(request) => result.map_err(|error| error.message().to_owned()),
            }
        });
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        cancel.cancel();
        let outcome = join.await.map_err(|error| error.to_string())?;
        assert_eq!(outcome, Err("cancelled".to_owned()));
        Ok(())
    }

    #[test]
    fn missing_or_invalid_overlay_credentials_map_to_provider_error()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let provider = DefaultVertexTokenProvider::new();
        let missing = provider.build_credentials(&token_request(Some(
            "/definitely/missing/google-adc.json".into(),
        )));
        let Err(missing_error) = missing else {
            return Err("missing file must fail".into());
        };
        assert!(missing_error.message().contains("failed to read"));
        assert!(!missing_error.message().contains("private_key"));

        let invalid = write_temp_credentials(r#"{"type":"not-a-real-type"}"#)?;
        let path = invalid
            .path()
            .to_str()
            .ok_or("temp credentials path is not utf-8")?
            .to_owned();
        let unsupported = provider.build_credentials(&token_request(Some(path)));
        let Err(unsupported_error) = unsupported else {
            return Err("unknown type must fail".into());
        };
        assert!(
            unsupported_error
                .message()
                .contains("unsupported Google application credentials type")
        );
        Ok(())
    }
}
