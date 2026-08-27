//! OpenRouter OAuth PKCE flow.
//!
//! Port of `.references/pi/packages/ai/src/auth/oauth/openrouter.ts`:
//! OpenRouter exchanges an authorization code for a permanent, user-controlled
//! API key rather than an expiring access/refresh pair. The callback is a
//! one-shot loopback server on an ephemeral port with a random UUID path
//! (no `state` parameter), raced against a manual paste prompt so remote or
//! headless sessions can complete login by pasting the redirect URL.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError};
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::anthropic::parse_authorization_input;
use super::callback_server::callback_host_from_env;
use super::page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;

/// Browser authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
/// Key-exchange endpoint.
pub const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
/// Whole-login timeout for the callback wait.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// `Number.MAX_SAFE_INTEGER`, the upstream sentinel expiry for a permanent key.
pub const MAX_SAFE_INTEGER_MS: i64 = 9_007_199_254_740_991;
/// Display name for the OpenRouter OAuth handler.
pub const OAUTH_NAME: &str = "OpenRouter OAuth";
/// Selector label for the OpenRouter login option.
pub const OAUTH_LOGIN_LABEL: &str = "Sign in with OpenRouter";

/// OpenRouter OAuth flow with injectable HTTP for tests.
#[derive(Clone, Debug)]
pub struct OpenRouterOAuth {
    http: AuthHttpClient,
    authorize_url: String,
    token_url: String,
}

impl OpenRouterOAuth {
    /// Build a production OpenRouter OAuth handler.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the shared HTTP client cannot be constructed.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        })
    }

    /// Build a handler around an existing HTTP client (tests / mocks).
    #[must_use]
    pub fn with_http(http: AuthHttpClient) -> Self {
        Self {
            http,
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        }
    }

    /// Override the authorize endpoint (tests only).
    #[must_use]
    pub fn with_authorize_url(mut self, authorize_url: impl Into<String>) -> Self {
        self.authorize_url = authorize_url.into();
        self
    }

    /// Override the key-exchange endpoint (tests only).
    #[must_use]
    pub fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into();
        self
    }

    /// Shared production-ready instance behind [`OAuthAuth`].
    ///
    /// # Errors
    ///
    /// Propagates client construction failure from [`Self::new`].
    pub fn shared() -> Result<Arc<dyn OAuthAuth>, AuthError> {
        Ok(Arc::new(Self::new()?))
    }

    /// Build the browser authorization URL for a callback URL and challenge.
    #[must_use]
    pub fn authorization_url(&self, callback_url: &str, challenge: &str) -> String {
        let params = [
            ("callback_url", callback_url),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ];
        let query = params
            .iter()
            .map(|(key, value)| format!("{key}={}", urlencode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{query}", self.authorize_url)
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
        let callback_path = format!("/oauth/callback/{}", uuid::Uuid::new_v4());
        let callback = OpenRouterCallbackServer::start(
            &callback_path,
            self.http.clone(),
            self.token_url.clone(),
            pkce.verifier.clone(),
            interaction.signal(),
        )
        .await?;

        let prompt_cancel = interaction
            .signal()
            .map_or_else(CancellationToken::new, |parent| parent.child_token());

        interaction.notify(AuthEvent::Progress {
            message: format!(
                "Listening for OpenRouter OAuth callback on {}",
                callback.callback_url()
            ),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: self.authorization_url(callback.callback_url(), &pkce.challenge),
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .to_owned(),
            ),
        });

        let race_result: Result<OAuthCredential, AuthError> = tokio::select! {
            biased;
            credential = callback.wait_for_credential() => {
                prompt_cancel.cancel();
                credential.and_then(|credential| {
                    credential.ok_or_else(|| AuthError::message("Missing authorization code"))
                })
            }
            manual = manual_code_prompt(interaction, callback.callback_url(), &prompt_cancel) => {
                callback.cancel_wait().await;
                let input = manual?;
                let code = parse_authorization_input(&input)
                    .code
                    .filter(|value| !value.is_empty());
                let Some(code) = code else {
                    return Err(AuthError::message("Missing authorization code"));
                };
                interaction.notify(AuthEvent::Progress {
                    message: "Exchanging authorization code for an API key...".to_owned(),
                });
                exchange_authorization_code(
                    &self.http,
                    &self.token_url,
                    &code,
                    &pkce.verifier,
                    interaction.signal().as_ref(),
                )
                .await
            }
            () = wait_for_optional_cancel(interaction.signal()) => {
                prompt_cancel.cancel();
                callback.cancel_wait().await;
                Err(AuthError::Cancelled)
            }
        };

        // Always tear down the listener before propagating the race result.
        prompt_cancel.cancel();
        callback.close().await;
        race_result
    }
}

impl Default for OpenRouterOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            http: AuthHttpClient::from_client(reqwest::Client::new()),
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        })
    }
}

impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        OAUTH_NAME
    }

    fn login_label(&self) -> Option<&str> {
        Some(OAUTH_LOGIN_LABEL)
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { self.login_inner(interaction).await })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        // The exchanged key is permanent; there is nothing to refresh.
        Box::pin(async move { Ok(credential.clone()) })
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

async fn manual_code_prompt(
    interaction: &dyn AuthInteraction,
    callback_url: &str,
    prompt_cancel: &CancellationToken,
) -> Result<String, AuthError> {
    interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                .to_owned(),
            placeholder: Some(callback_url.to_owned()),
            signal: Some(prompt_cancel.clone()),
        })
        .await
}

async fn wait_for_optional_cancel(signal: Option<CancellationToken>) {
    if let Some(signal) = signal {
        signal.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[derive(Serialize)]
struct ExchangeBody<'a> {
    code: &'a str,
    code_verifier: &'a str,
    code_challenge_method: &'static str,
}

/// Exchange an authorization code for the permanent OpenRouter API key.
async fn exchange_authorization_code(
    http: &AuthHttpClient,
    token_url: &str,
    code: &str,
    verifier: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<OAuthCredential, AuthError> {
    let body = ExchangeBody {
        code,
        code_verifier: verifier,
        code_challenge_method: "S256",
    };

    let raw = http
        .post_json(token_url, &body, None, cancellation)
        .await
        .map_err(|error| match error {
            AuthHttpError::Cancelled => AuthError::Cancelled,
            AuthHttpError::Http { status, body, .. } => AuthError::message(format!(
                "OpenRouter OAuth key exchange failed (HTTP {status}){}",
                error_detail_suffix(&body)
            )),
            other => AuthError::message(format!(
                "OpenRouter OAuth key exchange request failed: {other}"
            )),
        })?;

    parse_exchange_body(&raw)
}

fn parse_exchange_body(raw: &str) -> Result<OAuthCredential, AuthError> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| AuthError::message("OpenRouter OAuth returned invalid JSON"))?;
    let key = parsed.get("key").and_then(Value::as_str);
    if key.is_none_or(str::is_empty) {
        return Err(AuthError::message(
            "OpenRouter OAuth response carries no \"key\"",
        ));
    }
    Ok(OAuthCredential {
        access: key.unwrap_or_default().to_owned(),
        refresh: String::new(),
        expires: MAX_SAFE_INTEGER_MS,
        extra: BTreeMap::new(),
    })
}

/// Extract the best human-readable failure detail from an error body.
fn error_detail_suffix(body: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    let detail = parsed
        .get("error_description")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("message").and_then(Value::as_str))
        .or_else(|| parsed.get("error").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            parsed
                .get("error")
                .filter(|value| value.is_object())
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    detail.map_or_else(String::new, |detail| format!(": {detail}"))
}

/// `URLSearchParams`-compatible percent encoding (space becomes `+`).
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    char::from(HEX[usize::from(value)])
}

struct CallbackShared {
    http: AuthHttpClient,
    token_url: String,
    verifier: String,
    settle: Mutex<Option<oneshot::Sender<Result<Option<OAuthCredential>, AuthError>>>>,
    claimed: AtomicBool,
    shutdown: CancellationToken,
}

impl CallbackShared {
    async fn settle(&self, value: Result<Option<OAuthCredential>, AuthError>) {
        if let Some(tx) = self.settle.lock().await.take() {
            let _ = tx.send(value);
        }
        self.shutdown.cancel();
    }

    async fn is_settled(&self) -> bool {
        self.settle.lock().await.is_none()
    }
}

/// One-shot loopback OAuth callback server on an ephemeral port.
struct OpenRouterCallbackServer {
    callback_url: String,
    shared: Arc<CallbackShared>,
    wait: Mutex<Option<oneshot::Receiver<Result<Option<OAuthCredential>, AuthError>>>>,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Debug, serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl OpenRouterCallbackServer {
    async fn start(
        path: &str,
        http: AuthHttpClient,
        token_url: String,
        verifier: String,
        signal: Option<CancellationToken>,
    ) -> Result<Self, AuthError> {
        if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(AuthError::Cancelled);
        }
        let host = callback_host_from_env();
        let listener = TcpListener::bind((host, 0_u16))
            .await
            .map_err(|error| {
                AuthError::message(format!(
                    "failed to bind OpenRouter OAuth callback on {host}: {error}"
                ))
            })?;
        let port = listener.local_addr().map_err(|error| {
            AuthError::message(format!(
                "Could not determine the OpenRouter OAuth callback port: {error}"
            ))
        })?;

        let (tx, rx) = oneshot::channel();
        let shared = Arc::new(CallbackShared {
            http,
            token_url,
            verifier,
            settle: Mutex::new(Some(tx)),
            claimed: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
        });

        let router = Router::new()
            .route(path, get(handle_callback))
            .fallback(handle_not_found)
            .with_state(Arc::clone(&shared));

        let shutdown = shared.shutdown.clone();
        let join = tokio::spawn(async move {
            let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
                shutdown.cancelled().await;
            });
            let _ = serve.await;
        });

        let timeout_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            tokio::select! {
                () = timeout_shared.shutdown.cancelled() => {}
                () = tokio::time::sleep(LOGIN_TIMEOUT) => {
                    timeout_shared
                        .settle(Err(AuthError::message("OpenRouter OAuth login timed out")))
                        .await;
                }
            }
        });

        if let Some(signal) = signal.as_ref() {
            let abort_shared = Arc::clone(&shared);
            let child = signal.child_token();
            tokio::spawn(async move {
                child.cancelled().await;
                abort_shared.settle(Err(AuthError::Cancelled)).await;
            });
        }

        Ok(Self {
            callback_url: format!("http://{host}:{port}{}", normalize_path(path)),
            shared,
            wait: Mutex::new(Some(rx)),
            join: Mutex::new(Some(join)),
        })
    }

    fn callback_url(&self) -> &str {
        &self.callback_url
    }

    /// Resolve with the credential once a browser callback completes the key
    /// exchange, or `Ok(None)` once [`Self::cancel_wait`] hands the login over
    /// to manual code entry.
    async fn wait_for_credential(&self) -> Result<Option<OAuthCredential>, AuthError> {
        let receiver = {
            let mut guard = self.wait.lock().await;
            guard.take()
        };
        match receiver {
            Some(rx) => rx
                .await
                .map_err(|_| AuthError::message("Login cancelled"))?,
            None => Ok(None),
        }
    }

    /// Hand the login over to manual code entry unless a callback already
    /// claimed the exchange.
    async fn cancel_wait(&self) {
        if !self.shared.claimed.load(Ordering::SeqCst) {
            self.shared.settle(Ok(None)).await;
        }
    }

    /// Stop listening and release timers. Settles any pending wait so no
    /// awaiting future can hang.
    async fn close(&self) {
        self.shared.settle(Ok(None)).await;
        if let Some(handle) = self.join.lock().await.take() {
            let _ = handle.await;
        }
    }
}

async fn handle_callback(
    State(shared): State<Arc<CallbackShared>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if shared.claimed.load(Ordering::SeqCst) || shared.is_settled().await {
        return (
            StatusCode::CONFLICT,
            Html(oauth_error_html(
                "This OAuth callback has already been used.",
                None,
            )),
        )
            .into_response();
    }

    if let Some(error) = query.error {
        let description = query.error_description.unwrap_or(error);
        shared
            .settle(Err(AuthError::message(format!(
                "OpenRouter authorization failed: {description}"
            ))))
            .await;
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html(
                "OpenRouter authorization was denied.",
                Some(&description),
            )),
        )
            .into_response();
    }

    let Some(code) = query.code.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html(
                "OpenRouter returned no authorization code.",
                None,
            )),
        )
            .into_response();
    };
    shared.claimed.store(true, Ordering::SeqCst);

    let exchanged = exchange_authorization_code(&shared.http, &shared.token_url, &code, &shared.verifier, None).await;
    match exchanged {
        Ok(credential) => {
            shared.settle(Ok(Some(credential))).await;
            (
                StatusCode::OK,
                Html(oauth_success_html(
                    "Signed in to OpenRouter. You may now close this page.",
                )),
            )
                .into_response()
        }
        Err(error) => {
            shared.settle(Err(error.clone())).await;
            (
                StatusCode::BAD_GATEWAY,
                Html(oauth_error_html(
                    "OpenRouter key exchange failed.",
                    Some(&error.to_string()),
                )),
            )
                .into_response()
        }
    }
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

async fn handle_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(oauth_error_html("OAuth callback route not found.", None)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;
    use crate::auth::types::AuthPrompt;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    /// Interaction with scripted prompt answers, recording events.
    struct MockInteraction {
        events: Mutex<Vec<AuthEvent>>,
        answers: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<AuthPrompt>>,
    }

    impl MockInteraction {
        fn new(answers: Vec<String>) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                answers: Mutex::new(VecDeque::from(answers)),
                prompts: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Result<Vec<AuthEvent>, String> {
            self.events
                .lock()
                .map(|guard| guard.clone())
                .map_err(|_| err("events lock poisoned"))
        }
    }

    impl AuthInteraction for MockInteraction {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            if let Ok(mut prompts) = self.prompts.lock() {
                prompts.push(prompt);
            }
            let answer = self
                .answers
                .lock()
                .ok()
                .and_then(|mut guard| guard.pop_front())
                .unwrap_or_default();
            Box::pin(async move { Ok(answer) })
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

    struct ScriptedServer {
        requests: Arc<Mutex<Vec<String>>>,
        _join: thread::JoinHandle<()>,
        base: String,
    }

    impl ScriptedServer {
        fn spawn(responses: Vec<String>) -> Result<Self, String> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
            let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_thread = Arc::clone(&requests);
            let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
            let join = thread::spawn(move || {
                loop {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    let mut buf = vec![0_u8; 16_384];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if let Ok(mut guard) = requests_thread.lock() {
                        guard.push(request);
                    }
                    let response = queue
                        .lock()
                        .ok()
                        .and_then(|mut guard| guard.pop_front())
                        .unwrap_or_else(|| http_json(500, r#"{"error":"exhausted"}"#));
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            Ok(Self {
                requests,
                _join: join,
                base: format!("http://{address}"),
            })
        }

        fn requests(&self) -> Result<Vec<String>, String> {
            self.requests
                .lock()
                .map(|guard| guard.clone())
                .map_err(|_| err("requests lock poisoned"))
        }
    }

    fn http_json(status: u16, body: &str) -> String {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            _ => "Error",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn oauth(server: &ScriptedServer) -> Result<OpenRouterOAuth, String> {
        Ok(OpenRouterOAuth::with_http(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
        )
        .with_token_url(format!("{}/api/v1/auth/keys", server.base)))
    }

    #[tokio::test]
    async fn manual_paste_exchanges_code_for_permanent_key() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{"key":"sk-or-test-key"}"#,
        )])?;
        let flow = oauth(&server)?;
        let interaction = MockInteraction::new(vec!["http://localhost:9999/callback?code=pasted-code".to_owned()]);
        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;

        assert_eq!(credential.access, "sk-or-test-key");
        assert_eq!(credential.refresh, "");
        assert_eq!(credential.expires, MAX_SAFE_INTEGER_MS);
        assert!(credential.extra.is_empty());

        let events = interaction.events()?;
        match events.as_slice() {
            [AuthEvent::Progress { message }, AuthEvent::AuthUrl { url, .. }, AuthEvent::Progress { message: exchange }] => {
                assert!(message.starts_with("Listening for OpenRouter OAuth callback on http://127.0.0.1:"), "{message}");
                assert!(url.starts_with("https://openrouter.ai/auth?callback_url=http%3A%2F%2F127.0.0.1%3A"), "{url}");
                assert!(url.contains("%2Foauth%2Fcallback%2F"), "{url}");
                assert!(url.contains("&code_challenge="), "{url}");
                assert!(url.ends_with("&code_challenge_method=S256"), "{url}");
                assert_eq!(exchange, "Exchanging authorization code for an API key...");
            }
            other => return Err(err(format!("unexpected events: {other:?}"))),
        }

        let requests = server.requests()?;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /api/v1/auth/keys"));
        assert!(requests[0].contains("\"code\":\"pasted-code\""));
        assert!(requests[0].contains("\"code_challenge_method\":\"S256\""));
        assert!(requests[0].contains("\"code_verifier\":\""));
        Ok(())
    }

    #[tokio::test]
    async fn prompt_placeholder_is_the_callback_url() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(200, r#"{"key":"k"}"#)])?;
        let flow = oauth(&server)?;
        let interaction = MockInteraction::new(vec!["plain-code".to_owned()]);
        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credential.access, "k");

        let prompts = interaction
            .prompts
            .lock()
            .map_err(|_| err("prompts lock poisoned"))?;
        match prompts.as_slice() {
            [AuthPrompt::ManualCode { message, placeholder, .. }] => {
                assert_eq!(
                    message,
                    "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                );
                let placeholder = placeholder.as_deref().unwrap_or_default();
                assert!(
                    placeholder.starts_with("http://127.0.0.1:")
                        && placeholder.contains("/oauth/callback/"),
                    "{placeholder}"
                );
            }
            other => return Err(err(format!("unexpected prompts: {other:?}"))),
        }
        Ok(())
    }

    #[tokio::test]
    async fn exchange_failure_reports_http_status_and_detail() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            400,
            r#"{"error_description":"bad code"}"#,
        )])?;
        let flow = oauth(&server)?;
        let interaction = MockInteraction::new(vec!["code-1".to_owned()]);
        let error = flow
            .login(&interaction)
            .await
            .expect_err("failed exchange must fail");
        let message = error.to_string();
        assert!(
            message.starts_with("OpenRouter OAuth key exchange failed (HTTP 400): bad code"),
            "{message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_key_is_reported() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(200, r#"{"ok":true}"#)])?;
        let flow = oauth(&server)?;
        let interaction = MockInteraction::new(vec!["code-1".to_owned()]);
        let error = flow
            .login(&interaction)
            .await
            .expect_err("missing key must fail");
        assert_eq!(
            error.to_string(),
            "OpenRouter OAuth response carries no \"key\""
        );
        Ok(())
    }

    #[tokio::test]
    async fn browser_callback_completes_login_and_replays_conflict() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            http_json(200, r#"{"key":"sk-or-cb"}"#),
            http_json(200, r#"{"key":"sk-or-cb-2"}"#),
        ])?;
        let flow = oauth(&server)?;

        // Drive only the callback server by never answering the manual prompt:
        // spawn the login, hit the callback, and let the race settle.
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        let callback_url: Arc<tokio::sync::Mutex<String>> =
            Arc::new(tokio::sync::Mutex::new(String::new()));
        let callback_url_capture = Arc::clone(&callback_url);
        let interaction = CallbackInteraction {
            tx: Mutex::new(Some(tx)),
            callback_url: callback_url_capture,
        };
        let login = tokio::spawn(async move { flow.login(&interaction).await });

        // Wait for the callback URL to be published.
        let url = {
            loop {
                let snapshot = callback_url.lock().await.clone();
                if !snapshot.is_empty() {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let first = reqwest::get(format!("{url}?code=callback-code"))
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(first.status(), 200);
        let body = first.text().await.map_err(|e| err(e.to_string()))?;
        assert!(body.contains("Signed in to OpenRouter."), "{body}");

        // A second hit on the used callback is a conflict.
        let second = reqwest::get(format!("{url}?code=callback-code"))
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(second.status(), 409);

        let credential = rx
            .await
            .map_err(|_| err("callback url channel closed"))?;
        assert_eq!(credential, "sk-or-cb");
        let credential = login
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credential.access, "sk-or-cb");
        Ok(())
    }

    /// Interaction that publishes the prompt placeholder (the callback URL)
    /// and never answers the manual prompt.
    struct CallbackInteraction {
        tx: Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
        callback_url: Arc<tokio::sync::Mutex<String>>,
    }

    impl AuthInteraction for CallbackInteraction {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            let placeholder = match &prompt {
                AuthPrompt::ManualCode { placeholder, .. } => placeholder.clone(),
                _ => None,
            };
            let _url_slot = &self.callback_url;
            let tx_slot = self.tx.lock().ok().and_then(|mut guard| guard.take());
            Box::pin(async move {
                if let Some(url) = placeholder {
                    if let Some(tx) = tx_slot {
                        let _ = tx.send(url);
                    }
                    // Wait like a user who never pastes; the callback wins.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Err(AuthError::message("prompt cancelled"))
            })
        }

        fn notify(&self, _event: AuthEvent) {}

        fn signal(&self) -> Option<CancellationToken> {
            None
        }
    }

    #[test]
    fn urlencode_matches_urlsearchparams() -> TestResult {
        assert_eq!(
            urlencode("http://127.0.0.1:9/oauth/callback/x?"),
            "http%3A%2F%2F127.0.0.1%3A9%2Foauth%2Fcallback%2Fx%3F"
        );
        assert_eq!(urlencode("a b"), "a+b");
        assert_eq!(urlencode("~-_.*"), "~-_.*");
        Ok(())
    }

    #[test]
    fn refresh_and_to_auth_use_the_permanent_key() -> TestResult {
        let flow = OpenRouterOAuth::default();
        let credential = OAuthCredential {
            access: "sk-or-key".to_owned(),
            refresh: String::new(),
            expires: MAX_SAFE_INTEGER_MS,
            extra: BTreeMap::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| err(e.to_string()))?;
        runtime.block_on(async move {
            let refreshed = flow
                .refresh(&credential, None)
                .await
                .map_err(|e| err(e.to_string()))?;
            assert_eq!(refreshed.access, "sk-or-key");
            let auth = flow
                .to_auth(&credential)
                .await
                .map_err(|e| err(e.to_string()))?;
            assert_eq!(auth.api_key.as_deref(), Some("sk-or-key"));
            Ok(())
        })
    }
}
