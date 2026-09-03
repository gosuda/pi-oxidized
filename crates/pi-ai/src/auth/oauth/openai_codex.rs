//! `OpenAI` `Codex` (`ChatGPT` OAuth) login, refresh, and to-auth.
//!
//! Ports `.references/pi-2.0/packages/ai/src/auth/oauth/openai-codex.ts`:
//! browser `PKCE` + fixed loopback callback, device-code login, form-encoded
//! token exchange/refresh, JWT `accountId` extraction (no skew on expiry),
//! and soft-fail when port `1455` is already bound.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError};
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthSelectOption, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::callback_server::{OAuthCallbackConfig, OAuthCallbackServer};
use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::pkce::generate_pkce;

/// Fixed OAuth client id used by `Codex` / `ChatGPT` CLI flows.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Auth host for authorize/token/device endpoints.
pub const AUTH_BASE_URL: &str = "https://auth.openai.com";
/// Browser authorize endpoint.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// Token exchange / refresh endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Browser redirect URI (path must match the loopback callback).
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// Device-code user-code issuance endpoint.
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
/// Device-code token poll endpoint.
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// User-facing device verification URI.
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
/// Redirect URI used when exchanging a device-flow authorization code.
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
/// Device-flow total timeout (15 minutes).
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
/// Browser login method id.
pub const OPENAI_CODEX_BROWSER_LOGIN_METHOD: &str = "browser";
/// Device-code login method id.
pub const OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
/// OAuth scopes.
pub const SCOPE: &str = "openid profile email offline_access";
/// JWT claim path carrying `chatgpt_account_id`.
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// Loopback callback TCP port.
pub const CALLBACK_PORT: u16 = 1455;
/// Loopback callback path.
pub const CALLBACK_PATH: &str = "/auth/callback";
/// Credential extra key for the `ChatGPT` `accountId`.
pub const ACCOUNT_ID_EXTRA_KEY: &str = "accountId";

const ORIGINATOR: &str = "pi";
const OAUTH_DISPLAY_NAME: &str = "OpenAI (ChatGPT Plus/Pro)";
/// Largest integer exactly representable in `f64` (2^53 − 1).
const MAX_EXACT_F64_INT: f64 = 9_007_199_254_740_991.0;

/// Injectable endpoint table so unit tests can point at loopback mocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCodexEndpoints {
    /// Browser authorize URL.
    pub authorize_url: String,
    /// Token exchange / refresh URL.
    pub token_url: String,
    /// Device user-code URL.
    pub device_user_code_url: String,
    /// Device token poll URL.
    pub device_token_url: String,
    /// Device verification URI shown to the user.
    pub device_verification_uri: String,
    /// Browser redirect URI.
    pub redirect_uri: String,
    /// Device-flow redirect URI used at code exchange.
    pub device_redirect_uri: String,
    /// Loopback callback port.
    pub callback_port: u16,
    /// Loopback callback path.
    pub callback_path: String,
}

impl Default for OpenAiCodexEndpoints {
    fn default() -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            device_user_code_url: DEVICE_USER_CODE_URL.to_owned(),
            device_token_url: DEVICE_TOKEN_URL.to_owned(),
            device_verification_uri: DEVICE_VERIFICATION_URI.to_owned(),
            redirect_uri: REDIRECT_URI.to_owned(),
            device_redirect_uri: DEVICE_REDIRECT_URI.to_owned(),
            callback_port: CALLBACK_PORT,
            callback_path: CALLBACK_PATH.to_owned(),
        }
    }
}

/// `OpenAI` `Codex` OAuth provider.
#[derive(Clone, Debug)]
pub struct OpenAiCodexOAuth {
    http: AuthHttpClient,
    endpoints: OpenAiCodexEndpoints,
}

/// Outcome of racing the loopback callback against manual paste.
enum BrowserRaceOutcome {
    Callback(String),
    Manual(String),
    ManualError(AuthError),
    Cancelled,
    Empty,
}

impl OpenAiCodexOAuth {
    /// Build with a default HTTP client and production endpoints.
    ///
    /// # Errors
    ///
    /// Returns an auth error if the HTTP client cannot be constructed.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            endpoints: OpenAiCodexEndpoints::default(),
        })
    }

    /// Build with an injected HTTP client (tests / shared client pools).
    #[must_use]
    pub fn with_http(http: AuthHttpClient) -> Self {
        Self {
            http,
            endpoints: OpenAiCodexEndpoints::default(),
        }
    }

    /// Override endpoints (mock servers in tests).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: OpenAiCodexEndpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Production singleton-style constructor as `Arc<dyn OAuthAuth>`.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::new`] failures.
    pub fn shared() -> Result<Arc<dyn OAuthAuth>, AuthError> {
        Ok(Arc::new(Self::new()?))
    }

    async fn login_browser(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, AuthError> {
        let flow = create_authorization_flow(&self.endpoints, ORIGINATOR)?;
        let server = OAuthCallbackServer::start_soft(OAuthCallbackConfig {
            port: self.endpoints.callback_port,
            path: self.endpoints.callback_path.clone(),
            expected_state: flow.state.clone(),
            success_message: "OpenAI authentication completed. You can close this window."
                .to_owned(),
            host: None,
        })
        .await;

        let manual_abort = interaction
            .signal()
            .map_or_else(CancellationToken::new, |parent| parent.child_token());

        interaction.notify(AuthEvent::AuthUrl {
            url: flow.url.clone(),
            instructions: Some("A browser window should open. Complete login to finish.".into()),
        });

        let outcome = race_callback_and_manual_prompt(
            &server,
            interaction,
            &self.endpoints.redirect_uri,
            manual_abort.clone(),
        )
        .await;

        manual_abort.cancel();
        server.close().await;

        let code = authorization_code_from_race(outcome, &flow.state)?;
        self.exchange_authorization_code_for_credentials(
            &code,
            &flow.verifier,
            &self.endpoints.redirect_uri,
            interaction.signal(),
        )
        .await
    }

    async fn login_device_code(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, AuthError> {
        let device = self
            .start_device_auth(interaction.signal().as_ref())
            .await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: self.endpoints.device_verification_uri.clone(),
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
        });
        let code = self.poll_device_auth(&device, interaction.signal()).await?;
        self.exchange_authorization_code_for_credentials(
            &code.authorization_code,
            &code.code_verifier,
            &self.endpoints.device_redirect_uri,
            interaction.signal(),
        )
        .await
    }

    async fn start_device_auth(
        &self,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DeviceAuthInfo, AuthError> {
        let body = json!({ "client_id": CLIENT_ID });
        let raw = match self
            .http
            .post_json(
                &self.endpoints.device_user_code_url,
                &body,
                None,
                cancellation,
            )
            .await
        {
            Ok(raw) => raw,
            Err(AuthHttpError::Http { status: 404, .. }) => {
                return Err(AuthError::message(
                    "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
                ));
            }
            Err(AuthHttpError::Http { status, body, .. }) => {
                return Err(AuthError::message(format!(
                    "OpenAI Codex device code request failed with status {status}{}",
                    if body.is_empty() {
                        String::new()
                    } else {
                        format!(": {body}")
                    }
                )));
            }
            Err(error) => return Err(error.into_auth_error()),
        };

        let value: Value = serde_json::from_str(&raw).map_err(|_| {
            AuthError::message(format!("Invalid OpenAI Codex device code response: {raw}"))
        })?;
        let device_auth_id = value
            .get("device_auth_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let user_code = value
            .get("user_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let interval_seconds = parse_interval(value.get("interval"));
        match (device_auth_id, user_code, interval_seconds) {
            (Some(device_auth_id), Some(user_code), Some(interval_seconds)) => Ok(DeviceAuthInfo {
                device_auth_id: device_auth_id.to_owned(),
                user_code: user_code.to_owned(),
                interval_seconds,
            }),
            _ => Err(AuthError::message(format!(
                "Invalid OpenAI Codex device code response: {value}"
            ))),
        }
    }

    async fn poll_device_auth(
        &self,
        device: &DeviceAuthInfo,
        signal: Option<CancellationToken>,
    ) -> Result<DeviceTokenSuccess, AuthError> {
        let http = self.http.clone();
        let device_token_url = self.endpoints.device_token_url.clone();
        let device_auth_id = device.device_auth_id.clone();
        let user_code = device.user_code.clone();
        let poll_signal = signal.clone();

        let mut options = OAuthDeviceCodePollOptions::new(move || {
            let http = http.clone();
            let device_token_url = device_token_url.clone();
            let device_auth_id = device_auth_id.clone();
            let user_code = user_code.clone();
            let poll_signal = poll_signal.clone();
            async move {
                let body = json!({
                    "device_auth_id": device_auth_id,
                    "user_code": user_code,
                });
                match http
                    .post_json(&device_token_url, &body, None, poll_signal.as_ref())
                    .await
                {
                    Ok(raw) => {
                        let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                        let authorization_code = value
                            .get("authorization_code")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());
                        let code_verifier = value
                            .get("code_verifier")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());
                        match (authorization_code, code_verifier) {
                            (Some(authorization_code), Some(code_verifier)) => {
                                Ok(OAuthDeviceCodePollResult::Complete {
                                    value: DeviceTokenSuccess {
                                        authorization_code: authorization_code.to_owned(),
                                        code_verifier: code_verifier.to_owned(),
                                    },
                                })
                            }
                            _ => Ok(OAuthDeviceCodePollResult::Failed {
                                message: format!(
                                    "Invalid OpenAI Codex device auth token response: {value}"
                                ),
                            }),
                        }
                    }
                    Err(AuthHttpError::Http {
                        status: 403 | 404, ..
                    }) => Ok(OAuthDeviceCodePollResult::Pending),
                    Err(AuthHttpError::Http { status, body, .. }) => {
                        let error_code = extract_device_error_code(&body);
                        if error_code.as_deref() == Some("deviceauth_authorization_pending") {
                            return Ok(OAuthDeviceCodePollResult::Pending);
                        }
                        if error_code.as_deref() == Some("slow_down") {
                            return Ok(OAuthDeviceCodePollResult::SlowDown {
                                interval_seconds: None,
                            });
                        }
                        Ok(OAuthDeviceCodePollResult::Failed {
                            message: format!(
                                "OpenAI Codex device auth failed with status {status}{}",
                                if body.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {body}")
                                }
                            ),
                        })
                    }
                    Err(AuthHttpError::Cancelled) => Err(AuthError::Cancelled),
                    Err(error) => Err(error.into_auth_error()),
                }
            }
        });
        options.interval_seconds = Some(device.interval_seconds);
        options.expires_in_seconds = Some(DEVICE_CODE_TIMEOUT_SECONDS);
        options.wait_before_first_poll = false;
        options.signal = signal;
        poll_oauth_device_code_flow(options).await
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthToken, AuthError> {
        let mut fields = BTreeMap::new();
        fields.insert("grant_type".into(), "authorization_code".into());
        fields.insert("client_id".into(), CLIENT_ID.to_owned());
        fields.insert("code".into(), code.to_owned());
        fields.insert("code_verifier".into(), verifier.to_owned());
        fields.insert("redirect_uri".into(), redirect_uri.to_owned());

        let response = self
            .http
            .post_form(&self.endpoints.token_url, &fields, None, signal.as_ref())
            .await
            .map_err(map_token_http_error)?;
        read_token_response(&response, TokenOperation::Exchange)
    }

    async fn refresh_access_token(
        &self,
        refresh_token: &str,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthToken, AuthError> {
        let mut fields = BTreeMap::new();
        fields.insert("grant_type".into(), "refresh_token".into());
        fields.insert("refresh_token".into(), refresh_token.to_owned());
        fields.insert("client_id".into(), CLIENT_ID.to_owned());

        let response = match self
            .http
            .post_form(&self.endpoints.token_url, &fields, None, signal.as_ref())
            .await
        {
            Ok(response) => response,
            Err(AuthHttpError::Cancelled) => return Err(AuthError::Cancelled),
            Err(error) => {
                return Err(AuthError::message(format!(
                    "OpenAI Codex token refresh error: {error}"
                )));
            }
        };
        read_token_response(&response, TokenOperation::Refresh)
    }

    async fn exchange_authorization_code_for_credentials(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let token = self
            .exchange_authorization_code(code, verifier, redirect_uri, signal)
            .await?;
        credentials_from_token(token)
    }
}

impl Default for OpenAiCodexOAuth {
    fn default() -> Self {
        Self::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
    }
}

impl OAuthAuth for OpenAiCodexOAuth {
    fn name(&self) -> &'static str {
        OAUTH_DISPLAY_NAME
    }

    fn login_label(&self) -> Option<&str> {
        Some(OAUTH_DISPLAY_NAME)
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let method = interaction
                .prompt(AuthPrompt::Select {
                    message: "Select OpenAI Codex login method:".into(),
                    options: vec![
                        AuthSelectOption {
                            id: OPENAI_CODEX_BROWSER_LOGIN_METHOD.into(),
                            label: "Browser login (default)".into(),
                            description: None,
                        },
                        AuthSelectOption {
                            id: OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.into(),
                            label: "Device code login (headless)".into(),
                            description: None,
                        },
                    ],
                    signal: None,
                })
                .await?;

            if method == OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD {
                return self.login_device_code(interaction).await;
            }
            if method != OPENAI_CODEX_BROWSER_LOGIN_METHOD {
                return Err(AuthError::message(format!(
                    "Unknown OpenAI Codex login method: {method}"
                )));
            }
            self.login_browser(interaction).await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let token = self
                .refresh_access_token(&credential.refresh, signal)
                .await?;
            // Preserve non-account extras; accountId is always re-derived from the
            // access token so a rotated JWT cannot keep a stale account.
            let mut next = credentials_from_token(token)?;
            for (key, value) in &credential.extra {
                if key != ACCOUNT_ID_EXTRA_KEY {
                    next.extra
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
            Ok(next)
        })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        Box::pin(async move {
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: None,
            })
        })
    }
}

#[derive(Clone, Debug)]
struct AuthorizationFlow {
    verifier: String,
    state: String,
    url: String,
}

#[derive(Clone, Debug)]
struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: u64,
}

#[derive(Clone, Debug)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Clone, Debug)]
struct OAuthToken {
    access: String,
    refresh: String,
    expires: i64,
}

#[derive(Clone, Copy, Debug)]
enum TokenOperation {
    Exchange,
    Refresh,
}

impl TokenOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exchange => "exchange",
            Self::Refresh => "refresh",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ParsedAuthorizationInput {
    code: Option<String>,
    state: Option<String>,
}

async fn race_callback_and_manual_prompt(
    server: &OAuthCallbackServer,
    interaction: &dyn AuthInteraction,
    redirect_uri: &str,
    manual_abort: CancellationToken,
) -> BrowserRaceOutcome {
    let prompt = interaction.prompt(AuthPrompt::ManualCode {
        message:
            "Complete login in your browser, or paste the authorization code / redirect URL here:"
                .into(),
        placeholder: Some(redirect_uri.to_owned()),
        signal: Some(manual_abort.clone()),
    });
    tokio::pin!(prompt);

    let wait = server.wait_for_code();
    tokio::pin!(wait);

    let cancel = async {
        if let Some(signal) = interaction.signal() {
            signal.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(cancel);

    let mut waiting = true;
    let mut prompting = true;
    let mut result = BrowserRaceOutcome::Empty;

    while waiting || prompting {
        tokio::select! {
            biased;
            received = wait.as_mut(), if waiting => {
                waiting = false;
                if let Some(code) = received {
                    manual_abort.cancel();
                    result = BrowserRaceOutcome::Callback(code.code);
                    break;
                }
                // Soft-fail / cancelWait: keep waiting for manual.
                if !prompting {
                    break;
                }
            }
            manual = prompt.as_mut(), if prompting => {
                prompting = false;
                match manual {
                    Ok(input) => {
                        server.cancel_wait().await;
                        if matches!(result, BrowserRaceOutcome::Empty) {
                            result = BrowserRaceOutcome::Manual(input);
                        }
                        if !waiting {
                            break;
                        }
                    }
                    Err(AuthError::Cancelled) => {
                        server.cancel_wait().await;
                        if interaction
                            .signal()
                            .as_ref()
                            .is_some_and(CancellationToken::is_cancelled)
                        {
                            result = BrowserRaceOutcome::Cancelled;
                            break;
                        }
                        if !waiting {
                            break;
                        }
                    }
                    Err(error) => {
                        server.cancel_wait().await;
                        if matches!(result, BrowserRaceOutcome::Empty) {
                            result = BrowserRaceOutcome::ManualError(error);
                        }
                        if !waiting {
                            break;
                        }
                    }
                }
            }
            () = cancel.as_mut() => {
                manual_abort.cancel();
                server.cancel_wait().await;
                result = BrowserRaceOutcome::Cancelled;
                break;
            }
        }
    }
    result
}

fn authorization_code_from_race(
    outcome: BrowserRaceOutcome,
    expected_state: &str,
) -> Result<String, AuthError> {
    match outcome {
        BrowserRaceOutcome::Callback(code) => Ok(code),
        BrowserRaceOutcome::Manual(input) => {
            let parsed = parse_authorization_input(&input);
            if parsed
                .state
                .as_deref()
                .is_some_and(|state| state != expected_state)
            {
                return Err(AuthError::message("State mismatch"));
            }
            parsed
                .code
                .filter(|value| !value.is_empty())
                .ok_or_else(|| AuthError::message("Missing authorization code"))
        }
        BrowserRaceOutcome::ManualError(error) => Err(error),
        BrowserRaceOutcome::Cancelled => Err(AuthError::Cancelled),
        BrowserRaceOutcome::Empty => Err(AuthError::message("Missing authorization code")),
    }
}

fn create_authorization_flow(
    endpoints: &OpenAiCodexEndpoints,
    originator: &str,
) -> Result<AuthorizationFlow, AuthError> {
    let pkce = generate_pkce()?;
    let state = create_state()?;
    let mut url = reqwest::Url::parse(&endpoints.authorize_url)
        .map_err(|error| AuthError::message(format!("invalid authorize URL: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", CLIENT_ID);
        query.append_pair("redirect_uri", &endpoints.redirect_uri);
        query.append_pair("scope", SCOPE);
        query.append_pair("code_challenge", &pkce.challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", &state);
        query.append_pair("id_token_add_organizations", "true");
        query.append_pair("codex_cli_simplified_flow", "true");
        query.append_pair("originator", originator);
    }
    Ok(AuthorizationFlow {
        verifier: pkce.verifier,
        state,
        url: url.to_string(),
    })
}

/// 16 random bytes as lowercase hex — matches Node `randomBytes(16).toString("hex")`.
fn create_state() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AuthError::message(format!("failed to generate OAuth state: {error}")))?;
    let mut state = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(state, "{byte:02x}");
    }
    Ok(state)
}

fn parse_authorization_input(input: &str) -> ParsedAuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuthorizationInput::default();
    }

    if let Ok(url) = reqwest::Url::parse(value) {
        let mut code = None;
        let mut state = None;
        for (key, val) in url.query_pairs() {
            if key == "code" && code.is_none() {
                code = Some(val.into_owned());
            } else if key == "state" && state.is_none() {
                state = Some(val.into_owned());
            }
        }
        return ParsedAuthorizationInput { code, state };
    }

    if value.contains('#') {
        let mut parts = value.splitn(2, '#');
        let code = parts.next().map(str::to_owned);
        let state = parts.next().map(str::to_owned);
        return ParsedAuthorizationInput { code, state };
    }

    if value.contains("code=") {
        let mut code = None;
        let mut state = None;
        for pair in value.split('&') {
            let mut kv = pair.splitn(2, '=');
            let key = kv.next().unwrap_or("");
            let val = kv.next().unwrap_or("");
            if key == "code" && code.is_none() {
                code = Some(val.to_owned());
            } else if key == "state" && state.is_none() {
                state = Some(val.to_owned());
            }
        }
        return ParsedAuthorizationInput { code, state };
    }

    ParsedAuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

/// Decode a JWT payload without validating the signature.
///
/// Returns `None` on structural/base64/JSON failure. Never embeds the token in
/// errors — callers map `None` to a fixed message.
fn decode_jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = decode_base64url(payload)?;
    let text = String::from_utf8(decoded).ok()?;
    serde_json::from_str(&text).ok()
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(input) {
        return Some(bytes);
    }
    // Tolerate padded base64url (some issuers include `=`).
    let mut padded = input.to_owned();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    base64::engine::general_purpose::URL_SAFE
        .decode(padded)
        .ok()
}

fn get_account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    let account_id = payload
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()?;
    if account_id.is_empty() {
        None
    } else {
        Some(account_id.to_owned())
    }
}

fn credentials_from_token(token: OAuthToken) -> Result<OAuthCredential, AuthError> {
    let account_id = get_account_id(&token.access)
        .ok_or_else(|| AuthError::message("Failed to extract accountId from token"))?;
    let mut extra = BTreeMap::new();
    extra.insert(ACCOUNT_ID_EXTRA_KEY.to_owned(), Value::String(account_id));
    Ok(OAuthCredential {
        refresh: token.refresh,
        access: token.access,
        expires: token.expires,
        extra,
    })
}

fn read_token_response(
    response: &super::super::http::AuthHttpResponse,
    operation: TokenOperation,
) -> Result<OAuthToken, AuthError> {
    if !response.ok {
        return Err(AuthError::message(format!(
            "OpenAI Codex token {} failed ({})",
            operation.as_str(),
            response.status,
        )));
    }

    let access = response
        .body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let refresh = response
        .body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let expires_in = response.body.get("expires_in").and_then(Value::as_f64);

    let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
        let mut fields = Vec::with_capacity(3);
        if access.is_none() {
            fields.push("access_token");
        }
        if refresh.is_none() {
            fields.push("refresh_token");
        }
        if expires_in.is_none() {
            fields.push("expires_in");
        }
        return Err(AuthError::message(format!(
            "OpenAI Codex token {} response missing or invalid fields: {}",
            operation.as_str(),
            fields.join(", "),
        )));
    };
    let expires_ms = now_epoch_ms()?.saturating_add(seconds_f64_to_millis(expires_in)?);
    Ok(OAuthToken {
        access: access.to_owned(),
        refresh: refresh.to_owned(),
        expires: expires_ms,
    })
}

fn now_epoch_ms() -> Result<i64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .map_err(|error| AuthError::message(format!("system clock error: {error}")))
}

/// Convert a finite non-negative `expires_in` seconds value to milliseconds.
fn seconds_f64_to_millis(seconds: f64) -> Result<i64, AuthError> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(AuthError::message(
            "OpenAI Codex token response has invalid expires_in",
        ));
    }
    // Cap so the f64 is still exact as an integer.
    if seconds > MAX_EXACT_F64_INT {
        return Err(AuthError::message(
            "OpenAI Codex token response expires_in overflows",
        ));
    }
    let millis = seconds * 1000.0;
    if !millis.is_finite() {
        return Err(AuthError::message(
            "OpenAI Codex token response expires_in overflows",
        ));
    }
    millis
        .trunc()
        .to_string()
        .parse::<u64>()
        .ok()
        .and_then(|ms| i64::try_from(ms).ok())
        .ok_or_else(|| AuthError::message("OpenAI Codex token response expires_in overflows"))
}

fn parse_interval(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().and_then(finite_nonneg_f64_to_u64)),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            trimmed.parse::<u64>().ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .and_then(finite_nonneg_f64_to_u64)
            })
        }
        _ => None,
    }
}

fn finite_nonneg_f64_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    if value > MAX_EXACT_F64_INT {
        return None;
    }
    value.trunc().to_string().parse::<u64>().ok()
}

fn extract_device_error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("error") {
        Some(Value::String(code)) => Some(code.clone()),
        Some(Value::Object(map)) => map.get("code").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn map_token_http_error(error: AuthHttpError) -> AuthError {
    match error {
        AuthHttpError::Cancelled => AuthError::Cancelled,
        other => other.into_auth_error(),
    }
}

/// Shared production instance helper used by the auth registry once wired.
///
/// # Errors
///
/// Returns an auth error if the default HTTP client cannot be built.
pub fn openai_codex_oauth() -> Result<OpenAiCodexOAuth, AuthError> {
    OpenAiCodexOAuth::new()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::auth::oauth::pkce::s256_challenge;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;

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

    struct ScriptedInteraction {
        prompts: Mutex<Vec<Result<String, AuthError>>>,
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<CancellationToken>,
    }

    impl ScriptedInteraction {
        fn new(prompts: Vec<Result<String, AuthError>>) -> Self {
            Self {
                prompts: Mutex::new(prompts),
                events: Mutex::new(Vec::new()),
                signal: None,
            }
        }
    }

    impl AuthInteraction for ScriptedInteraction {
        fn prompt(&self, _prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                let mut guard = self
                    .prompts
                    .lock()
                    .map_err(|_| AuthError::message("prompts lock poisoned"))?;
                if guard.is_empty() {
                    return Err(AuthError::message("unexpected prompt"));
                }
                guard.remove(0)
            })
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

    struct HangManual {
        select_done: AtomicUsize,
        events: Mutex<Vec<AuthEvent>>,
        hang: CancellationToken,
        prompt_signal: Mutex<Option<CancellationToken>>,
        signal: Option<CancellationToken>,
    }

    impl AuthInteraction for HangManual {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                match prompt {
                    AuthPrompt::Select { .. } => {
                        self.select_done.fetch_add(1, Ordering::SeqCst);
                        Ok(OPENAI_CODEX_BROWSER_LOGIN_METHOD.into())
                    }
                    AuthPrompt::ManualCode { signal, .. } => {
                        if let Some(signal) = &signal
                            && let Ok(mut prompt_signal) = self.prompt_signal.lock()
                        {
                            *prompt_signal = Some(signal.clone());
                        }
                        let cancel = signal.unwrap_or_else(|| self.hang.clone());
                        cancel.cancelled().await;
                        Err(AuthError::Cancelled)
                    }
                    _ => Err(AuthError::message("unexpected")),
                }
            })
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

    async fn hit_callback_when_ready(interaction: &HangManual, port: u16) -> TestResult {
        let url = loop {
            if interaction.select_done.load(Ordering::SeqCst) > 0 {
                let events = interaction
                    .events
                    .lock()
                    .map_err(|_| err("events lock"))?
                    .clone();
                if let Some(url) = events.iter().find_map(|event| match event {
                    AuthEvent::AuthUrl { url, .. } => Some(url.clone()),
                    _ => None,
                }) {
                    break url;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let parsed = reqwest::Url::parse(&url).map_err(|e| err(e.to_string()))?;
        let state = parsed
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| err("missing state"))?;
        let callback =
            format!("http://127.0.0.1:{port}{CALLBACK_PATH}?code=from-browser&state={state}");
        let response = reqwest::Client::new()
            .get(callback)
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(err(format!("callback status {}", response.status())))
        }
    }

    /// Render one HTTP/1.1 stub reply with an exact `Content-Length`.
    fn http_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
    /// Drain one HTTP request from a test stub connection before replying.
    /// The stub answers from the request line, so it must read the whole
    /// head first; a single `read` can return a partial segment. The body
    /// is drained by declared length: closing with unread bytes queued
    /// makes Linux send RST instead of FIN, which can race the client.
    fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut buf = [0_u8; 4096];
        for _ in 0..16 {
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                _ => break,
            }
        }
        if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = pos + 4;
            let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while raw.len() < header_end + content_length {
                match stream.read(&mut buf) {
                    Ok(n) if n > 0 => raw.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
        }
        raw
    }
    fn spawn_device_user_code_server() -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            // Same sweep guard as the token stub: only a real device-code
            // request draws the single scripted reply.
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                if !String::from_utf8_lossy(&req).starts_with("POST /usercode ") {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let body = json!({
                    "device_auth_id": "dev-1",
                    "user_code": "ABCD-1234",
                    "interval": 0,
                })
                .to_string();
                let _ = stream.write_all(http_response("200 OK", &body).as_slice());
                return;
            }
        });
        Ok(format!("http://{address}/usercode"))
    }

    fn spawn_device_token_server() -> Result<(String, Arc<AtomicUsize>), String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        let hits = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&hits);
        thread::spawn(move || {
            // Answer pending once, then success, for real device polls only.
            // Alien traffic (localhost sweeps send `GET /`) gets a 404
            // without consuming the script, so a sweep cannot starve the
            // flow and fail the suite with a refused connection.
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                let text = String::from_utf8_lossy(&req);
                if !text.starts_with("POST /device-token ") {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let attempt = served.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt == 0 {
                    (
                        "403 Forbidden",
                        json!({"error":"deviceauth_authorization_pending"}).to_string(),
                    )
                } else {
                    (
                        "200 OK",
                        json!({
                            "authorization_code": "dev-code",
                            "code_verifier": "dev-verifier",
                        })
                        .to_string(),
                    )
                };
                let _ = stream.write_all(http_response(status, &body).as_slice());
                if attempt > 0 {
                    return;
                }
            }
        });
        Ok((format!("http://{address}/device-token"), hits))
    }

    fn spawn_hanging_token_server(
        path: &'static str,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<()>), String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let (request_seen, request_started) = tokio::sync::oneshot::channel();
        thread::spawn(move || {
            // Sweep guard: a `GET /` must not consume the single accept and
            // starve the real device poll of its hang point.
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                if !String::from_utf8_lossy(&req).starts_with(&format!("POST {path} ")) {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                let _ = request_seen.send(());
                let mut remaining = [0_u8; 1024];
                while stream.read(&mut remaining).is_ok_and(|size| size > 0) {}
                return;
            }
        });
        Ok((format!("http://{address}{path}"), request_started))
    }

    fn make_jwt(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            json!({
                JWT_CLAIM_PATH: { "chatgpt_account_id": account_id },
                "sub": "user"
            })
            .to_string()
            .as_bytes(),
        );
        format!("{header}.{payload}.sig")
    }

    fn spawn_form_token_server(
        expected_grant: &'static str,
        access: &str,
        refresh: &str,
        expires_in: u64,
        capture: Arc<AsyncMutex<Option<String>>>,
    ) -> Result<String, String> {
        let access = access.to_owned();
        let refresh = refresh.to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            // Sweep guard: answer only the expected grant POST; anything
            // else gets a 404 so a sweep cannot consume the single accept.
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let raw = read_http_request(&mut stream);
                let request = String::from_utf8_lossy(&raw).into_owned();
                let first = request.lines().next().unwrap_or("").to_owned();
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
                let lowered = request.to_ascii_lowercase();
                let is_grant_post = first.starts_with("POST /token ")
                    && lowered.contains("content-type: application/x-www-form-urlencoded")
                    && (body.contains(&format!("grant_type={expected_grant}"))
                        || body.contains(&format!(
                            "grant_type={}",
                            expected_grant.replace('_', "%5F")
                        )));
                if !is_grant_post {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                {
                    let mut guard = capture.blocking_lock();
                    *guard = Some(body);
                }
                let response_body = json!({
                    "access_token": access,
                    "refresh_token": refresh,
                    "expires_in": expires_in,
                })
                .to_string();
                let _ = stream.write_all(http_response("200 OK", &response_body).as_slice());
                return;
            }
        });
        Ok(format!("http://{address}/token"))
    }

    #[test]
    fn parse_authorization_input_accepts_url_hash_query_and_bare() {
        let url =
            parse_authorization_input("http://localhost:1455/auth/callback?code=abc&state=xyz");
        assert_eq!(url.code.as_deref(), Some("abc"));
        assert_eq!(url.state.as_deref(), Some("xyz"));

        let hash = parse_authorization_input("code123#state456");
        assert_eq!(hash.code.as_deref(), Some("code123"));
        assert_eq!(hash.state.as_deref(), Some("state456"));

        let query = parse_authorization_input("code=c1&state=s1");
        assert_eq!(query.code.as_deref(), Some("c1"));
        assert_eq!(query.state.as_deref(), Some("s1"));

        let bare = parse_authorization_input("only-code");
        assert_eq!(bare.code.as_deref(), Some("only-code"));
        assert!(bare.state.is_none());
    }

    #[test]
    fn jwt_account_id_extracts_and_rejects_malformed_without_token_leak() -> TestResult {
        let good = make_jwt("acct-1");
        assert_eq!(get_account_id(&good).as_deref(), Some("acct-1"));

        let malformed = "not-a-jwt";
        assert!(get_account_id(malformed).is_none());
        let message = match credentials_from_token(OAuthToken {
            access: malformed.into(),
            refresh: "r".into(),
            expires: 1,
        }) {
            Ok(_) => return Err(err("expected malformed JWT failure")),
            Err(error) => error.to_string(),
        };
        assert_eq!(message, "Failed to extract accountId from token");
        assert!(!message.contains(malformed));

        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"x"}"#);
        let missing_claim = format!("{header}.{payload}.sig");
        let message = match credentials_from_token(OAuthToken {
            access: missing_claim.clone(),
            refresh: "r".into(),
            expires: 1,
        }) {
            Ok(_) => return Err(err("expected missing claim failure")),
            Err(error) => error.to_string(),
        };
        assert_eq!(message, "Failed to extract accountId from token");
        assert!(!message.contains(&missing_claim));
        Ok(())
    }

    #[test]
    fn credentials_extra_roundtrip_preserves_account_id() -> TestResult {
        let token = OAuthToken {
            access: make_jwt("acct-round"),
            refresh: "refresh-1".into(),
            expires: 1_700_000_000_000,
        };
        let cred = credentials_from_token(token).map_err(|e| err(e.to_string()))?;
        let encoded = serde_json::to_value(&cred).map_err(|e| err(e.to_string()))?;
        let back: OAuthCredential =
            serde_json::from_value(encoded).map_err(|e| err(e.to_string()))?;
        assert_eq!(
            back.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-round".into()))
        );
        assert_eq!(back.refresh, "refresh-1");
        assert_eq!(back.expires, 1_700_000_000_000);
        Ok(())
    }

    #[test]
    fn authorization_flow_uses_s256_and_fixed_client_scope_redirect() -> TestResult {
        let endpoints = OpenAiCodexEndpoints::default();
        let flow = create_authorization_flow(&endpoints, "pi").map_err(|e| err(e.to_string()))?;
        assert_eq!(flow.state.len(), 32);
        assert!(flow.state.chars().all(|c| c.is_ascii_hexdigit()));
        let url = reqwest::Url::parse(&flow.url).map_err(|e| err(e.to_string()))?;
        let challenge = url
            .query_pairs()
            .find(|(k, _)| k == "code_challenge")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| err("missing challenge"))?;
        assert_eq!(s256_challenge(&flow.verifier), challenge);
        let pairs: BTreeMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(pairs.get("scope").map(String::as_str), Some(SCOPE));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            pairs.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            pairs.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(pairs.get("originator").map(String::as_str), Some("pi"));
        Ok(())
    }

    #[tokio::test]
    async fn exchange_and_refresh_form_bodies_and_no_skew_expiry() -> TestResult {
        let access = make_jwt("acct-ex");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "new-refresh",
            3600,
            Arc::clone(&capture),
        )?;

        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url: token_url.clone(),
                    ..OpenAiCodexEndpoints::default()
                });

        let before = now_epoch_ms().map_err(|e| err(e.to_string()))?;
        let cred = oauth
            .exchange_authorization_code_for_credentials("code-1", "verifier-1", REDIRECT_URI, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        let after = now_epoch_ms().map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.refresh, "new-refresh");
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-ex".into()))
        );
        assert!(cred.expires >= before + 3600 * 1000 - 2_000);
        assert!(cred.expires <= after + 3600 * 1000 + 2_000);

        let body = capture
            .lock()
            .await
            .clone()
            .ok_or_else(|| err("missing body"))?;
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(body.contains("code=code-1"));
        assert!(body.contains("code_verifier=verifier-1"));
        assert!(body.contains("redirect_uri="));

        let access2 = make_jwt("acct-ref");
        let capture2 = Arc::new(AsyncMutex::new(None));
        let token_url2 = spawn_form_token_server(
            "refresh_token",
            &access2,
            "rotated-refresh",
            7200,
            Arc::clone(&capture2),
        )?;
        let oauth2 =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url: token_url2,
                    ..OpenAiCodexEndpoints::default()
                });
        let mut previous = cred;
        previous
            .extra
            .insert("customFlag".into(), Value::Bool(true));
        let refreshed = oauth2
            .refresh(&previous, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.refresh, "rotated-refresh");
        assert_eq!(refreshed.access, access2);
        assert_eq!(
            refreshed.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-ref".into()))
        );
        assert_eq!(refreshed.extra.get("customFlag"), Some(&Value::Bool(true)));
        let refresh_body = capture2
            .lock()
            .await
            .clone()
            .ok_or_else(|| err("missing refresh body"))?;
        assert!(refresh_body.contains("grant_type=refresh_token"));
        assert!(refresh_body.contains("refresh_token=new-refresh"));
        assert!(refresh_body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        Ok(())
    }

    #[tokio::test]
    async fn to_auth_returns_access_as_api_key() -> TestResult {
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?);
        let cred = OAuthCredential {
            refresh: "r".into(),
            access: "access-token".into(),
            expires: 1,
            extra: BTreeMap::from([(ACCOUNT_ID_EXTRA_KEY.into(), Value::String("acct".into()))]),
        };
        let auth = oauth.to_auth(&cred).await.map_err(|e| err(e.to_string()))?;
        assert_eq!(auth.api_key.as_deref(), Some("access-token"));
        assert!(auth.headers.is_none());
        assert!(auth.base_url.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn soft_port_conflict_falls_back_to_manual_code() -> TestResult {
        let blocker = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = blocker.local_addr().map_err(|e| err(e.to_string()))?.port();

        let access = make_jwt("acct-manual");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "refresh-m",
            60,
            Arc::clone(&capture),
        )?;

        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    callback_port: port,
                    redirect_uri: format!("http://localhost:{port}/auth/callback"),
                    ..OpenAiCodexEndpoints::default()
                });

        let interaction = ScriptedInteraction::new(vec![
            Ok(OPENAI_CODEX_BROWSER_LOGIN_METHOD.into()),
            Ok("manual-code-only".into()),
        ]);
        let cred = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-manual".into()))
        );
        assert_eq!(cred.refresh, "refresh-m");
        let body = capture
            .lock()
            .await
            .clone()
            .ok_or_else(|| err("missing body"))?;
        assert!(body.contains("code=manual-code-only"));
        Ok(())
    }

    #[tokio::test]
    async fn soft_fail_server_wait_returns_none() -> TestResult {
        let blocker = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = blocker.local_addr().map_err(|e| err(e.to_string()))?.port();
        let server = OAuthCallbackServer::start_soft(OAuthCallbackConfig {
            port,
            path: CALLBACK_PATH.into(),
            expected_state: "state".into(),
            success_message: "ok".into(),
            host: None,
        })
        .await;
        assert!(server.is_soft_failed());
        assert!(server.wait_for_code().await.is_none());
        server.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn browser_login_manual_fallback_after_soft_fail() -> TestResult {
        let blocker = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = blocker.local_addr().map_err(|e| err(e.to_string()))?.port();

        let access = make_jwt("acct-login");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "refresh-login",
            90,
            Arc::clone(&capture),
        )?;

        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    callback_port: port,
                    redirect_uri: format!("http://localhost:{port}/auth/callback"),
                    ..OpenAiCodexEndpoints::default()
                });

        let interaction = ScriptedInteraction::new(vec![
            Ok(OPENAI_CODEX_BROWSER_LOGIN_METHOD.into()),
            Ok("paste-code-only".into()),
        ]);
        let cred = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-login".into()))
        );
        assert_eq!(cred.refresh, "refresh-login");
        Ok(())
    }

    #[tokio::test]
    async fn callback_success_path_exchanges_code() -> TestResult {
        let access = make_jwt("acct-cb");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "refresh-cb",
            30,
            Arc::clone(&capture),
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| err(e.to_string()))?
            .port();
        drop(listener);
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    callback_port: port,
                    redirect_uri: format!("http://localhost:{port}/auth/callback"),
                    ..OpenAiCodexEndpoints::default()
                });
        let hang = CancellationToken::new();
        let parent_cancel = CancellationToken::new();
        let interaction = HangManual {
            select_done: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            hang: hang.clone(),
            prompt_signal: Mutex::new(None),
            signal: Some(parent_cancel.clone()),
        };
        let login = oauth.login(&interaction);
        let callback = hit_callback_when_ready(&interaction, port);
        let (cred, hit) = tokio::join!(login, callback);
        hit?;
        let cred = cred.map_err(|e| err(e.to_string()))?;
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-cb".into()))
        );
        let body = capture
            .lock()
            .await
            .clone()
            .ok_or_else(|| err("missing token body"))?;
        assert!(body.contains("code=from-browser"));
        let prompt_signal = interaction
            .prompt_signal
            .lock()
            .map_err(|_| err("prompt signal lock"))?
            .clone()
            .ok_or_else(|| err("missing prompt signal"))?;
        assert!(prompt_signal.is_cancelled());
        assert!(
            !parent_cancel.is_cancelled(),
            "settling the prompt child must not cancel the parent"
        );
        hang.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn browser_login_parent_cancel_propagates_to_prompt_child() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| err(e.to_string()))?
            .port();
        drop(listener);
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url: "http://127.0.0.1:1/unused".into(),
                    callback_port: port,
                    redirect_uri: format!("http://localhost:{port}/auth/callback"),
                    ..OpenAiCodexEndpoints::default()
                });
        let parent_cancel = CancellationToken::new();
        let interaction = HangManual {
            select_done: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            hang: CancellationToken::new(),
            prompt_signal: Mutex::new(None),
            signal: Some(parent_cancel.clone()),
        };

        let cancel = async {
            loop {
                let prompt_signal = interaction
                    .prompt_signal
                    .lock()
                    .ok()
                    .and_then(|signal| signal.clone());
                if let Some(prompt_signal) = prompt_signal {
                    parent_cancel.cancel();
                    assert!(
                        prompt_signal.is_cancelled(),
                        "parent cancellation must synchronously propagate to the active prompt child"
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        let (login, ()) = tokio::join!(oauth.login(&interaction), cancel);
        assert!(matches!(login, Err(AuthError::Cancelled)));
        let prompt_signal = interaction
            .prompt_signal
            .lock()
            .map_err(|_| err("prompt signal lock"))?
            .clone()
            .ok_or_else(|| err("missing prompt signal"))?;
        assert!(
            prompt_signal.is_cancelled(),
            "parent cancellation must propagate to the prompt child"
        );
        Ok(())
    }

    #[tokio::test]
    async fn parent_cancel_aborts_post_race_token_exchange() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        drop(listener);
        let (token_url, request_started) = spawn_hanging_token_server("/token")?;
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|error| error.to_string())?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    callback_port: port,
                    redirect_uri: format!("http://localhost:{port}/auth/callback"),
                    ..OpenAiCodexEndpoints::default()
                });
        let parent_cancel = CancellationToken::new();
        let interaction = HangManual {
            select_done: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
            hang: CancellationToken::new(),
            prompt_signal: Mutex::new(None),
            signal: Some(parent_cancel.clone()),
        };

        let exchange_cancel = async {
            tokio::time::timeout(Duration::from_secs(2), request_started)
                .await
                .map_err(|_| err("token exchange did not start"))?
                .map_err(|_| err("token exchange signal dropped"))?;
            let prompt_signal = interaction
                .prompt_signal
                .lock()
                .map_err(|_| err("prompt signal lock"))?
                .clone()
                .ok_or_else(|| err("missing prompt signal"))?;
            assert!(prompt_signal.is_cancelled());
            assert!(!parent_cancel.is_cancelled());
            parent_cancel.cancel();
            Ok::<(), String>(())
        };
        let (login, callback, cancel) = tokio::join!(
            oauth.login(&interaction),
            hit_callback_when_ready(&interaction, port),
            exchange_cancel,
        );
        callback?;
        cancel?;
        assert!(matches!(login, Err(AuthError::Cancelled)));
        Ok(())
    }

    #[tokio::test]
    async fn bad_state_on_manual_paste_is_rejected() -> TestResult {
        let blocker = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let port = blocker.local_addr().map_err(|e| err(e.to_string()))?.port();
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url: "http://127.0.0.1:1/token".into(),
                    callback_port: port,
                    ..OpenAiCodexEndpoints::default()
                });
        let interaction = ScriptedInteraction::new(vec![
            Ok(OPENAI_CODEX_BROWSER_LOGIN_METHOD.into()),
            Ok("http://localhost/cb?code=x&state=definitely-wrong".into()),
        ]);
        match oauth.login(&interaction).await {
            Ok(_) => Err(err("expected state mismatch")),
            Err(error) => {
                assert_eq!(error.to_string(), "State mismatch");
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn cancel_signal_aborts_refresh() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        let host = address.to_string();
        thread::spawn(move || {
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let req = read_http_request(&mut stream);
                let text = String::from_utf8_lossy(&req);
                let lowered = text.to_ascii_lowercase();
                if !text.starts_with("POST /token ") || !lowered.contains(&format!("host: {host}"))
                {
                    let _ = stream.write_all(http_response("404 Not Found", "").as_slice());
                    continue;
                }
                thread::sleep(Duration::from_secs(5));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
                return;
            }
        });
        let token_url = format!("http://{address}/token");
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    ..OpenAiCodexEndpoints::default()
                });
        let token = CancellationToken::new();
        let cancel = token.clone();
        let cred = OAuthCredential {
            refresh: "r".into(),
            access: make_jwt("a"),
            expires: 1,
            extra: BTreeMap::new(),
        };
        let handle = tokio::spawn(async move { oauth.refresh(&cred, Some(cancel)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let result = handle.await.map_err(|e| err(e.to_string()))?;
        match result {
            Err(AuthError::Cancelled) => Ok(()),
            other => Err(err(format!("expected Cancelled, got {other:?}"))),
        }
    }

    #[tokio::test]
    async fn device_login_polls_and_exchanges() -> TestResult {
        let usercode = spawn_device_user_code_server()?;
        let (device_token, token_hits) = spawn_device_token_server()?;
        let access = make_jwt("acct-device");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "refresh-dev",
            45,
            Arc::clone(&capture),
        )?;
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    device_user_code_url: usercode,
                    device_token_url: device_token,
                    device_redirect_uri: DEVICE_REDIRECT_URI.into(),
                    ..OpenAiCodexEndpoints::default()
                });
        let interaction =
            ScriptedInteraction::new(vec![Ok(OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.into())]);
        let cred = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-device".into()))
        );
        let body = capture
            .lock()
            .await
            .clone()
            .ok_or_else(|| err("missing body"))?;
        assert!(body.contains("code=dev-code"));
        assert!(body.contains("code_verifier=dev-verifier"));
        assert!(body.contains(&format!(
            "redirect_uri={}",
            DEVICE_REDIRECT_URI.replace(':', "%3A").replace('/', "%2F")
        )));
        assert_eq!(
            token_hits.load(Ordering::SeqCst),
            2,
            "device flow must poll exactly twice (pending then success)"
        );
        Ok(())
    }

    /// Send one sweeper-style `GET /` at a stub, draining its 404 reply.
    fn sweep_probe_stub(url: &str) -> Result<(), String> {
        let addr = url
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .ok_or_else(|| err("bad stub url"))?;
        let mut stream = TcpStream::connect(addr).map_err(|e| err(e.to_string()))?;
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: sweep\r\nConnection: close\r\n\r\n")
            .map_err(|e| err(e.to_string()))?;
        let mut buf = [0_u8; 1024];
        let _ = stream.read(&mut buf);
        Ok(())
    }

    /// A localhost sweep (`GET /` at every new listener) must not consume
    /// scripted stub replies: after probing all three stubs, the device
    /// flow still polls exactly twice and exchanges.
    #[tokio::test]
    async fn sweep_get_does_not_consume_stub_scripts() -> TestResult {
        let usercode = spawn_device_user_code_server()?;
        let (device_token, token_hits) = spawn_device_token_server()?;
        let access = make_jwt("acct-device");
        let capture = Arc::new(AsyncMutex::new(None));
        let token_url = spawn_form_token_server(
            "authorization_code",
            &access,
            "refresh-dev",
            45,
            Arc::clone(&capture),
        )?;
        sweep_probe_stub(&usercode)?;
        sweep_probe_stub(&device_token)?;
        sweep_probe_stub(&token_url)?;
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|e| err(e.to_string()))?)
                .with_endpoints(OpenAiCodexEndpoints {
                    token_url,
                    device_user_code_url: usercode,
                    device_token_url: device_token,
                    device_redirect_uri: DEVICE_REDIRECT_URI.into(),
                    ..OpenAiCodexEndpoints::default()
                });
        let interaction =
            ScriptedInteraction::new(vec![Ok(OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.into())]);
        let cred = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(
            cred.extra.get(ACCOUNT_ID_EXTRA_KEY),
            Some(&Value::String("acct-device".into()))
        );
        assert_eq!(
            token_hits.load(Ordering::SeqCst),
            2,
            "sweeper traffic must not consume the pending/success script"
        );
        Ok(())
    }

    #[tokio::test]
    async fn parent_cancel_aborts_in_flight_device_poll() -> TestResult {
        let usercode = spawn_device_user_code_server()?;
        let (device_token, request_started) = spawn_hanging_token_server("/device-token")?;
        let oauth =
            OpenAiCodexOAuth::with_http(AuthHttpClient::new().map_err(|error| error.to_string())?)
                .with_endpoints(OpenAiCodexEndpoints {
                    device_user_code_url: usercode,
                    device_token_url: device_token,
                    ..OpenAiCodexEndpoints::default()
                });
        let parent_cancel = CancellationToken::new();
        let mut interaction =
            ScriptedInteraction::new(vec![Ok(OPENAI_CODEX_DEVICE_CODE_LOGIN_METHOD.into())]);
        interaction.signal = Some(parent_cancel.clone());

        let cancel_poll = async {
            tokio::time::timeout(Duration::from_secs(2), request_started)
                .await
                .map_err(|_| err("device poll did not start"))?
                .map_err(|_| err("device poll signal dropped"))?;
            parent_cancel.cancel();
            Ok::<(), String>(())
        };
        let (login, cancel) = tokio::join!(oauth.login(&interaction), cancel_poll);
        cancel?;
        assert!(matches!(login, Err(AuthError::Cancelled)));
        assert!(
            interaction
                .events
                .lock()
                .map_err(|_| err("events lock"))?
                .iter()
                .any(|event| matches!(event, AuthEvent::DeviceCode { .. }))
        );
        Ok(())
    }

    #[test]
    fn name_is_static_display_string() {
        let oauth = OpenAiCodexOAuth::default();
        assert_eq!(oauth.name(), OAUTH_DISPLAY_NAME);
        assert_eq!(oauth.login_label(), Some(OAUTH_DISPLAY_NAME));
    }

    #[test]
    fn token_response_errors_never_include_secret_values() -> TestResult {
        let access = "secret-access-token";
        let refresh = "secret-refresh-token";
        let malformed = super::super::super::http::AuthHttpResponse {
            ok: true,
            status: 200,
            body: json!({
                "access_token": access,
                "refresh_token": refresh
            }),
            raw_body: format!("{{\"access_token\":\"{access}\",\"refresh_token\":\"{refresh}\"}}"),
        };
        let error = expect_err(
            read_token_response(&malformed, TokenOperation::Exchange),
            "malformed token response",
        )?;
        let message = error.to_string();
        assert_eq!(
            message,
            "OpenAI Codex token exchange response missing or invalid fields: expires_in"
        );
        assert!(!message.contains(access));
        assert!(!message.contains(refresh));

        let failed = super::super::super::http::AuthHttpResponse {
            ok: false,
            status: 400,
            body: json!({
                "error": "invalid_grant",
                "access_token": access,
                "refresh_token": refresh
            }),
            raw_body: format!("provider details: {access} {refresh}"),
        };
        let error = expect_err(
            read_token_response(&failed, TokenOperation::Refresh),
            "failed token response",
        )?;
        let message = error.to_string();
        assert_eq!(message, "OpenAI Codex token refresh failed (400)");
        assert!(!message.contains(access));
        assert!(!message.contains(refresh));
        Ok(())
    }

    #[test]
    fn seconds_f64_to_millis_rejects_invalid() {
        assert!(seconds_f64_to_millis(f64::NAN).is_err());
        assert!(seconds_f64_to_millis(-1.0).is_err());
        assert!(matches!(seconds_f64_to_millis(1.5), Ok(1500)));
        assert!(matches!(seconds_f64_to_millis(3600.0), Ok(3_600_000)));
    }
}
