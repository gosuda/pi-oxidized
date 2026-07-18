//! GitHub Copilot device-flow OAuth.
//!
//! Port of `.references/pi/packages/ai/src/auth/oauth/github-copilot.ts`.
//! Login is device-code only (no loopback). The stored `refresh` value is the
//! GitHub user access token; `refresh` re-exchanges it for a Copilot API token
//! via `GET /copilot_internal/v2/token` (not an RFC `refresh_token` grant).

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError};
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};

/// Base64 of the fixed GitHub OAuth app client id (matches the TypeScript decode).
pub const CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";
/// Decoded client id `Iv1.b507a08c87ecfe98`.
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Exact `User-Agent` header sent to GitHub and Copilot endpoints.
pub const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
/// Exact `Editor-Version` header.
pub const EDITOR_VERSION: &str = "vscode/1.107.0";
/// Exact `Editor-Plugin-Version` header.
pub const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
/// Exact `Copilot-Integration-Id` header.
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
/// Exact `X-GitHub-Api-Version` header on `/models` requests.
pub const COPILOT_API_VERSION: &str = "2026-06-01";

/// Five-minute skew applied to Copilot `expires_at` (seconds → ms).
pub const EXPIRES_SKEW_MS: i64 = 5 * 60 * 1000;

/// Default public GitHub host.
pub const DEFAULT_PUBLIC_DOMAIN: &str = "github.com";
/// Default public Copilot API base URL.
pub const DEFAULT_COPILOT_BASE_URL: &str = "https://api.individual.githubcopilot.com";

/// Extra key for the enterprise hostname (when not public GitHub).
pub const EXTRA_ENTERPRISE_URL: &str = "enterpriseUrl";
/// Extra key for model ids available to the account after login/refresh.
pub const EXTRA_AVAILABLE_MODEL_IDS: &str = "availableModelIds";

/// Built-in GitHub Copilot model ids used for post-login policy enablement.
///
/// Mirrors `GITHUB_COPILOT_MODELS` keys from
/// `.references/pi/packages/ai/src/providers/github-copilot.models.ts`.
pub const GITHUB_COPILOT_MODEL_IDS: &[&str] = &[
    "claude-fable-5",
    "claude-haiku-4.5",
    "claude-opus-4.5",
    "claude-opus-4.6",
    "claude-opus-4.7",
    "claude-opus-4.8",
    "claude-sonnet-4",
    "claude-sonnet-4.5",
    "claude-sonnet-4.6",
    "claude-sonnet-5",
    "gemini-2.5-pro",
    "gemini-3-flash-preview",
    "gemini-3.1-pro-preview",
    "gemini-3.5-flash",
    "gpt-4.1",
    "gpt-5-mini",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "kimi-k2.7-code",
    "mai-code-1-flash-picker",
];

#[derive(Clone, Debug)]
struct CopilotUrls {
    device_code: String,
    access_token: String,
    copilot_token: String,
}

/// GitHub Copilot OAuth flow.
#[derive(Clone, Debug)]
pub struct GitHubCopilotOAuth {
    client: AuthHttpClient,
    /// When set, rewrites every request URL onto this origin (scheme+host) for
    /// mock HTTP tests while preserving path/query.
    mock_origin: Option<String>,
    /// Model IDs sent by `POST` to `/models/{id}/policy` after login.
    enable_model_ids: Arc<[&'static str]>,
}

impl GitHubCopilotOAuth {
    /// Build a production flow with the shared OAuth HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an auth error when the HTTP client cannot be constructed.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self::with_client(
            AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
        ))
    }

    /// Build a flow around an existing HTTP client.
    #[must_use]
    pub fn with_client(client: AuthHttpClient) -> Self {
        Self {
            client,
            mock_origin: None,
            enable_model_ids: Arc::from(GITHUB_COPILOT_MODEL_IDS),
        }
    }

    /// Shared production-ready instance behind [`OAuthAuth`].
    ///
    /// # Errors
    ///
    /// Propagates client construction failure from [`Self::new`].
    pub fn shared() -> Result<Arc<dyn OAuthAuth>, AuthError> {
        Ok(Arc::new(Self::new()?))
    }

    /// Rewrite all GitHub/Copilot hosts to `origin` (for example `http://127.0.0.1:1234`).
    #[must_use]
    pub fn with_mock_origin(mut self, origin: impl Into<String>) -> Self {
        self.mock_origin = Some(origin.into());
        self
    }

    /// Override the post-login model-enable list (tests).
    #[must_use]
    pub fn with_enable_model_ids(mut self, ids: &[&'static str]) -> Self {
        self.enable_model_ids = Arc::from(ids);
        self
    }

    fn rewrite_url(&self, url: &str) -> String {
        let Some(origin) = self.mock_origin.as_deref() else {
            return url.to_owned();
        };
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return url.to_owned();
        };
        let Ok(mut base) = reqwest::Url::parse(origin) else {
            return url.to_owned();
        };
        base.set_path(parsed.path());
        base.set_query(parsed.query());
        base.set_fragment(parsed.fragment());
        base.into()
    }

    fn urls_for_domain(&self, domain: &str) -> CopilotUrls {
        CopilotUrls {
            device_code: self.rewrite_url(&format!("https://{domain}/login/device/code")),
            access_token: self.rewrite_url(&format!("https://{domain}/login/oauth/access_token")),
            copilot_token: self
                .rewrite_url(&format!("https://api.{domain}/copilot_internal/v2/token")),
        }
    }

    fn copilot_headers() -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert("User-Agent".into(), USER_AGENT.into());
        headers.insert("Editor-Version".into(), EDITOR_VERSION.into());
        headers.insert("Editor-Plugin-Version".into(), EDITOR_PLUGIN_VERSION.into());
        headers.insert(
            "Copilot-Integration-Id".into(),
            COPILOT_INTEGRATION_ID.into(),
        );
        headers
    }

    fn github_form_headers() -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert("User-Agent".into(), USER_AGENT.into());
        headers
    }

    async fn start_device_flow(
        &self,
        domain: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DeviceCodeResponse, AuthError> {
        let urls = self.urls_for_domain(domain);
        let mut fields = BTreeMap::new();
        fields.insert("client_id".into(), CLIENT_ID.into());
        fields.insert("scope".into(), "read:user".into());

        let response = self
            .client
            .post_form(
                &urls.device_code,
                &fields,
                Some(&Self::github_form_headers()),
                cancellation,
            )
            .await
            .map_err(AuthHttpError::into_auth_error)?;

        if !response.ok {
            return Err(AuthError::message(format!(
                "HTTP request failed. status={}; url={}; body={}",
                response.status, urls.device_code, response.raw_body
            )));
        }

        parse_device_code_response(&response.body)
    }

    async fn poll_for_github_access_token(
        &self,
        domain: &str,
        device: &DeviceCodeResponse,
        signal: Option<CancellationToken>,
    ) -> Result<String, AuthError> {
        let urls = self.urls_for_domain(domain);
        let client = self.client.clone();
        let device_code = device.device_code.clone();
        let headers = Self::github_form_headers();
        let poll_signal = signal.clone();

        let mut options = OAuthDeviceCodePollOptions::new(move || {
            let client = client.clone();
            let url = urls.access_token.clone();
            let device_code = device_code.clone();
            let headers = headers.clone();
            let signal = poll_signal.clone();
            async move {
                let mut fields = BTreeMap::new();
                fields.insert("client_id".into(), CLIENT_ID.into());
                fields.insert("device_code".into(), device_code);
                fields.insert(
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                );

                let response = client
                    .post_form(&url, &fields, Some(&headers), signal.as_ref())
                    .await
                    .map_err(AuthHttpError::into_auth_error)?;

                let body = &response.body;
                if let Some(access_token) = body.get("access_token").and_then(Value::as_str) {
                    return Ok(OAuthDeviceCodePollResult::Complete {
                        value: access_token.to_owned(),
                    });
                }

                if let Some(error) = body.get("error").and_then(Value::as_str) {
                    return Ok(map_device_token_error(error, body));
                }

                Ok(OAuthDeviceCodePollResult::Failed {
                    message: "Invalid device token response".into(),
                })
            }
        });
        options.interval_seconds = device.interval;
        options.expires_in_seconds.replace(device.expires_in);
        options.wait_before_first_poll = true;
        options.signal = signal;

        poll_oauth_device_code_flow(options).await
    }

    async fn refresh_github_copilot_access_token(
        &self,
        refresh_token: &str,
        enterprise_domain: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let domain = enterprise_domain.unwrap_or(DEFAULT_PUBLIC_DOMAIN);
        let urls = self.urls_for_domain(domain);

        let mut headers = Self::copilot_headers();
        headers.insert("Authorization".into(), format!("Bearer {refresh_token}"));

        let response = self
            .client
            .get_json(&urls.copilot_token, Some(&headers), cancellation)
            .await
            .map_err(AuthHttpError::into_auth_error)?;

        if !response.ok {
            return Err(AuthError::message(format!(
                "HTTP request failed. status={}; url={}; body={}",
                response.status, urls.copilot_token, response.raw_body
            )));
        }

        let token = response
            .body
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| AuthError::message("Invalid Copilot token response fields"))?;
        let expires_at = response
            .body
            .get("expires_at")
            .and_then(json_number_as_i64)
            .ok_or_else(|| AuthError::message("Invalid Copilot token response fields"))?;

        let mut extra = BTreeMap::new();
        if let Some(domain) = enterprise_domain {
            extra.insert(
                EXTRA_ENTERPRISE_URL.into(),
                Value::String(domain.to_owned()),
            );
        }

        Ok(OAuthCredential {
            refresh: refresh_token.to_owned(),
            access: token.to_owned(),
            expires: expires_at
                .saturating_mul(1000)
                .saturating_sub(EXPIRES_SKEW_MS),
            extra,
        })
    }

    async fn fetch_available_github_copilot_model_ids(
        &self,
        copilot_token: &str,
        enterprise_domain: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<String>, AuthError> {
        let base_url = get_github_copilot_base_url(Some(copilot_token), enterprise_domain);
        let url = self.rewrite_url(&format!("{base_url}/models"));

        let mut headers = Self::copilot_headers();
        headers.insert("Authorization".into(), format!("Bearer {copilot_token}"));
        headers.insert("X-GitHub-Api-Version".into(), COPILOT_API_VERSION.into());

        let response = self
            .client
            .get_json(&url, Some(&headers), cancellation)
            .await
            .map_err(AuthHttpError::into_auth_error)?;

        if !response.ok {
            return Err(AuthError::message(format!(
                "HTTP request failed. status={}; url={}; body={}",
                response.status, url, response.raw_body
            )));
        }

        // Prefer the parsed object body when it already contains `data`.
        // Otherwise re-parse the raw body so non-object envelopes still work.
        let raw = if response.body.get("data").is_some() {
            response.body.clone()
        } else {
            serde_json::from_str(&response.raw_body).unwrap_or_else(|_| response.body.clone())
        };
        parse_available_copilot_model_ids(&raw)
    }

    async fn enable_github_copilot_model(
        &self,
        token: &str,
        model_id: &str,
        enterprise_domain: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) -> bool {
        let base_url = get_github_copilot_base_url(Some(token), enterprise_domain);
        let url = self.rewrite_url(&format!("{base_url}/models/{model_id}/policy"));

        let mut headers = Self::copilot_headers();
        headers.insert("Authorization".into(), format!("Bearer {token}"));
        headers.insert("openai-intent".into(), "chat-policy".into());
        headers.insert("x-interaction-type".into(), "chat-policy".into());

        self.client
            .post_json(
                &url,
                &json!({ "state": "enabled" }),
                Some(&headers),
                cancellation,
            )
            .await
            .is_ok()
    }

    async fn enable_all_github_copilot_models(
        &self,
        token: &str,
        enterprise_domain: Option<&str>,
        cancellation: Option<&CancellationToken>,
    ) {
        let mut tasks = Vec::with_capacity(self.enable_model_ids.len());
        for model_id in self.enable_model_ids.iter().copied() {
            let this = self.clone();
            let token = token.to_owned();
            let enterprise = enterprise_domain.map(str::to_owned);
            let cancel = cancellation.cloned();
            tasks.push(async move {
                this.enable_github_copilot_model(
                    &token,
                    model_id,
                    enterprise.as_deref(),
                    cancel.as_ref(),
                )
                .await
            });
        }
        let _ = futures::future::join_all(tasks).await;
    }
}

impl Default for GitHubCopilotOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| {
            Self::with_client(AuthHttpClient::from_client(reqwest::Client::new()))
        })
    }
}

impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &'static str {
        "GitHub Copilot"
    }

    fn login_label(&self) -> Option<&'static str> {
        None
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let input = interaction
                .prompt(AuthPrompt::Text {
                    message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
                    placeholder: Some("company.ghe.com".into()),
                    signal: None,
                })
                .await?;

            if interaction
                .signal()
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(AuthError::Cancelled);
            }

            let trimmed = input.trim();
            let enterprise_domain = normalize_domain(input.as_str());
            if !trimmed.is_empty() && enterprise_domain.is_none() {
                return Err(AuthError::message("Invalid GitHub Enterprise URL/domain"));
            }
            let domain = enterprise_domain
                .clone()
                .unwrap_or_else(|| DEFAULT_PUBLIC_DOMAIN.to_owned());

            let device = self
                .start_device_flow(&domain, interaction.signal().as_ref())
                .await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri.clone(),
                interval_seconds: device.interval,
                expires_in_seconds: Some(device.expires_in),
            });

            let github_access_token = self
                .poll_for_github_access_token(&domain, &device, interaction.signal())
                .await?;

            let credentials = self
                .refresh_github_copilot_access_token(
                    &github_access_token,
                    enterprise_domain.as_deref(),
                    interaction.signal().as_ref(),
                )
                .await?;

            interaction.notify(AuthEvent::Progress {
                message: "Enabling models...".into(),
            });
            self.enable_all_github_copilot_models(
                &credentials.access,
                enterprise_domain.as_deref(),
                interaction.signal().as_ref(),
            )
            .await;

            let available = self
                .fetch_available_github_copilot_model_ids(
                    &credentials.access,
                    enterprise_domain.as_deref(),
                    interaction.signal().as_ref(),
                )
                .await?;

            Ok(with_available_model_ids(credentials, available))
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let enterprise = copilot_enterprise_domain(credential);
            let credentials = self
                .refresh_github_copilot_access_token(
                    &credential.refresh,
                    enterprise.as_deref(),
                    signal.as_ref(),
                )
                .await?;
            let available = self
                .fetch_available_github_copilot_model_ids(
                    &credentials.access,
                    enterprise.as_deref(),
                    signal.as_ref(),
                )
                .await?;
            Ok(with_available_model_ids(credentials, available))
        })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        Box::pin(async move {
            let enterprise = copilot_enterprise_domain(credential);
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: Some(get_github_copilot_base_url(
                    Some(credential.access.as_str()),
                    enterprise.as_deref(),
                )),
            })
        })
    }
}

#[derive(Clone, Debug)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
    expires_in: u64,
}

/// Normalize a user-entered enterprise URL/domain to a hostname.
#[must_use]
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = reqwest::Url::parse(&candidate).ok()?;
    let host = parsed.host_str()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_owned())
    }
}

/// Parse `proxy-ep` from a Copilot token into an API base URL.
///
/// Token format: `tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...`
/// → `https://api.individual.githubcopilot.com`.
#[must_use]
pub fn get_base_url_from_token(token: &str) -> Option<String> {
    get_base_url_from_token_for_provider(token, None)
}

fn get_base_url_from_token_for_provider(
    token: &str,
    enterprise_domain: Option<&str>,
) -> Option<String> {
    const KEY: &str = "proxy-ep=";
    let start = token.find(KEY)? + KEY.len();
    let rest = &token[start..];
    let end = rest.find(';').unwrap_or(rest.len());
    let proxy_host = rest[..end].trim();
    let parsed = reqwest::Url::parse(&format!("https://{proxy_host}")).ok()?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let proxy_host = parsed.host_str()?;
    if proxy_host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let api_suffix = proxy_host.strip_prefix("proxy.")?;
    let api_host = format!("api.{api_suffix}");

    let allowed = if let Some(enterprise_domain) = enterprise_domain {
        api_host.eq_ignore_ascii_case(&format!("api.{enterprise_domain}"))
    } else {
        api_host.ends_with(".githubcopilot.com")
    };
    allowed.then(|| format!("https://{api_host}"))
}

/// Resolve the Copilot API base URL from a token and optional enterprise domain.
#[must_use]
pub fn get_github_copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    let enterprise_domain = enterprise_domain.and_then(normalize_domain);
    if let Some(token) = token
        && let Some(url) =
            get_base_url_from_token_for_provider(token, enterprise_domain.as_deref())
    {
        return url;
    }
    if let Some(domain) = enterprise_domain {
        return format!("https://copilot-api.{domain}");
    }
    DEFAULT_COPILOT_BASE_URL.to_owned()
}

fn map_device_token_error(error: &str, body: &Value) -> OAuthDeviceCodePollResult<String> {
    if error == "authorization_pending" {
        return OAuthDeviceCodePollResult::Pending;
    }
    if error == "slow_down" {
        let interval_seconds = body
            .get("interval")
            .and_then(json_number_as_u64)
            .filter(|value| *value > 0);
        return OAuthDeviceCodePollResult::SlowDown { interval_seconds };
    }
    let description = body
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let description_suffix = if description.is_empty() {
        String::new()
    } else {
        format!(": {description}")
    };
    OAuthDeviceCodePollResult::Failed {
        message: format!("Device flow failed: {error}{description_suffix}"),
    }
}

fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    let enterprise_url = credential.extra.get(EXTRA_ENTERPRISE_URL)?.as_str()?;
    if enterprise_url.is_empty() {
        return None;
    }
    normalize_domain(enterprise_url)
}

fn with_available_model_ids(mut credentials: OAuthCredential, ids: Vec<String>) -> OAuthCredential {
    credentials.extra.insert(
        EXTRA_AVAILABLE_MODEL_IDS.into(),
        Value::Array(ids.into_iter().map(Value::String).collect()),
    );
    credentials
}

fn parse_device_code_response(body: &Value) -> Result<DeviceCodeResponse, AuthError> {
    if !body.is_object() {
        return Err(AuthError::message("Invalid device code response"));
    }

    let device_code = body
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::message("Invalid device code response fields"))?;
    let user_code = body
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::message("Invalid device code response fields"))?;
    let verification_uri = body
        .get("verification_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::message("Invalid device code response fields"))?;
    let interval = match body.get("interval") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            json_number_as_u64(value)
                .ok_or_else(|| AuthError::message("Invalid device code response fields"))?,
        ),
    };
    let expires_in = body
        .get("expires_in")
        .and_then(json_number_as_u64)
        .ok_or_else(|| AuthError::message("Invalid device code response fields"))?;

    let parsed_uri = reqwest::Url::parse(verification_uri)
        .map_err(|_| AuthError::message("Untrusted verification_uri in device code response"))?;
    if parsed_uri.scheme() != "https" && parsed_uri.scheme() != "http" {
        return Err(AuthError::message(
            "Untrusted verification_uri in device code response",
        ));
    }

    Ok(DeviceCodeResponse {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        verification_uri: parsed_uri.to_string(),
        interval,
        expires_in,
    })
}

fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn is_selectable_copilot_model(item: &serde_json::Map<String, Value>) -> bool {
    let policy_disabled = item
        .get("policy")
        .and_then(as_object)
        .and_then(|policy| policy.get("state"))
        .and_then(Value::as_str)
        == Some("disabled");
    let supports_tool_calls = item
        .get("capabilities")
        .and_then(as_object)
        .and_then(|capabilities| capabilities.get("supports"))
        .and_then(as_object)
        .and_then(|supports| supports.get("tool_calls"))
        .and_then(Value::as_bool);
    let picker_enabled = item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true);

    picker_enabled && !policy_disabled && supports_tool_calls != Some(false)
}

fn parse_available_copilot_model_ids(raw: &Value) -> Result<Vec<String>, AuthError> {
    let data = raw
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AuthError::message("Invalid Copilot models response"))?;

    let mut ids = Vec::new();
    for raw_item in data {
        let Some(item) = as_object(raw_item) else {
            continue;
        };
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if is_selectable_copilot_model(item) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

fn json_number_as_i64(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    // JSON numbers that do not fit native integers may still be whole floats.
    value
        .as_f64()
        .filter(|number| number.is_finite() && number.fract() == 0.0)
        .and_then(|number| number.to_string().parse::<i64>().ok())
}

fn json_number_as_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return u64::try_from(number).ok();
    }
    value
        .as_f64()
        .filter(|number| number.is_finite() && number.fract() == 0.0 && *number >= 0.0)
        .and_then(|number| number.to_string().parse::<u64>().ok())
}

#[cfg(test)]
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Path, Request, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::Response;
    use axum::routing::{get, post};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    fn expect_err<T, E>(result: Result<T, E>, label: &str) -> Result<E, String> {
        match result {
            Ok(_) => Err(err(label)),
            Err(error) => Ok(error),
        }
    }

    fn lock_mutex<'a, T>(
        mutex: &'a Mutex<T>,
        label: &'static str,
    ) -> Result<std::sync::MutexGuard<'a, T>, String> {
        mutex
            .lock()
            .map_err(|_| err(format!("{label} lock poisoned")))
    }

    #[derive(Clone, Default)]
    struct MockState {
        access_polls: Arc<AtomicUsize>,
        access_script: Arc<AsyncMutex<VecDeque<Value>>>,
        policy_hits: Arc<AtomicUsize>,
        last_device_headers: Arc<Mutex<HeaderMap>>,
        last_token_headers: Arc<Mutex<HeaderMap>>,
        last_models_headers: Arc<Mutex<HeaderMap>>,
        last_policy_headers: Arc<Mutex<HeaderMap>>,
        copilot_token: Arc<Mutex<String>>,
        expires_at: Arc<Mutex<i64>>,
        models_body: Arc<Mutex<Value>>,
    }

    struct MockInteraction {
        enterprise_input: String,
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<CancellationToken>,
    }

    impl AuthInteraction for MockInteraction {
        fn prompt(&self, _prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async { Ok(self.enterprise_input.clone()) })
        }

        fn notify(&self, event: AuthEvent) {
            if let Ok(mut events) = self.events.lock() {
                events.push(event);
            }
        }

        fn signal(&self) -> Option<CancellationToken> {
            self.signal.clone()
        }
    }

    fn json_response(status: StatusCode, body: &Value) -> Response {
        let raw = body.to_string();
        Response::builder()
            .status(status)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(raw))
            .unwrap_or_else(|_| Response::new(Body::empty()))
    }

    async fn device_code_handler(
        State(state): State<Arc<MockState>>,
        request: Request,
    ) -> Response {
        if let Ok(mut headers) = state.last_device_headers.lock() {
            *headers = request.headers().clone();
        }
        let body = request.into_body();
        let bytes = axum::body::to_bytes(body, 64 * 1024)
            .await
            .unwrap_or_default();
        let form = String::from_utf8_lossy(&bytes);
        if !(form.contains("client_id=")
            && (form.contains("scope=read%3Auser") || form.contains("scope=read:user")))
        {
            return json_response(StatusCode::BAD_REQUEST, &json!({"error":"bad form"}));
        }

        json_response(
            StatusCode::OK,
            &json!({
                "device_code": "device-abc",
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "interval": 1,
                "expires_in": 30
            }),
        )
    }

    async fn access_token_handler(
        State(state): State<Arc<MockState>>,
        request: Request,
    ) -> Response {
        state.access_polls.fetch_add(1, Ordering::SeqCst);
        let _ = request;
        let mut script = state.access_script.lock().await;
        let body = script
            .pop_front()
            .unwrap_or_else(|| json!({ "access_token": "ghu_test_token" }));
        json_response(StatusCode::OK, &body)
    }

    async fn copilot_token_handler(
        State(state): State<Arc<MockState>>,
        headers: HeaderMap,
    ) -> Response {
        if let Ok(mut last) = state.last_token_headers.lock() {
            *last = headers;
        }
        let token = state
            .copilot_token
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let expires_at = state.expires_at.lock().map_or(0, |guard| *guard);
        json_response(
            StatusCode::OK,
            &json!({
                "token": token,
                "expires_at": expires_at
            }),
        )
    }

    async fn models_handler(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
        if let Ok(mut last) = state.last_models_headers.lock() {
            *last = headers;
        }
        let body = state
            .models_body
            .lock()
            .map_or_else(|_| json!({"data": []}), |guard| guard.clone());
        json_response(StatusCode::OK, &body)
    }

    async fn policy_handler(
        State(state): State<Arc<MockState>>,
        Path(_id): Path<String>,
        headers: HeaderMap,
        _body: String,
    ) -> Response {
        state.policy_hits.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut last) = state.last_policy_headers.lock() {
            *last = headers;
        }
        json_response(StatusCode::OK, &json!({ "ok": true }))
    }

    async fn spawn_mock(state: Arc<MockState>) -> Result<String, String> {
        let app = Router::new()
            .route("/login/device/code", post(device_code_handler))
            .route("/login/oauth/access_token", post(access_token_handler))
            .route("/copilot_internal/v2/token", get(copilot_token_handler))
            .route("/models", get(models_handler))
            .route("/models/{id}/policy", post(policy_handler))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| err(e.to_string()))?;
        let addr = listener.local_addr().map_err(|e| err(e.to_string()))?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(format!("http://{addr}"))
    }

    fn selectable_model(id: &str) -> Value {
        json!({
            "id": id,
            "model_picker_enabled": true,
            "policy": { "state": "enabled" },
            "capabilities": { "supports": { "tool_calls": true } }
        })
    }

    fn default_models_body() -> Value {
        json!({
            "data": [
                selectable_model("gpt-4.1"),
                {
                    "id": "hidden",
                    "model_picker_enabled": false,
                    "policy": { "state": "enabled" },
                    "capabilities": { "supports": { "tool_calls": true } }
                },
                {
                    "id": "disabled",
                    "model_picker_enabled": true,
                    "policy": { "state": "disabled" },
                    "capabilities": { "supports": { "tool_calls": true } }
                },
                {
                    "id": "no-tools",
                    "model_picker_enabled": true,
                    "policy": { "state": "enabled" },
                    "capabilities": { "supports": { "tool_calls": false } }
                },
                selectable_model("claude-sonnet-4")
            ]
        })
    }

    fn copilot_token_with_proxy() -> String {
        "tid=1;exp=9;proxy-ep=proxy.individual.githubcopilot.com;sku=free".into()
    }

    fn oauth_flow(origin: &str) -> Result<GitHubCopilotOAuth, String> {
        Ok(
            GitHubCopilotOAuth::with_client(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_mock_origin(origin)
                .with_enable_model_ids(&["gpt-4.1", "claude-sonnet-4"]),
        )
    }

    #[test]
    fn client_id_matches_base64_constant() -> TestResult {
        let decoded = STANDARD
            .decode(CLIENT_ID_B64)
            .map_err(|e| err(e.to_string()))?
            .into_iter()
            .map(char::from)
            .collect::<String>();
        assert_eq!(decoded, CLIENT_ID);
        assert_eq!(CLIENT_ID, "Iv1.b507a08c87ecfe98");
        Ok(())
    }

    #[test]
    fn normalize_domain_accepts_host_and_url() {
        assert_eq!(
            normalize_domain("company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("https://company.ghe.com/path").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(normalize_domain("   "), None);
        assert_eq!(normalize_domain("://bad"), None);
    }

    #[test]
    fn base_url_from_token_and_fallbacks() {
        assert_eq!(
            get_base_url_from_token(&copilot_token_with_proxy()).as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        assert_eq!(
            get_base_url_from_token(
                "tid=1;proxy-ep=proxy.business.githubcopilot.com;sku=business"
            )
            .as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
        assert_eq!(
            get_github_copilot_base_url(None, Some("corp.ghe.com")),
            "https://copilot-api.corp.ghe.com"
        );
        assert_eq!(
            get_github_copilot_base_url(None, Some("https://corp.ghe.com/login")),
            "https://copilot-api.corp.ghe.com"
        );
        assert_eq!(
            get_github_copilot_base_url(None, None),
            DEFAULT_COPILOT_BASE_URL
        );
        assert_eq!(
            get_github_copilot_base_url(Some("no-proxy-ep"), Some("corp.ghe.com")),
            "https://copilot-api.corp.ghe.com"
        );
    }

    #[test]
    fn token_proxy_destination_is_bound_to_provider() {
        for untrusted in [
            "proxy.attacker.example",
            "proxy.169.254.169.254",
            "proxy.individual.githubcopilot.com:444",
            "user@proxy.individual.githubcopilot.com",
            "proxy.individual.githubcopilot.com/path",
        ] {
            let token = format!("tid=1;proxy-ep={untrusted};sku=free");
            assert_eq!(get_base_url_from_token(&token), None, "accepted {untrusted}");
            assert_eq!(
                get_github_copilot_base_url(Some(&token), None),
                DEFAULT_COPILOT_BASE_URL
            );
        }

        let enterprise = "tid=1;proxy-ep=proxy.corp.ghe.com;sku=enterprise";
        assert_eq!(
            get_github_copilot_base_url(Some(enterprise), Some("corp.ghe.com")),
            "https://api.corp.ghe.com"
        );
        assert_eq!(
            get_github_copilot_base_url(Some(enterprise), Some("other.ghe.com")),
            "https://copilot-api.other.ghe.com"
        );
    }

    #[test]
    fn parse_available_models_filters_policy_and_tools() -> TestResult {
        let ids = parse_available_copilot_model_ids(&default_models_body())
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            ids,
            vec!["gpt-4.1".to_owned(), "claude-sonnet-4".to_owned()]
        );
        Ok(())
    }

    fn assert_public_credential(credential: &OAuthCredential, expires_at: i64) -> TestResult {
        assert_eq!(credential.refresh, "ghu_public");
        assert_eq!(credential.access, copilot_token_with_proxy());
        assert!(!credential.extra.contains_key(EXTRA_ENTERPRISE_URL));
        let available = credential
            .extra
            .get(EXTRA_AVAILABLE_MODEL_IDS)
            .and_then(Value::as_array)
            .ok_or_else(|| err("availableModelIds"))?;
        assert_eq!(
            available,
            &vec![
                Value::String("gpt-4.1".into()),
                Value::String("claude-sonnet-4".into())
            ]
        );
        assert_eq!(
            credential.expires,
            expires_at
                .saturating_mul(1000)
                .saturating_sub(EXPIRES_SKEW_MS)
        );
        Ok(())
    }

    fn assert_interaction_events(events: &[AuthEvent]) {
        assert!(events.iter().any(|event| matches!(
            event,
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } if user_code == "ABCD-1234"
                && verification_uri == "https://github.com/login/device"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AuthEvent::Progress { message } if message == "Enabling models..."
        )));
    }

    fn assert_token_headers(token_headers: &HeaderMap, authorization: &str) {
        assert_eq!(
            token_headers
                .get("User-Agent")
                .and_then(|value| value.to_str().ok()),
            Some(USER_AGENT)
        );
        assert_eq!(
            token_headers
                .get("Editor-Version")
                .and_then(|value| value.to_str().ok()),
            Some(EDITOR_VERSION)
        );
        assert_eq!(
            token_headers
                .get("Editor-Plugin-Version")
                .and_then(|value| value.to_str().ok()),
            Some(EDITOR_PLUGIN_VERSION)
        );
        assert_eq!(
            token_headers
                .get("Copilot-Integration-Id")
                .and_then(|value| value.to_str().ok()),
            Some(COPILOT_INTEGRATION_ID)
        );
        assert_eq!(
            token_headers
                .get("Authorization")
                .and_then(|value| value.to_str().ok()),
            Some(authorization)
        );
    }

    #[tokio::test]
    async fn public_login_device_flow_refresh_and_to_auth() -> TestResult {
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::from([
                json!({ "error": "authorization_pending" }),
                json!({ "access_token": "ghu_public" }),
            ]))),
            copilot_token: Arc::new(Mutex::new(copilot_token_with_proxy())),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 3600)),
            models_body: Arc::new(Mutex::new(default_models_body())),
            ..MockState::default()
        });
        let origin = spawn_mock(state.clone()).await?;
        let flow = oauth_flow(&origin)?;
        let interaction = MockInteraction {
            enterprise_input: String::new(),
            events: Mutex::new(Vec::new()),
            signal: None,
        };

        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;

        let expires_at = *lock_mutex(&state.expires_at, "expires")?;
        assert_public_credential(&credential, expires_at)?;

        let events = lock_mutex(&interaction.events, "events")?.clone();
        assert_interaction_events(&events);

        let token_headers = lock_mutex(&state.last_token_headers, "headers")?.clone();
        assert_token_headers(&token_headers, "Bearer ghu_public");

        let models_headers = lock_mutex(&state.last_models_headers, "headers")?.clone();
        assert_eq!(
            models_headers
                .get("X-GitHub-Api-Version")
                .and_then(|value| value.to_str().ok()),
            Some(COPILOT_API_VERSION)
        );

        assert_eq!(state.policy_hits.load(Ordering::SeqCst), 2);

        let auth = flow
            .to_auth(&credential)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(auth.api_key.as_deref(), Some(credential.access.as_str()));
        assert_eq!(
            auth.base_url.as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
        Ok(())
    }

    #[tokio::test]
    async fn enterprise_login_stores_enterprise_url_and_base_url() -> TestResult {
        let enterprise_token = "tid=1;exp=9;proxy-ep=proxy.corp.ghe.com;sku=enterprise".to_owned();
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::from([json!({
                "access_token": "ghu_enterprise"
            })]))),
            copilot_token: Arc::new(Mutex::new(enterprise_token)),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 1800)),
            models_body: Arc::new(Mutex::new(json!({
                "data": [selectable_model("gpt-4.1")]
            }))),
            ..MockState::default()
        });
        let origin = spawn_mock(state).await?;
        let flow = oauth_flow(&origin)?;
        let interaction = MockInteraction {
            enterprise_input: "https://corp.ghe.com/login".into(),
            events: Mutex::new(Vec::new()),
            signal: None,
        };

        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            credential
                .extra
                .get(EXTRA_ENTERPRISE_URL)
                .and_then(Value::as_str),
            Some("corp.ghe.com")
        );

        let auth = flow
            .to_auth(&credential)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(auth.base_url.as_deref(), Some("https://api.corp.ghe.com"));

        // When the token has no proxy-ep, fall back to copilot-api.<enterprise>.
        let mut no_proxy = credential.clone();
        no_proxy.access = "plain-token".into();
        let auth = flow
            .to_auth(&no_proxy)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            auth.base_url.as_deref(),
            Some("https://copilot-api.corp.ghe.com")
        );
        Ok(())
    }

    #[tokio::test]
    async fn slow_down_then_complete() -> TestResult {
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::from([
                json!({ "error": "slow_down", "interval": 1 }),
                json!({ "access_token": "ghu_slow" }),
            ]))),
            copilot_token: Arc::new(Mutex::new(copilot_token_with_proxy())),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 3600)),
            models_body: Arc::new(Mutex::new(default_models_body())),
            ..MockState::default()
        });
        let origin = spawn_mock(state.clone()).await?;
        let flow = oauth_flow(&origin)?;
        let interaction = MockInteraction {
            enterprise_input: String::new(),
            events: Mutex::new(Vec::new()),
            signal: None,
        };

        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credential.refresh, "ghu_slow");
        assert!(state.access_polls.load(Ordering::SeqCst) >= 2);
        Ok(())
    }

    #[tokio::test]
    async fn refresh_rotates_copilot_token_and_model_ids() -> TestResult {
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::new())),
            copilot_token: Arc::new(Mutex::new(
                "tid=1;exp=9;proxy-ep=proxy.individual.githubcopilot.com;sku=pro".into(),
            )),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 7200)),
            models_body: Arc::new(Mutex::new(json!({
                "data": [selectable_model("gpt-5.4")]
            }))),
            ..MockState::default()
        });
        let origin = spawn_mock(state.clone()).await?;
        let flow = oauth_flow(&origin)?;

        let mut prior = OAuthCredential {
            refresh: "ghu_prior".into(),
            access: "old-copilot".into(),
            expires: 1,
            extra: BTreeMap::new(),
        };
        prior.extra.insert(
            EXTRA_ENTERPRISE_URL.into(),
            Value::String("corp.ghe.com".into()),
        );
        prior
            .extra
            .insert(EXTRA_AVAILABLE_MODEL_IDS.into(), json!(["stale-model"]));

        let refreshed = flow
            .refresh(&prior, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.refresh, "ghu_prior");
        assert_eq!(
            refreshed.access,
            "tid=1;exp=9;proxy-ep=proxy.individual.githubcopilot.com;sku=pro"
        );
        assert_eq!(
            refreshed
                .extra
                .get(EXTRA_ENTERPRISE_URL)
                .and_then(Value::as_str),
            Some("corp.ghe.com")
        );
        assert_eq!(
            refreshed.extra.get(EXTRA_AVAILABLE_MODEL_IDS),
            Some(&json!(["gpt-5.4"]))
        );

        let expires_at = *lock_mutex(&state.expires_at, "expires")?;
        assert_eq!(
            refreshed.expires,
            expires_at
                .saturating_mul(1000)
                .saturating_sub(EXPIRES_SKEW_MS)
        );

        let token_headers = lock_mutex(&state.last_token_headers, "headers")?.clone();
        assert_eq!(
            token_headers
                .get("Authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer ghu_prior")
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancel_during_device_poll_returns_cancelled() -> TestResult {
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::from([json!({
                "error": "authorization_pending"
            })]))),
            copilot_token: Arc::new(Mutex::new(copilot_token_with_proxy())),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 3600)),
            models_body: Arc::new(Mutex::new(default_models_body())),
            ..MockState::default()
        });
        let origin = spawn_mock(state).await?;
        let flow = oauth_flow(&origin)?;
        let cancel = CancellationToken::new();
        let interaction = MockInteraction {
            enterprise_input: String::new(),
            events: Mutex::new(Vec::new()),
            signal: Some(cancel.clone()),
        };

        let login = flow.login(&interaction);
        tokio::pin!(login);
        // Let the first wait-before-poll begin, then cancel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let err_value = expect_err(login.await, "cancelled")?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn device_error_surfaces_failed_message() -> TestResult {
        let state = Arc::new(MockState {
            access_script: Arc::new(AsyncMutex::new(VecDeque::from([json!({
                "error": "access_denied",
                "error_description": "user refused"
            })]))),
            copilot_token: Arc::new(Mutex::new(copilot_token_with_proxy())),
            expires_at: Arc::new(Mutex::new(now_epoch_secs() + 3600)),
            models_body: Arc::new(Mutex::new(default_models_body())),
            ..MockState::default()
        });
        let origin = spawn_mock(state).await?;
        let flow = oauth_flow(&origin)?;
        let interaction = MockInteraction {
            enterprise_input: String::new(),
            events: Mutex::new(Vec::new()),
            signal: None,
        };

        let err_value = expect_err(flow.login(&interaction).await, "failed")?;
        let AuthError::Message(message) = err_value else {
            return Err(err(format!("unexpected error: {err_value:?}")));
        };
        assert!(message.contains("Device flow failed: access_denied: user refused"));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_enterprise_domain_errors() -> TestResult {
        let flow =
            GitHubCopilotOAuth::with_client(AuthHttpClient::new().map_err(|e| err(e.to_string()))?);
        let interaction = MockInteraction {
            enterprise_input: "://not-a-domain".into(),
            events: Mutex::new(Vec::new()),
            signal: None,
        };
        let err_value = expect_err(flow.login(&interaction).await, "invalid domain")?;
        assert_eq!(
            err_value.to_string(),
            "Invalid GitHub Enterprise URL/domain"
        );
        Ok(())
    }

    #[tokio::test]
    async fn untrusted_verification_uri_is_rejected() -> TestResult {
        let err_value = expect_err(
            parse_device_code_response(&json!({
                "device_code": "x",
                "user_code": "y",
                "verification_uri": "file:///etc/passwd",
                "expires_in": 10
            })),
            "untrusted",
        )?;
        assert_eq!(
            err_value.to_string(),
            "Untrusted verification_uri in device code response"
        );
        Ok(())
    }
}
