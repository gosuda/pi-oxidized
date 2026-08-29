//! OpenRouter OAuth PKCE flow.
//!
//! Ports `.references/pi/packages/ai/src/auth/oauth/openrouter.ts`: PKCE
//! browser flow with an ephemeral callback server on a random UUID path,
//! manual-code race, JSON token exchange returning a permanent API key,
//! and no-op refresh.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError};
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::callback_server::callback_host_from_env;
use super::page::{oauth_error_html, oauth_success_html};
use super::pkce::generate_pkce;

/// Browser authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";

/// Token exchange endpoint.
pub const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

/// Login timeout (5 minutes).
pub const LOGIN_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// Display name for the OpenRouter OAuth handler.
pub const OAUTH_NAME: &str = "OpenRouter OAuth";

/// Selector label for the subscription login option.
pub const OAUTH_LOGIN_LABEL: &str = "Sign in with OpenRouter";

/// `Number.MAX_SAFE_INTEGER` from JavaScript — OpenRouter keys never expire.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// OpenRouter OAuth handler.
#[derive(Clone, Debug)]
pub struct OpenRouterOAuth {
    http: AuthHttpClient,
    authorize_url: String,
    token_url: String,
}

impl OpenRouterOAuth {
    /// Build with production endpoints and a fresh HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the underlying HTTP client cannot be built.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        })
    }

    /// Build with explicit endpoints (tests / mocks).
    #[must_use]
    pub fn with_endpoints(
        http: AuthHttpClient,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
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

    async fn exchange_authorization_code(
        &self,
        code: &str,
        verifier: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let body = ExchangeRequest {
            code: code.to_owned(),
            code_verifier: verifier.to_owned(),
            code_challenge_method: "S256".to_owned(),
        };
        let raw = self
            .http
            .post_json(&self.token_url, &body, None, signal)
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| AuthError::message("OpenRouter OAuth returned invalid JSON"))?;
        let key = parsed
            .get("key")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AuthError::message("OpenRouter OAuth response carries no \"key\""))?;
        Ok(OAuthCredential {
            access: key.to_owned(),
            refresh: String::new(),
            expires: MAX_SAFE_INTEGER,
            extra: BTreeMap::new(),
        })
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
        Box::pin(async move {
            let signal = interaction.signal();
            let pkce = generate_pkce().map_err(|e| AuthError::message(e.to_string()))?;
            let verifier = pkce.verifier;
            let challenge = pkce.challenge;
            let callback_path = format!("/oauth/callback/{}", Uuid::new_v4());
            let callback_host = callback_host_from_env();

            let listener = TcpListener::bind(SocketAddr::new(callback_host, 0))
                .await
                .map_err(|e| {
                    AuthError::message(format!("Failed to bind OpenRouter callback: {e}"))
                })?;
            let port = listener
                .local_addr()
                .map_err(|e| AuthError::message(format!("Failed to get callback port: {e}")))?
                .port();
            let callback_url = format!("http://{callback_host}:{port}{callback_path}");

            let (code_tx, code_rx) = oneshot::channel::<Option<String>>();
            let code_tx = Arc::new(Mutex::new(Some(code_tx)));
            let claimed = Arc::new(Mutex::new(false));
            let shutdown = CancellationToken::new();
            let shutdown_outer = shutdown.clone();
            let timeout_shutdown = shutdown.clone();
            let timeout_code_tx = code_tx.clone();

            let timeout_handle = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(LOGIN_TIMEOUT_MS)).await;
                timeout_shutdown.cancel();
                if let Some(tx) = timeout_code_tx.lock().await.take() {
                    let _ = tx.send(None);
                }
            });

            let callback_path_clone = callback_path.clone();
            let callback_claimed = claimed.clone();
            let callback_code_tx = code_tx.clone();
            let callback_shutdown = shutdown.clone();

            let server_handle = tokio::spawn(async move {
                use axum::Router;
                use axum::extract::{Query, State};
                use axum::http::StatusCode;
                use axum::response::{Html, IntoResponse, Response};
                use axum::routing::any;

                #[derive(Deserialize)]
                struct CallbackQuery {
                    code: Option<String>,
                    error: Option<String>,
                    error_description: Option<String>,
                }

                #[derive(Clone)]
                struct CbState {
                    code_tx: Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>,
                    claimed: Arc<Mutex<bool>>,
                    shutdown: CancellationToken,
                }

                async fn handle_callback(
                    State(state): State<CbState>,
                    Query(query): Query<CallbackQuery>,
                ) -> Response {
                    if let Some(error) = query.error {
                        let description = query
                            .error_description
                            .unwrap_or_else(|| format!("Error: {error}"));
                        if let Some(tx) = state.code_tx.lock().await.take() {
                            let _ = tx.send(None);
                        }
                        state.shutdown.cancel();
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

                    let mut claimed = state.claimed.lock().await;
                    if *claimed {
                        return (
                            StatusCode::CONFLICT,
                            Html(oauth_error_html(
                                "This OAuth callback has already been used.",
                                None,
                            )),
                        )
                            .into_response();
                    }
                    *claimed = true;
                    drop(claimed);

                    if let Some(tx) = state.code_tx.lock().await.take() {
                        let _ = tx.send(Some(code));
                    }
                    state.shutdown.cancel();
                    (
                        StatusCode::OK,
                        Html(oauth_success_html(
                            "Signed in to OpenRouter. You may now close this page.",
                        )),
                    )
                        .into_response()
                }

                let app = Router::new()
                    .route(&callback_path_clone, any(handle_callback))
                    .fallback(|| async {
                        (
                            StatusCode::NOT_FOUND,
                            Html(oauth_error_html("OAuth callback route not found.", None)),
                        )
                    })
                    .with_state(CbState {
                        code_tx: callback_code_tx,
                        claimed: callback_claimed,
                        shutdown: callback_shutdown,
                    });

                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        shutdown.cancelled().await;
                    })
                    .await
                    .ok();
            });

            interaction.notify(AuthEvent::Progress {
                message: format!("Listening for OpenRouter OAuth callback on {callback_url}"),
            });

            let authorize_url = format!(
                "{}?callback_url={}&code_challenge={}&code_challenge_method=S256",
                self.authorize_url,
                urlencode(&callback_url),
                challenge,
            );
            interaction.notify(AuthEvent::AuthUrl {
                url: authorize_url,
                instructions: Some(
                    "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.".to_owned(),
                ),
            });

            let manual_cancel = CancellationToken::new();
            let manual_cancel_clone = manual_cancel.clone();
            let manual = async {
                let result = interaction
                    .prompt(AuthPrompt::ManualCode {
                        message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:".to_owned(),
                        placeholder: Some(callback_url.clone()),
                        signal: Some(manual_cancel_clone),
                    })
                    .await;
                result
            };

            let callback_result = tokio::select! {
                biased;
                code = code_rx => {
                    manual_cancel.cancel();
                    code.ok().flatten()
                }
                input = manual => {
                    if let Some(tx) = code_tx.lock().await.take() {
                        let _ = tx.send(None);
                    }
                    let input = input?;
                    let code = parse_authorization_input(&input);
                    Some(code)
                }
                () = wait_for_cancel(signal.clone()) => {
                    manual_cancel.cancel();
                    shutdown_outer.cancel();
                    return Err(AuthError::Cancelled);
                }
            };

            timeout_handle.abort();
            server_handle.abort();
            shutdown_outer.cancel();

            let code = match callback_result {
                Some(code) if !code.is_empty() => code,
                _ => return Err(AuthError::message("Missing authorization code")),
            };

            interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for an API key...".to_owned(),
            });

            self.exchange_authorization_code(&code, &verifier, signal.as_ref())
                .await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
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

#[derive(Serialize)]
struct ExchangeRequest {
    code: String,
    code_verifier: String,
    code_challenge_method: String,
}

async fn wait_for_cancel(signal: Option<CancellationToken>) {
    if let Some(signal) = signal {
        signal.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Parse a pasted authorization redirect URL, query string, or bare code.
fn parse_authorization_input(input: &str) -> String {
    let value = input.trim();
    if value.is_empty() {
        return String::new();
    }

    if let Ok(url) = reqwest::Url::parse(value) {
        if let Some(code) = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, v)| v.to_string())
        {
            return code;
        }
    }

    if value.contains("code=") {
        if let Ok(url) = reqwest::Url::parse(&format!("http://localhost/?{value}")) {
            if let Some(code) = url
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, v)| v.to_string())
            {
                return code;
            }
        }
    }

    value.to_owned()
}

fn urlencode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}
