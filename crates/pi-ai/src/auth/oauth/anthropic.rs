//! Anthropic OAuth flow (Claude Pro/Max).
//!
//! Port of `.references/pi/packages/ai/src/auth/oauth/anthropic.ts`.
//!
//! Quirks preserved from the reference:
//! - PKCE verifier is reused as the OAuth `state`
//! - Loopback binds `PI_OAUTH_CALLBACK_HOST` or `127.0.0.1` on fixed port `53692`
//! - Authorization `redirect_uri` is always `http://localhost:53692/callback`
//! - Callback bind failures are hard errors (not soft-fail)
//! - Token exchange/refresh use JSON bodies (not form-encoded)
//! - Access-token expiry applies a 5-minute clock skew

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::AuthHttpClient;
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::callback_server::{
    OAuthCallbackCode, OAuthCallbackConfig, OAuthCallbackServer, callback_host_from_env,
};
use super::pkce::generate_pkce;

/// Base64-decoded Claude Code OAuth client id.
///
/// Source: `atob("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl")`.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Browser authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token exchange / refresh endpoint.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Fixed loopback callback port.
pub const CALLBACK_PORT: u16 = 53692;

/// Fixed loopback callback path.
pub const CALLBACK_PATH: &str = "/callback";

/// Authorization redirect URI (always `localhost`, independent of bind host).
pub const REDIRECT_URI: &str = "http://localhost:53692/callback";

/// OAuth scopes requested during authorization.
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Access-token lifetime skew applied after exchange/refresh (5 minutes).
pub const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

/// Display name for the Anthropic subscription OAuth handler.
pub const OAUTH_NAME: &str = "Anthropic (Claude Pro/Max)";

/// Anthropic OAuth flow with injectable HTTP for tests.
#[derive(Clone, Debug)]
pub struct AnthropicOAuth {
    http: AuthHttpClient,
    authorize_url: String,
    token_url: String,
    /// Bind port for the loopback callback. Production always uses [`CALLBACK_PORT`].
    callback_port: u16,
}

impl AnthropicOAuth {
    /// Build a production Anthropic OAuth handler.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the shared HTTP client cannot be constructed.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new()
                .map_err(super::super::http::AuthHttpError::into_auth_error)?,
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            callback_port: CALLBACK_PORT,
        })
    }

    /// Build a handler around an existing HTTP client (and optional URL overrides).
    #[must_use]
    pub fn with_http(http: AuthHttpClient) -> Self {
        Self {
            http,
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            callback_port: CALLBACK_PORT,
        }
    }

    /// Override the token endpoint (tests only).
    #[must_use]
    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into();
        self
    }

    /// Override the authorize endpoint (tests only).
    #[must_use]
    pub fn with_authorize_url(mut self, authorize_url: impl Into<String>) -> Self {
        self.authorize_url = authorize_url.into();
        self
    }

    /// Override the callback bind port (tests only). Redirect URI stays fixed.
    #[must_use]
    pub fn with_callback_port(mut self, port: u16) -> Self {
        self.callback_port = port;
        self
    }

    /// Build the browser authorization URL for a PKCE challenge/verifier pair.
    #[must_use]
    pub fn authorization_url(&self, challenge: &str, verifier: &str) -> String {
        // state == verifier is intentional Anthropic quirk.
        let mut params: BTreeMap<&str, &str> = BTreeMap::new();
        params.insert("code", "true");
        params.insert("client_id", CLIENT_ID);
        params.insert("response_type", "code");
        params.insert("redirect_uri", REDIRECT_URI);
        params.insert("scope", SCOPES);
        params.insert("code_challenge", challenge);
        params.insert("code_challenge_method", "S256");
        params.insert("state", verifier);
        format!("{}?{}", self.authorize_url, encode_query(&params))
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        state: &str,
        verifier: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        #[derive(Serialize)]
        struct Body<'a> {
            grant_type: &'static str,
            client_id: &'static str,
            code: &'a str,
            state: &'a str,
            redirect_uri: &'static str,
            code_verifier: &'a str,
        }

        let body = Body {
            grant_type: "authorization_code",
            client_id: CLIENT_ID,
            code,
            state,
            redirect_uri: REDIRECT_URI,
            code_verifier: verifier,
        };

        let response_body = self
            .http
            .post_json(&self.token_url, &body, None, cancellation)
            .await
            .map_err(|error| {
                if matches!(error, super::super::http::AuthHttpError::Cancelled) {
                    AuthError::Cancelled
                } else {
                    AuthError::message(format!(
                        "Token exchange request failed. url={}; redirect_uri={}; response_type=authorization_code; details={}",
                        self.token_url, REDIRECT_URI, error
                    ))
                }
            })?;

        parse_token_response(&response_body, &self.token_url, "Token exchange")
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        // Scope is intentionally omitted from refresh requests.
        #[derive(Serialize)]
        struct Body<'a> {
            grant_type: &'static str,
            client_id: &'static str,
            refresh_token: &'a str,
        }

        let body = Body {
            grant_type: "refresh_token",
            client_id: CLIENT_ID,
            refresh_token,
        };

        let response_body = self
            .http
            .post_json(&self.token_url, &body, None, cancellation)
            .await
            .map_err(|error| {
                if matches!(error, super::super::http::AuthHttpError::Cancelled) {
                    AuthError::Cancelled
                } else {
                    AuthError::message(format!(
                        "Anthropic token refresh request failed. url={}; details={}",
                        self.token_url, error
                    ))
                }
            })?;

        parse_token_response(&response_body, &self.token_url, "Anthropic token refresh")
    }

    async fn login_inner(
        &self,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, AuthError> {
        if interaction
            .signal()
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(AuthError::Cancelled);
        }

        let pkce = generate_pkce()?;
        // Anthropic reuses the PKCE verifier as the OAuth state value.
        let server = self.start_login_callback(&pkce.verifier).await?;
        let prompt_cancel = interaction
            .signal()
            .map_or_else(CancellationToken::new, |parent| parent.child_token());

        interaction.notify(AuthEvent::AuthUrl {
            url: self.authorization_url(&pkce.challenge, &pkce.verifier),
            instructions: Some(
                "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .to_owned(),
            ),
        });

        let race_result =
            race_login_sources(interaction, &server, &prompt_cancel, &pkce.verifier).await;

        // Always tear down the listener before propagating the race result.
        server.close().await;
        prompt_cancel.cancel();

        let received = take_login_code(race_result, interaction)?;
        validate_callback_code(&received, &pkce.verifier)?;

        interaction.notify(AuthEvent::Progress {
            message: "Exchanging authorization code for tokens...".to_owned(),
        });

        self.exchange_authorization_code(
            &received.code,
            &received.state,
            &pkce.verifier,
            interaction.signal().as_ref(),
        )
        .await
    }

    async fn start_login_callback(
        &self,
        expected_state: &str,
    ) -> Result<OAuthCallbackServer, AuthError> {
        // Hard bind failure: Anthropic does not soft-fail (unlike Codex).
        OAuthCallbackServer::start(OAuthCallbackConfig {
            port: self.callback_port,
            path: CALLBACK_PATH.to_owned(),
            expected_state: expected_state.to_owned(),
            success_message: "Anthropic authentication completed. You can close this window."
                .to_owned(),
            host: Some(callback_host_from_env()),
        })
        .await
        .map_err(AuthError::from)
    }
}

async fn race_login_sources(
    interaction: &dyn AuthInteraction,
    server: &OAuthCallbackServer,
    prompt_cancel: &CancellationToken,
    verifier: &str,
) -> Result<Option<OAuthCallbackCode>, AuthError> {
    let verifier_for_manual = verifier.to_owned();
    let prompt_cancel_manual = prompt_cancel.clone();
    let manual = async {
        match interaction
            .prompt(AuthPrompt::ManualCode {
                message: "Complete login in your browser, or paste the authorization code / redirect URL here:"
                    .to_owned(),
                placeholder: Some(REDIRECT_URI.to_owned()),
                signal: Some(prompt_cancel_manual),
            })
            .await
        {
            Ok(input) => parse_manual_code(&input, &verifier_for_manual),
            Err(error) => Err(error),
        }
    };

    tokio::select! {
        biased;
        callback = server.wait_for_code() => {
            prompt_cancel.cancel();
            Ok(callback)
        }
        manual = manual => {
            server.cancel_wait().await;
            manual
        }
        () = wait_for_optional_cancel(interaction.signal()) => {
            prompt_cancel.cancel();
            server.cancel_wait().await;
            Err(AuthError::Cancelled)
        }
    }
}

async fn wait_for_optional_cancel(signal: Option<CancellationToken>) {
    if let Some(signal) = signal {
        signal.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn parse_manual_code(input: &str, verifier: &str) -> Result<Option<OAuthCallbackCode>, AuthError> {
    let parsed = parse_authorization_input(input);
    if let Some(state) = parsed.state.as_deref()
        && state != verifier
    {
        return Err(AuthError::message("OAuth state mismatch"));
    }
    let Some(code) = parsed.code.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(OAuthCallbackCode {
        code,
        state: parsed.state.unwrap_or_else(|| verifier.to_owned()),
    }))
}

fn take_login_code(
    race_result: Result<Option<OAuthCallbackCode>, AuthError>,
    interaction: &dyn AuthInteraction,
) -> Result<OAuthCallbackCode, AuthError> {
    let Some(received) = race_result? else {
        if interaction
            .signal()
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(AuthError::Cancelled);
        }
        return Err(AuthError::message("Missing authorization code"));
    };
    Ok(received)
}

fn validate_callback_code(received: &OAuthCallbackCode, verifier: &str) -> Result<(), AuthError> {
    if received.code.is_empty() {
        return Err(AuthError::message("Missing authorization code"));
    }
    if received.state.is_empty() {
        return Err(AuthError::message("Missing OAuth state"));
    }
    if received.state != verifier {
        return Err(AuthError::message("OAuth state mismatch"));
    }
    Ok(())
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            http: AuthHttpClient::from_client(reqwest::Client::new()),
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            callback_port: CALLBACK_PORT,
        })
    }
}

impl OAuthAuth for AnthropicOAuth {
    fn name(&self) -> &str {
        OAUTH_NAME
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(self.login_inner(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err(AuthError::Cancelled);
            }
            self.refresh_token(&credential.refresh, signal.as_ref())
                .await
        })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        Box::pin(async move {
            // Request-time OAuth material: the access token is the api key.
            // Adapter code detects `sk-ant-oat` and applies beta/Bearer headers.
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: None,
            })
        })
    }
}

/// Parsed authorization input from a redirect URL, `code#state`, query fragment,
/// or bare authorization code.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedAuthorizationInput {
    /// Authorization code when present.
    pub code: Option<String>,
    /// OAuth state when present.
    pub state: Option<String>,
}

/// Parse a pasted authorization redirect URL, `code#state`, query string, or bare code.
#[must_use]
pub fn parse_authorization_input(input: &str) -> ParsedAuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuthorizationInput::default();
    }

    if let Ok(url) = reqwest::Url::parse(value) {
        let mut code = None;
        let mut state = None;
        for (key, val) in url.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() => code = Some(val.into_owned()),
                "state" if state.is_none() => state = Some(val.into_owned()),
                _ => {}
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
            match key {
                "code" if code.is_none() => code = Some(percent_decode(val)),
                "state" if state.is_none() => state = Some(percent_decode(val)),
                _ => {}
            }
        }
        return ParsedAuthorizationInput { code, state };
    }

    ParsedAuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

fn parse_token_response(
    response_body: &str,
    token_url: &str,
    context: &str,
) -> Result<OAuthCredential, AuthError> {
    let value: Value = serde_json::from_str(response_body).map_err(|error| {
        AuthError::message(format!(
            "{context} returned invalid JSON. url={token_url}; details={error}"
        ))
    })?;

    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AuthError::message(format!(
                "{context} response missing access_token. url={token_url}"
            ))
        })?;
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AuthError::message(format!(
                "{context} response missing refresh_token. url={token_url}"
            ))
        })?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .or_else(|| {
            value
                .get("expires_in")
                .and_then(Value::as_u64)
                .and_then(|value| i64::try_from(value).ok())
        })
        .or_else(|| {
            value
                .get("expires_in")
                .and_then(Value::as_str)
                .and_then(|raw| {
                    raw.parse::<i64>().ok().or_else(|| {
                        raw.parse::<u64>()
                            .ok()
                            .and_then(|value| i64::try_from(value).ok())
                    })
                })
        })
        .ok_or_else(|| {
            AuthError::message(format!(
                "{context} response missing expires_in. url={token_url}"
            ))
        })?;

    Ok(OAuthCredential {
        refresh: refresh.to_owned(),
        access: access.to_owned(),
        expires: now_ms().saturating_add(expires_in.saturating_mul(1000) - EXPIRY_SKEW_MS),
        extra: BTreeMap::new(),
    })
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn encode_query(params: &BTreeMap<&str, &str>) -> String {
    let mut out = String::new();
    for (index, (key, value)) in params.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&urlencoding_encode(key));
        out.push('=');
        out.push_str(&urlencoding_encode(value));
    }
    out
}

fn urlencoding_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(nibble(byte >> 4));
                out.push(nibble(byte & 0x0f));
            }
        }
    }
    out
}

fn nibble(value: u8) -> char {
    char::from(match value {
        0..=9 => b'0' + value,
        _ => b'A' + (value - 10),
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = from_hex(bytes[index + 1]);
                let lo = from_hex(bytes[index + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi << 4) | lo);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::auth::oauth::callback_server::CallbackServerError;
    use crate::auth::types::AuthEvent;

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

    struct TestInteraction {
        events: Mutex<Vec<AuthEvent>>,
        prompts: Mutex<Vec<AuthPrompt>>,
        manual_response: Mutex<Option<Result<String, AuthError>>>,
        signal: Option<CancellationToken>,
        prompt_signal_out: Mutex<Option<CancellationToken>>,
    }

    impl TestInteraction {
        fn with_manual(response: Result<String, AuthError>) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                prompts: Mutex::new(Vec::new()),
                manual_response: Mutex::new(Some(response)),
                signal: None,
                prompt_signal_out: Mutex::new(None),
            }
        }

        fn with_signal(signal: CancellationToken) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                prompts: Mutex::new(Vec::new()),
                manual_response: Mutex::new(None),
                signal: Some(signal),
                prompt_signal_out: Mutex::new(None),
            }
        }
    }

    impl AuthInteraction for TestInteraction {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                if let AuthPrompt::ManualCode {
                    signal: Some(signal),
                    ..
                } = &prompt
                    && let Ok(mut out) = self.prompt_signal_out.lock()
                {
                    *out = Some(signal.clone());
                }
                if let Ok(mut prompts) = self.prompts.lock() {
                    prompts.push(prompt);
                }
                if let Ok(mut manual) = self.manual_response.lock()
                    && let Some(result) = manual.take()
                {
                    return result;
                }
                if let Some(signal) = &self.signal {
                    signal.cancelled().await;
                    return Err(AuthError::Cancelled);
                }
                std::future::pending().await
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

    /// Manual prompt that waits until the test injects a response after seeing `auth_url`.
    struct DeferredManual {
        events: Mutex<Vec<AuthEvent>>,
        response: Arc<tokio::sync::Mutex<Option<String>>>,
        notify: Arc<tokio::sync::Notify>,
        prompt_signal: Mutex<Option<CancellationToken>>,
        signal: Option<CancellationToken>,
    }

    impl DeferredManual {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                response: Arc::new(tokio::sync::Mutex::new(None)),
                notify: Arc::new(tokio::sync::Notify::new()),
                prompt_signal: Mutex::new(None),
                signal: None,
            }
        }

        fn with_signal(signal: CancellationToken) -> Self {
            Self {
                signal: Some(signal),
                ..Self::new()
            }
        }

        fn auth_url(&self) -> Option<String> {
            let Ok(events) = self.events.lock() else {
                return None;
            };
            events.iter().find_map(|event| {
                if let AuthEvent::AuthUrl { url, .. } = event {
                    Some(url.clone())
                } else {
                    None
                }
            })
        }

        fn complete_manual(&self, value: String) {
            let response = Arc::clone(&self.response);
            let notify = Arc::clone(&self.notify);
            tokio::spawn(async move {
                *response.lock().await = Some(value);
                notify.notify_waiters();
            });
        }

        fn prompt_was_cancelled(&self) -> bool {
            self.prompt_signal
                .lock()
                .ok()
                .as_ref()
                .and_then(|guard| guard.as_ref())
                .is_some_and(CancellationToken::is_cancelled)
        }
    }

    impl AuthInteraction for DeferredManual {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                if let AuthPrompt::ManualCode {
                    signal: Some(signal),
                    ..
                } = &prompt
                    && let Ok(mut out) = self.prompt_signal.lock()
                {
                    *out = Some(signal.clone());
                }
                loop {
                    if let Some(value) = self.response.lock().await.take() {
                        return Ok(value);
                    }
                    self.notify.notified().await;
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

    /// Manual prompt that never resolves so the callback path can win.
    struct PendingManual {
        events: Mutex<Vec<AuthEvent>>,
        prompt_signal: Mutex<Option<CancellationToken>>,
    }

    impl PendingManual {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                prompt_signal: Mutex::new(None),
            }
        }

        fn auth_url(&self) -> Option<String> {
            let Ok(events) = self.events.lock() else {
                return None;
            };
            events.iter().find_map(|event| {
                if let AuthEvent::AuthUrl { url, .. } = event {
                    Some(url.clone())
                } else {
                    None
                }
            })
        }

        fn prompt_was_cancelled(&self) -> bool {
            self.prompt_signal
                .lock()
                .ok()
                .as_ref()
                .and_then(|guard| guard.as_ref())
                .is_some_and(CancellationToken::is_cancelled)
        }
    }

    impl AuthInteraction for PendingManual {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                if let AuthPrompt::ManualCode {
                    signal: Some(signal),
                    ..
                } = &prompt
                {
                    if let Ok(mut out) = self.prompt_signal.lock() {
                        *out = Some(signal.clone());
                    }
                    signal.cancelled().await;
                    return Err(AuthError::Cancelled);
                }
                std::future::pending().await
            })
        }

        fn notify(&self, event: AuthEvent) {
            if let Ok(mut events) = self.events.lock() {
                events.push(event);
            }
        }

        fn signal(&self) -> Option<CancellationToken> {
            None
        }
    }

    fn free_port() -> Result<u16, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        Ok(listener
            .local_addr()
            .map_err(|e| err(e.to_string()))?
            .port())
    }

    #[derive(Clone)]
    struct CapturedRequest {
        headers: String,
        body: String,
    }

    fn spawn_json_token_server(
        expected_grant: &'static str,
        response_body: String,
        capture: Arc<Mutex<Option<CapturedRequest>>>,
    ) -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = vec![0_u8; 16 * 1024];
            let Ok(n) = stream.read(&mut buf) else {
                return;
            };
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let (headers, body) = request
                .split_once("\r\n\r\n")
                .map(|(h, b)| (h.to_owned(), b.to_owned()))
                .unwrap_or((request, String::new()));
            if let Ok(mut guard) = capture.lock() {
                *guard = Some(CapturedRequest {
                    headers: headers.clone(),
                    body: body.clone(),
                });
            }
            let headers_l = headers.to_ascii_lowercase();
            if !headers_l.contains("content-type: application/json")
                || !headers_l.contains("accept: application/json")
                || !body.contains(&format!("\"grant_type\":\"{expected_grant}\""))
            {
                return;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        Ok(format!("http://{address}/v1/oauth/token"))
    }

    #[test]
    fn client_id_matches_decoded_reference() {
        assert_eq!(CLIENT_ID, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(CALLBACK_PORT, 53692);
        assert_eq!(CALLBACK_PATH, "/callback");
        assert_eq!(REDIRECT_URI, "http://localhost:53692/callback");
    }

    #[test]
    fn parse_authorization_input_variants() {
        let url = parse_authorization_input(
            "http://localhost:53692/callback?code=abc&state=verifier-state",
        );
        assert_eq!(url.code.as_deref(), Some("abc"));
        assert_eq!(url.state.as_deref(), Some("verifier-state"));

        let hash = parse_authorization_input("the-code#the-state");
        assert_eq!(hash.code.as_deref(), Some("the-code"));
        assert_eq!(hash.state.as_deref(), Some("the-state"));

        let query = parse_authorization_input("code=from-query&state=st");
        assert_eq!(query.code.as_deref(), Some("from-query"));
        assert_eq!(query.state.as_deref(), Some("st"));

        let bare = parse_authorization_input("bare-code");
        assert_eq!(bare.code.as_deref(), Some("bare-code"));
        assert_eq!(bare.state, None);

        assert_eq!(
            parse_authorization_input("   "),
            ParsedAuthorizationInput::default()
        );
    }

    #[test]
    fn authorization_url_uses_verifier_as_state_and_fixed_redirect() -> TestResult {
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()));
        let url = oauth.authorization_url("challenge-value", "verifier-as-state");
        let parsed = reqwest::Url::parse(&url).map_err(|e| err(e.to_string()))?;
        assert_eq!(parsed.as_str().split('?').next(), Some(AUTHORIZE_URL));
        let params: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(params.get("code").map(String::as_str), Some("true"));
        assert_eq!(params.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
        assert_eq!(params.get("scope").map(String::as_str), Some(SCOPES));
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge-value")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        // state == verifier (Anthropic quirk)
        assert_eq!(
            params.get("state").map(String::as_str),
            Some("verifier-as-state")
        );
        Ok(())
    }

    #[tokio::test]
    async fn to_auth_uses_access_token_as_api_key() -> TestResult {
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()));
        let auth = oauth
            .to_auth(&OAuthCredential {
                refresh: "r".into(),
                access: "sk-ant-oat-token".into(),
                expires: 0,
                extra: BTreeMap::new(),
            })
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(auth.api_key.as_deref(), Some("sk-ant-oat-token"));
        assert!(auth.headers.is_none());
        assert!(auth.base_url.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_applies_skew_and_omits_scope() -> TestResult {
        let capture = Arc::new(Mutex::new(None));
        let token_url = spawn_json_token_server(
            "refresh_token",
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
                .to_owned(),
            capture.clone(),
        )?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url(token_url);
        let before = now_ms();
        let refreshed = oauth
            .refresh(
                &OAuthCredential {
                    refresh: "old-refresh".into(),
                    access: "old-access".into(),
                    expires: 0,
                    extra: BTreeMap::new(),
                },
                None,
            )
            .await
            .map_err(|e| err(e.to_string()))?;
        let after = now_ms();

        assert_eq!(refreshed.access, "new-access");
        assert_eq!(refreshed.refresh, "new-refresh");
        let expected_min = before + 3600 * 1000 - EXPIRY_SKEW_MS - 2_000;
        let expected_max = after + 3600 * 1000 - EXPIRY_SKEW_MS + 2_000;
        assert!(refreshed.expires >= expected_min);
        assert!(refreshed.expires <= expected_max);

        let captured = lock_mutex(&capture, "capture")?
            .clone()
            .ok_or_else(|| err("captured"))?;
        assert!(captured.body.contains("\"grant_type\":\"refresh_token\""));
        assert!(captured.body.contains("\"refresh_token\":\"old-refresh\""));
        assert!(
            captured
                .body
                .contains(&format!("\"client_id\":\"{CLIENT_ID}\""))
        );
        assert!(
            !captured.body.contains("scope"),
            "refresh must omit scope: {}",
            captured.body
        );
        assert!(
            !captured
                .headers
                .to_ascii_lowercase()
                .contains("authorization:"),
            "token refresh must not send Authorization"
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_manual_code_exchanges_with_localhost_redirect_and_state() -> TestResult {
        let capture = Arc::new(Mutex::new(None));
        let token_url = spawn_json_token_server(
            "authorization_code",
            r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}"#
                .to_owned(),
            capture.clone(),
        )?;
        let port = free_port()?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url(token_url)
            .with_callback_port(port);

        let parent_cancel = CancellationToken::new();
        let interaction = Arc::new(DeferredManual::with_signal(parent_cancel.clone()));
        let login = {
            let interaction = interaction.clone();
            let oauth = oauth.clone();
            tokio::spawn(async move { oauth.login(interaction.as_ref()).await })
        };

        let auth_url = loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(url) = interaction.auth_url() {
                break url;
            }
        };
        let parsed = reqwest::Url::parse(&auth_url).map_err(|e| err(e.to_string()))?;
        let params: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        let state = params.get("state").cloned().ok_or_else(|| err("state"))?;
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
        // state is the PKCE verifier (not a separate random value).
        assert!(!state.is_empty());
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );

        interaction.complete_manual(format!("{REDIRECT_URI}?code=manual-code&state={state}"));

        let credentials = tokio::time::timeout(Duration::from_secs(5), login)
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credentials.access, "access-token");
        assert_eq!(credentials.refresh, "refresh-token");

        let captured = lock_mutex(&capture, "capture")?
            .clone()
            .ok_or_else(|| err("captured"))?;
        assert!(
            captured
                .body
                .contains("\"grant_type\":\"authorization_code\"")
        );
        assert!(captured.body.contains("\"code\":\"manual-code\""));
        assert!(captured.body.contains(&format!("\"state\":\"{state}\"")));
        assert!(
            captured
                .body
                .contains(&format!("\"redirect_uri\":\"{REDIRECT_URI}\""))
        );
        assert!(captured.body.contains("\"code_verifier\":"));
        assert!(
            interaction.prompt_was_cancelled(),
            "manual prompt signal must be aborted after settle"
        );
        assert!(
            !parent_cancel.is_cancelled(),
            "settling the prompt child must not cancel the parent"
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_manual_rejects_bad_state() -> TestResult {
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url("http://127.0.0.1:1/unused")
            .with_callback_port(0);
        let interaction = TestInteraction::with_manual(Ok(
            "http://localhost:53692/callback?code=x&state=wrong-state".into(),
        ));
        let err_value = expect_err(oauth.login(&interaction).await, "bad state")?;
        assert!(
            err_value.to_string().contains("OAuth state mismatch"),
            "unexpected error: {err_value}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_callback_path_wins_over_manual_and_validates_state() -> TestResult {
        let capture = Arc::new(Mutex::new(None));
        let token_url = spawn_json_token_server(
            "authorization_code",
            r#"{"access_token":"cb-access","refresh_token":"cb-refresh","expires_in":7200}"#
                .to_owned(),
            capture.clone(),
        )?;
        let port = free_port()?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url(token_url)
            .with_callback_port(port);

        let interaction = Arc::new(PendingManual::new());
        let login = {
            let interaction = interaction.clone();
            let oauth = oauth.clone();
            tokio::spawn(async move { oauth.login(interaction.as_ref()).await })
        };

        let auth_url = loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(url) = interaction.auth_url() {
                break url;
            }
        };
        let parsed = reqwest::Url::parse(&auth_url).map_err(|e| err(e.to_string()))?;
        let params: BTreeMap<_, _> = parsed.query_pairs().into_owned().collect();
        let state = params.get("state").cloned().ok_or_else(|| err("state"))?;

        // Bad state is rejected without settling the wait.
        let client = reqwest::Client::new();
        let bad = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=nope&state=wrong"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
        assert!(
            bad.text()
                .await
                .map_err(|e| err(e.to_string()))?
                .contains("State mismatch")
        );

        let good = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=callback-code&state={state}"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(good.status(), reqwest::StatusCode::OK);

        let credentials = tokio::time::timeout(Duration::from_secs(5), login)
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credentials.access, "cb-access");
        assert_eq!(credentials.refresh, "cb-refresh");

        let captured = lock_mutex(&capture, "capture")?
            .clone()
            .ok_or_else(|| err("captured"))?;
        assert!(captured.body.contains("\"code\":\"callback-code\""));
        assert!(interaction.prompt_was_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn login_cancel_returns_login_cancelled() -> TestResult {
        let port = free_port()?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url("http://127.0.0.1:1/unused")
            .with_callback_port(port);
        let signal = CancellationToken::new();
        let interaction = TestInteraction::with_signal(signal.clone());
        let cancel = async {
            loop {
                let prompt_signal =
                    lock_mutex(&interaction.prompt_signal_out, "prompt signal")?.clone();
                if let Some(prompt_signal) = prompt_signal {
                    signal.cancel();
                    assert!(
                        prompt_signal.is_cancelled(),
                        "parent cancellation must synchronously propagate to the active prompt child"
                    );
                    return Ok::<(), String>(());
                }
                tokio::task::yield_now().await;
            }
        };
        let (login, cancel) = tokio::join!(oauth.login(&interaction), cancel);
        cancel?;
        let err_value = expect_err(login, "cancelled")?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_cancel_returns_login_cancelled() -> TestResult {
        // Hang the token endpoint so cancellation is observed mid-request.
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            let Ok(_stream) = listener.accept() else {
                return;
            };
            thread::sleep(Duration::from_secs(30));
        });
        let token_url = format!("http://{address}/v1/oauth/token");
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url(token_url);
        let signal = CancellationToken::new();
        let signal2 = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal2.cancel();
        });
        let err_value = expect_err(
            oauth
                .refresh(
                    &OAuthCredential {
                        refresh: "r".into(),
                        access: "a".into(),
                        expires: 0,
                        extra: BTreeMap::new(),
                    },
                    Some(signal),
                )
                .await,
            "cancelled",
        )?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn callback_bind_error_is_hard_failure() -> TestResult {
        let port = free_port()?;
        let hold =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
                .await
                .map_err(|e| err(e.to_string()))?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_callback_port(port);
        let interaction = TestInteraction::with_manual(Ok("code".into()));
        let err_value = expect_err(oauth.login(&interaction).await, "bind")?;
        assert!(
            err_value
                .to_string()
                .contains("failed to bind OAuth callback")
                || err_value.to_string().contains(&port.to_string()),
            "expected hard bind error, got: {err_value}"
        );
        // Also confirm the primitive returns Bind rather than soft-fail for Anthropic's path.
        let Err(start_err) = OAuthCallbackServer::start(OAuthCallbackConfig {
            port,
            path: CALLBACK_PATH.into(),
            expected_state: "s".into(),
            success_message: "ok".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await
        else {
            return Err(err("expected bind error"));
        };
        assert!(matches!(
            start_err,
            CallbackServerError::Bind { port: p, .. } if p == port
        ));
        drop(hold);
        Ok(())
    }

    #[tokio::test]
    async fn exchange_payload_includes_required_fields() -> TestResult {
        let capture = Arc::new(Mutex::new(None));
        let token_url = spawn_json_token_server(
            "authorization_code",
            r#"{"access_token":"a","refresh_token":"r","expires_in":120}"#.to_owned(),
            capture.clone(),
        )?;
        let oauth = AnthropicOAuth::with_http(AuthHttpClient::from_client(reqwest::Client::new()))
            .with_token_url(token_url);
        let before = now_ms();
        let cred = oauth
            .exchange_authorization_code("c", "s", "v", None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.access, "a");
        assert_eq!(cred.refresh, "r");
        assert!(cred.expires <= before + 120_000 - EXPIRY_SKEW_MS + 2_000);
        assert!(cred.expires >= before + 120_000 - EXPIRY_SKEW_MS - 2_000);

        let captured = lock_mutex(&capture, "capture")?
            .clone()
            .ok_or_else(|| err("captured"))?;
        assert!(captured.body.contains("\"code\":\"c\""));
        assert!(captured.body.contains("\"state\":\"s\""));
        assert!(captured.body.contains("\"code_verifier\":\"v\""));
        assert!(
            captured
                .body
                .contains(&format!("\"redirect_uri\":\"{REDIRECT_URI}\""))
        );
        assert!(
            captured
                .body
                .contains(&format!("\"client_id\":\"{CLIENT_ID}\""))
        );
        Ok(())
    }
}
