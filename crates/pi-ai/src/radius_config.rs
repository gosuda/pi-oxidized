//! Radius gateway model-catalog discovery and conversion.
//!
//! Radius exposes its selectable pi-messages models from `GET /v1/config`.
//! This module keeps the wire payload separate from the persisted [`Model`]
//! representation and reuses the shared OAuth HTTP client for timeout and
//! cancellation behavior.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::http::{AuthHttpClient, AuthHttpError};
use crate::auth::oauth::radius::normalize_radius_gateway_url;
use crate::auth::types::OAuthCredential;
use crate::types::{Model, ModelCost, ModelInput, ThinkingLevelMap};

const CONFIG_PATH: &str = "/v1/config";
const MAX_ERROR_BODY_CHARS: usize = 512;

/// A model advertised by a Radius gateway.
///
/// The required fields mirror the shallow runtime guard in the reference
/// implementation. Unknown fields are retained so a store round trip does
/// not discard forward-compatible gateway metadata.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    /// Gateway model identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether the model supports reasoning.
    pub reasoning: bool,
    /// Input modalities accepted by the model.
    pub input: Vec<ModelInput>,
    /// Token pricing.
    pub cost: ModelCost,
    /// Context-window size in tokens.
    pub context_window: u64,
    /// Maximum output tokens.
    pub max_tokens: u64,
    /// Optional provider-specific thinking-level mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// Forward-compatible fields returned by the gateway.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A sanitized Radius gateway configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    /// Base URL used by the advertised models for pi-messages requests.
    pub base_url: String,
    /// Sanitized models advertised by the gateway.
    pub models: Vec<RadiusGatewayModel>,
}

/// Failure while loading a Radius model catalog.
///
/// Error variants intentionally do not retain request headers, credentials, or
/// the original request URL. A gateway may be user-configured, so diagnostics
/// are kept to a bounded response body and a status/error class.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RadiusCatalogError {
    /// The caller cancelled the request.
    #[error("Radius config request cancelled")]
    Cancelled,
    /// The shared client reached its request deadline.
    #[error("Radius config request timed out")]
    Timeout,
    /// The gateway returned a non-success status.
    #[error("Could not load Radius config: {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Trimmed, bounded, credential-redacted response text.
        body: String,
    },
    /// The request could not be sent.
    #[error("Could not load Radius config: request failed")]
    Request,
    /// The response body could not be read.
    #[error("Could not load Radius config: response body failed")]
    Body,
    /// The successful response was not valid Radius configuration JSON.
    #[error("Invalid Radius config")]
    InvalidConfig,
    /// The configured gateway is not an HTTP(S) URL.
    #[error("Invalid Radius gateway URL")]
    InvalidGateway,
}

/// Native Radius catalog loader backed by the shared auth HTTP client.
#[derive(Clone, Debug)]
pub struct RadiusCatalogLoader {
    http: AuthHttpClient,
}

impl RadiusCatalogLoader {
    /// Build a loader with the shared OAuth request timeout policy.
    ///
    /// # Errors
    ///
    /// Returns [`AuthHttpError`] when the underlying HTTP client cannot be
    /// constructed.
    pub fn new() -> Result<Self, AuthHttpError> {
        Ok(Self {
            http: AuthHttpClient::new()?,
        })
    }

    /// Build a loader around an already configured shared HTTP client.
    #[must_use]
    pub fn with_client(http: AuthHttpClient) -> Self {
        Self { http }
    }

    /// Load and sanitize a Radius gateway configuration.
    ///
    /// `gateway` is normalized with the canonical Radius URL normalizer. The
    /// request is always `GET <origin>/v1/config`; any path, query, or
    /// fragment on the supplied gateway is not sent to the server. A non-empty
    /// `api_key` is sent only as `Authorization: Bearer <api_key>`.
    ///
    /// # Errors
    ///
    /// Returns [`RadiusCatalogError::Http`] for a non-success status,
    /// [`RadiusCatalogError::InvalidConfig`] for an invalid success payload,
    /// and cancellation/transport errors without exposing the API key.
    pub async fn load_config(
        &self,
        gateway: &str,
        api_key: Option<&str>,
        signal: Option<&CancellationToken>,
    ) -> Result<RadiusGatewayConfig, RadiusCatalogError> {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(RadiusCatalogError::Cancelled);
        }

        let normalized = normalize_radius_gateway_url(gateway);
        let url = config_url(&normalized)?;
        load_radius_gateway_config_with_url(&self.http, &url, api_key, signal).await
    }
}

/// Load and sanitize a Radius gateway configuration with an injected HTTP
/// client.
///
/// This free function is the small seam used by callers that already own an
/// [`AuthHttpClient`].
///
/// # Errors
///
/// Returns [`RadiusCatalogError`] using the same policy as
/// [`RadiusCatalogLoader::load_config`].
pub async fn load_radius_gateway_config(
    http: &AuthHttpClient,
    gateway: &str,
    api_key: Option<&str>,
    signal: Option<&CancellationToken>,
) -> Result<RadiusGatewayConfig, RadiusCatalogError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(RadiusCatalogError::Cancelled);
    }
    let normalized = normalize_radius_gateway_url(gateway);
    let url = config_url(&normalized)?;
    load_radius_gateway_config_with_url(http, &url, api_key, signal).await
}

async fn load_radius_gateway_config_with_url(
    http: &AuthHttpClient,
    url: &str,
    api_key: Option<&str>,
    signal: Option<&CancellationToken>,
) -> Result<RadiusGatewayConfig, RadiusCatalogError> {
    let headers = non_empty_bearer_header(api_key);
    let response = http
        .get_json(url, headers.as_ref(), signal)
        .await
        .map_err(|error| map_http_error(error, api_key))?;

    if !response.ok {
        return Err(RadiusCatalogError::Http {
            status: response.status,
            body: redact_and_truncate(&response.raw_body, api_key),
        });
    }

    sanitize_radius_gateway_config(&response.body).ok_or(RadiusCatalogError::InvalidConfig)
}

fn config_url(gateway: &str) -> Result<String, RadiusCatalogError> {
    let mut url = reqwest::Url::parse(gateway).map_err(|_| RadiusCatalogError::InvalidGateway)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(RadiusCatalogError::InvalidGateway);
    }
    // This matches `new URL("/v1/config", gateway)` in the reference: the
    // configured gateway selects the origin, while its path/query do not alter
    // the discovery endpoint.
    url.set_path(CONFIG_PATH);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn non_empty_bearer_header(api_key: Option<&str>) -> Option<BTreeMap<String, String>> {
    let key = api_key.filter(|value| !value.is_empty())?;
    Some(BTreeMap::from([(
        "authorization".to_owned(),
        format!("Bearer {key}"),
    )]))
}

fn map_http_error(error: AuthHttpError, api_key: Option<&str>) -> RadiusCatalogError {
    match error {
        AuthHttpError::Cancelled => RadiusCatalogError::Cancelled,
        AuthHttpError::Request(error) if error.is_timeout() => RadiusCatalogError::Timeout,
        AuthHttpError::Request(_) => RadiusCatalogError::Request,
        AuthHttpError::Body(error) if error.is_timeout() => RadiusCatalogError::Timeout,
        AuthHttpError::Body(_) => RadiusCatalogError::Body,
        AuthHttpError::InvalidJson(_) => RadiusCatalogError::InvalidConfig,
        AuthHttpError::Http { status, body, .. } => RadiusCatalogError::Http {
            status,
            body: redact_and_truncate(&body, api_key),
        },
    }
}

fn redact_and_truncate(body: &str, api_key: Option<&str>) -> String {
    let redacted = api_key
        .filter(|value| !value.is_empty())
        .map_or_else(|| body.to_owned(), |key| body.replace(key, "[redacted]"));
    let trimmed = redacted.trim();
    let mut chars = trimmed.chars();
    let prefix: String = chars.by_ref().take(MAX_ERROR_BODY_CHARS).collect();
    if chars.next().is_none() {
        prefix
    } else {
        format!("{prefix}…")
    }
}

/// Sanitize an untrusted Radius config value using the reference's shallow
/// object checks. Models that cannot be represented by the typed Rust model
/// are dropped rather than allowing a malformed catalog to reach persistence.
#[must_use]
pub fn sanitize_radius_gateway_config(value: &Value) -> Option<RadiusGatewayConfig> {
    let object = value.as_object()?;
    let base_url = object.get("baseUrl")?.as_str()?.to_owned();
    let models = object.get("models")?.as_array()?;
    Some(RadiusGatewayConfig {
        base_url,
        models: models
            .iter()
            .filter_map(sanitize_radius_gateway_model)
            .collect(),
    })
}

fn sanitize_radius_gateway_model(value: &Value) -> Option<RadiusGatewayModel> {
    let object = value.as_object()?;
    if !object.get("id").is_some_and(Value::is_string)
        || !object.get("name").is_some_and(Value::is_string)
        || !object.get("reasoning").is_some_and(Value::is_boolean)
        || !object.get("input").is_some_and(Value::is_array)
        || !object
            .get("cost")
            .is_some_and(|cost| cost.is_object() && !cost.is_array())
        || !object.get("contextWindow").is_some_and(Value::is_number)
        || !object.get("maxTokens").is_some_and(Value::is_number)
    {
        return None;
    }

    serde_json::from_value(value.clone()).ok()
}

/// Read a legacy `gatewayConfig` OAuth extra, if it is valid.
///
/// This is intentionally an import-only helper. New refreshes persist the
/// converted model list in [`crate::catalog::ModelsStoreEntry`] and do not
/// mutate the credential store.
#[must_use]
pub fn get_radius_credential_config(credential: &OAuthCredential) -> Option<RadiusGatewayConfig> {
    credential
        .extra
        .get("gatewayConfig")
        .and_then(sanitize_radius_gateway_config)
}

/// Convert a Radius config into provider-scoped native models.
///
/// Each model uses the `pi-messages` API, the supplied `provider_id`, and the
/// config's advertised `baseUrl`. Empty required strings, empty modality lists,
/// and values rejected by the typed model contract are omitted; this is the
/// necessary typed boundary after the reference's shallow filter.
#[must_use]
pub fn get_radius_models_from_config(
    provider_id: &str,
    config: &RadiusGatewayConfig,
) -> Vec<Model> {
    let reserved: BTreeSet<&str> = [
        "id",
        "name",
        "api",
        "provider",
        "baseUrl",
        "reasoning",
        "thinkingLevelMap",
        "input",
        "cost",
        "contextWindow",
        "maxTokens",
    ]
    .into_iter()
    .collect();

    config
        .models
        .iter()
        .filter(|model| {
            !model.id.trim().is_empty() && !model.name.trim().is_empty() && !model.input.is_empty()
        })
        .map(|model| {
            let mut extra = model.extra.clone();
            extra.retain(|key, _| !reserved.contains(key.as_str()));
            Model {
                id: model.id.clone(),
                name: model.name.clone(),
                api: "pi-messages".to_owned(),
                provider: provider_id.to_owned(),
                base_url: config.base_url.clone(),
                reasoning: model.reasoning,
                thinking_level_map: model.thinking_level_map.clone(),
                input: model.input.clone(),
                cost: model.cost.clone(),
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                headers: None,
                compat: None,
                extra,
            }
        })
        .collect()
}

/// Convert a legacy OAuth credential's cached catalog into native models.
#[must_use]
pub fn get_radius_models(provider_id: &str, credential: &OAuthCredential) -> Vec<Model> {
    get_radius_credential_config(credential)
        .as_ref()
        .map_or_else(Vec::new, |config| {
            get_radius_models_from_config(provider_id, config)
        })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        for _ in 0..16 {
            let Ok(size) = stream.read(&mut buffer) else {
                break;
            };
            if size == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..size]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn server(
        response: String,
        request: Arc<Mutex<Option<String>>>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let request_text = read_request(&mut stream);
            if let Ok(mut slot) = request.lock() {
                *slot = Some(request_text);
            }
            let _ = stream.write_all(response.as_bytes());
        });
        Ok(format!("http://{address}/gateway?ignored=yes"))
    }

    fn config_json() -> Value {
        serde_json::json!({
            "baseUrl": "http://models.example/v1",
            "models": [{
                "id": "auto",
                "name": "Auto",
                "reasoning": true,
                "thinkingLevelMap": {"low": "low"},
                "input": ["text", "image"],
                "cost": {
                    "input": 1.0,
                    "output": 2.0,
                    "cacheRead": 0.1,
                    "cacheWrite": 0.2
                },
                "contextWindow": 128_000,
                "maxTokens": 8192,
                "metadata": {"vendor": "radius"}
            }, {
                "id": "drop-me",
                "name": "Drop me",
                "reasoning": "yes",
                "input": ["text"],
                "cost": {},
                "contextWindow": 128_000,
                "maxTokens": 8192
            }]
        })
    }

    #[tokio::test]
    async fn loads_config_from_v1_endpoint_and_bearer()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = Arc::new(Mutex::new(None));
        let gateway = server(
            http_response("200 OK", &config_json().to_string()),
            Arc::clone(&request),
        )?;
        let loader = RadiusCatalogLoader::new()?;
        let config = loader.load_config(&gateway, Some("secret"), None).await?;
        assert_eq!(config.base_url, "http://models.example/v1");
        assert_eq!(config.models.len(), 1);
        let request = request
            .lock()
            .expect("request captured")
            .clone()
            .expect("request sent");
        assert!(request.starts_with("GET /v1/config "));
        assert!(request.contains("authorization: Bearer secret"));
        Ok(())
    }

    #[tokio::test]
    async fn status_body_is_bounded_and_redacted()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let body = format!("  prefix secret {} suffix  ", "x".repeat(900));
        let gateway = server(
            http_response("401 Unauthorized", &body),
            Arc::new(Mutex::new(None)),
        )?;
        let loader = RadiusCatalogLoader::new()?;
        let error = loader
            .load_config(&gateway, Some("secret"), None)
            .await
            .expect_err("status should fail");
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.ends_with('…'));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_stops_body_read() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let token = CancellationToken::new();
        token.cancel();
        let loader = RadiusCatalogLoader::new()?;
        let error = loader
            .load_config("http://127.0.0.1:1", None, Some(&token))
            .await
            .expect_err("cancelled request should fail");
        assert_eq!(error, RadiusCatalogError::Cancelled);
        Ok(())
    }

    #[test]
    fn shallow_config_guard_drops_malformed_models() {
        let mut value = config_json();
        value["models"][0]["contextWindow"] = serde_json::json!("128000");
        let config = sanitize_radius_gateway_config(&value).expect("config shape");
        assert!(config.models.is_empty());
        assert!(sanitize_radius_gateway_config(&serde_json::json!([])).is_none());
    }

    #[test]
    fn model_conversion_sets_pi_messages_and_gateway_url() {
        let config = sanitize_radius_gateway_config(&config_json()).expect("config shape");
        let models = get_radius_models_from_config("radius-dev", &config);
        assert_eq!(models[0].api, "pi-messages");
        assert_eq!(models[0].provider, "radius-dev");
        assert_eq!(models[0].base_url, "http://models.example/v1");
    }
}
