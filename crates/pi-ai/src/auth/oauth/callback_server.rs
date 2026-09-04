//! Loopback OAuth callback listener.
//!
//! Binds a fixed host/port/path, validates `state` before yielding the
//! authorization code, serves success/error HTML, and shuts down cleanly.
//! Callers race [`OAuthCallbackServer::wait_for_code`] against a manual-code
//! future they own via [`race_callback_and_manual`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::page::{oauth_error_html, oauth_success_html};

/// Environment variable that overrides the loopback bind host.
pub const OAUTH_CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";

/// Default bind host when [`OAUTH_CALLBACK_HOST_ENV`] is unset.
pub const DEFAULT_CALLBACK_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Successful authorization-code delivery from the browser callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthCallbackCode {
    /// Authorization code from the query string.
    pub code: String,
    /// Echoed state parameter (already validated against the expected value).
    pub state: String,
}

/// Configuration for a single-shot loopback callback server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthCallbackConfig {
    /// TCP port to bind.
    pub port: u16,
    /// Exact request path (for example `/callback` or `/auth/callback`).
    pub path: String,
    /// Expected OAuth `state` value; mismatched callbacks are rejected.
    pub expected_state: String,
    /// Message shown on the success HTML page.
    pub success_message: String,
    /// Optional override for the bind host. When `None`, uses
    /// [`callback_host_from_env`].
    pub host: Option<IpAddr>,
}

#[derive(Clone)]
struct SettleSlot {
    tx: Arc<Mutex<Option<oneshot::Sender<Option<OAuthCallbackCode>>>>>,
    settled: Arc<AtomicBool>,
}

impl SettleSlot {
    fn new() -> (Self, oneshot::Receiver<Option<OAuthCallbackCode>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                tx: Arc::new(Mutex::new(Some(tx))),
                settled: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    fn pre_settled_none() -> (Self, oneshot::Receiver<Option<OAuthCallbackCode>>) {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(None);
        (
            Self {
                tx: Arc::new(Mutex::new(None)),
                settled: Arc::new(AtomicBool::new(true)),
            },
            rx,
        )
    }

    async fn settle(&self, value: Option<OAuthCallbackCode>) {
        if self
            .settled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send(value);
        }
    }
}

/// Running loopback callback server.
pub struct OAuthCallbackServer {
    host: IpAddr,
    port: u16,
    path: String,
    wait: Mutex<Option<oneshot::Receiver<Option<OAuthCallbackCode>>>>,
    settle: SettleSlot,
    shutdown: CancellationToken,
    join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    soft_failed: bool,
}

/// Failure starting the callback listener.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CallbackServerError {
    /// The requested port could not be bound.
    #[error("failed to bind OAuth callback on {host}:{port}: {message}")]
    Bind {
        /// Bind host.
        host: IpAddr,
        /// Bind port.
        port: u16,
        /// Underlying I/O error text.
        message: String,
    },
    /// Defensive guard: the compiled-in default host is not loopback.
    #[error("OAuth callback host must be loopback by default; refusing {host}")]
    NonLoopbackDefault {
        /// Rejected host.
        host: IpAddr,
    },
}

impl From<CallbackServerError> for AuthError {
    fn from(value: CallbackServerError) -> Self {
        Self::message(value.to_string())
    }
}

/// Resolve the callback bind host: `PI_OAUTH_CALLBACK_HOST` or `127.0.0.1`.
#[must_use]
pub fn callback_host_from_env() -> IpAddr {
    parse_callback_host(std::env::var(OAUTH_CALLBACK_HOST_ENV).ok().as_deref())
}

/// Parse a host override, falling back to [`DEFAULT_CALLBACK_HOST`].
#[must_use]
pub fn parse_callback_host(value: Option<&str>) -> IpAddr {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<IpAddr>().ok())
        .unwrap_or(DEFAULT_CALLBACK_HOST)
}

/// True when the default resolution path (no override) is loopback-only.
#[must_use]
pub fn default_callback_host_is_loopback() -> bool {
    DEFAULT_CALLBACK_HOST.is_loopback() && parse_callback_host(None).is_loopback()
}

impl OAuthCallbackServer {
    /// Bind and start serving the callback route.
    ///
    /// # Errors
    ///
    /// Returns [`CallbackServerError::Bind`] when the port is already in use or
    /// cannot be opened. Providers that soft-fail on bind should call
    /// [`Self::start_soft`] instead.
    pub async fn start(config: OAuthCallbackConfig) -> Result<Self, CallbackServerError> {
        let host = config.host.unwrap_or_else(callback_host_from_env);
        // Defaults are always loopback. Explicit non-loopback is only possible
        // via `PI_OAUTH_CALLBACK_HOST` / `config.host` and is intentional.
        if config.host.is_none()
            && std::env::var_os(OAUTH_CALLBACK_HOST_ENV).is_none()
            && !host.is_loopback()
        {
            return Err(CallbackServerError::NonLoopbackDefault { host });
        }

        let addr = SocketAddr::new(host, config.port);
        let listener =
            TcpListener::bind(addr)
                .await
                .map_err(|error| CallbackServerError::Bind {
                    host,
                    port: config.port,
                    message: error.to_string(),
                })?;
        let bound = listener
            .local_addr()
            .map_err(|error| CallbackServerError::Bind {
                host,
                port: config.port,
                message: error.to_string(),
            })?;

        let (settle, rx) = SettleSlot::new();
        let shutdown = CancellationToken::new();
        let path = normalize_path(&config.path);

        let state = CallbackState {
            expected_state: config.expected_state,
            success_message: config.success_message,
            settle: settle.clone(),
            shutdown: shutdown.clone(),
        };

        let app = Router::new()
            .route(&path, get(handle_callback))
            .fallback(handle_not_found)
            .with_state(state);

        let shutdown_serve = shutdown.clone();
        let join = tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
                shutdown_serve.cancelled().await;
            });
            let _ = serve.await;
        });

        Ok(Self {
            host: bound.ip(),
            port: bound.port(),
            path,
            wait: Mutex::new(Some(rx)),
            settle,
            shutdown,
            join: StdMutex::new(Some(join)),
            soft_failed: false,
        })
    }

    /// Start the server, mapping bind failures into a soft-failed server whose
    /// [`Self::wait_for_code`] resolves to `None` so manual paste can proceed.
    pub async fn start_soft(config: OAuthCallbackConfig) -> Self {
        match Self::start(config).await {
            Ok(server) => server,
            Err(_) => Self::soft_failed_placeholder(),
        }
    }

    fn soft_failed_placeholder() -> Self {
        let (settle, rx) = SettleSlot::pre_settled_none();
        Self {
            host: DEFAULT_CALLBACK_HOST,
            port: 0,
            path: String::new(),
            wait: Mutex::new(Some(rx)),
            settle,
            shutdown: CancellationToken::new(),
            join: StdMutex::new(None),
            soft_failed: true,
        }
    }

    /// Bind host actually used by the listener.
    #[must_use]
    pub fn host(&self) -> IpAddr {
        self.host
    }

    /// Bind port actually used by the listener.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Callback path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether start soft-failed (port in use / bind error).
    #[must_use]
    pub fn is_soft_failed(&self) -> bool {
        self.soft_failed
    }

    /// Wait until a valid code arrives, the wait is cancelled, or the server
    /// soft-failed. Returns `None` when cancelled or soft-failed.
    pub async fn wait_for_code(&self) -> Option<OAuthCallbackCode> {
        let receiver = {
            let mut guard = self.wait.lock().await;
            guard.take()
        };
        match receiver {
            Some(rx) => rx.await.ok().flatten(),
            None => None,
        }
    }

    /// Resolve the pending wait with `None` without requiring a code.
    ///
    /// Used when a caller-controlled manual-code future wins the race.
    pub async fn cancel_wait(&self) {
        self.settle.settle(None).await;
    }

    /// Cancel the wait and stop the HTTP listener.
    pub async fn close(self) {
        self.settle.settle(None).await;
        self.shutdown.cancel();
        let handle = self
            .join
            .lock()
            .map_or_else(|e| e.into_inner().take(), |mut guard| guard.take());
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}
impl Drop for OAuthCallbackServer {
    fn drop(&mut self) {
        // Signal graceful shutdown; then, if the listener task has not already
        // been awaited by [`close`], abort it so the task and its TcpListener
        // are released instead of being detached.
        self.shutdown.cancel();
        let handle = self
            .join
            .lock()
            .map_or_else(|e| e.into_inner().take(), |mut guard| guard.take());
        if let Some(handle) = handle {
            handle.abort();
        }
    }
}

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    success_message: String,
    settle: SettleSlot,
    shutdown: CancellationToken,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn handle_callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        let details = query
            .error_description
            .unwrap_or_else(|| format!("Error: {error}"));
        // Provider error is terminal for this attempt.
        state.settle.settle(None).await;
        schedule_shutdown(state.shutdown.clone());
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html(
                "Authentication did not complete.",
                Some(&details),
            )),
        )
            .into_response();
    }

    let Some(code) = query.code.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html("Missing authorization code.", None)),
        )
            .into_response();
    };

    let Some(state_value) = query.state.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html("Missing OAuth state.", None)),
        )
            .into_response();
    };

    if state_value != state.expected_state {
        // Reject without settling so a later correct callback can still win,
        // matching the reference servers that only settle on success.
        return (
            StatusCode::BAD_REQUEST,
            Html(oauth_error_html("State mismatch.", None)),
        )
            .into_response();
    }

    let payload = OAuthCallbackCode {
        code,
        state: state_value,
    };
    state.settle.settle(Some(payload)).await;
    schedule_shutdown(state.shutdown.clone());
    (
        StatusCode::OK,
        Html(oauth_success_html(&state.success_message)),
    )
        .into_response()
}

async fn handle_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(oauth_error_html("Callback route not found.", None)),
    )
        .into_response()
}

fn schedule_shutdown(shutdown: CancellationToken) {
    tokio::spawn(async move {
        // Allow the success/error HTML response to flush first.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        shutdown.cancel();
    });
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Race the callback wait against a caller-provided manual-code future.
///
/// The first side to produce a value wins. When the manual future completes,
/// the callback wait is cancelled via [`OAuthCallbackServer::cancel_wait`].
///
/// # Errors
///
/// Propagates errors from the manual future. The callback path itself yields
/// `Ok(None)` on cancel/soft-fail rather than an error.
pub async fn race_callback_and_manual<F, E>(
    server: &OAuthCallbackServer,
    manual: F,
) -> Result<Option<OAuthCallbackCode>, E>
where
    F: Future<Output = Result<Option<OAuthCallbackCode>, E>>,
{
    tokio::select! {
        callback = server.wait_for_code() => Ok(callback),
        manual = manual => {
            server.cancel_wait().await;
            manual
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    type TestResult = Result<(), String>;

    fn err(msg: impl Into<String>) -> String {
        msg.into()
    }

    fn free_port() -> Result<u16, String> {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        Ok(listener
            .local_addr()
            .map_err(|e| err(e.to_string()))?
            .port())
    }

    #[test]
    fn default_host_is_loopback_and_not_all_interfaces() {
        assert!(default_callback_host_is_loopback());
        assert_eq!(parse_callback_host(None), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(parse_callback_host(None), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        // Override may be non-loopback when the operator sets the env/config,
        // but the compiled-in default never is.
        assert!(parse_callback_host(Some("0.0.0.0")).is_unspecified());
        assert_eq!(
            parse_callback_host(Some("127.0.0.1")),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[tokio::test]
    async fn rejects_bad_state_and_accepts_good_state() -> TestResult {
        let port = free_port()?;
        let server = OAuthCallbackServer::start(OAuthCallbackConfig {
            port,
            path: "/callback".into(),
            expected_state: "expected-state".into(),
            success_message: "ok".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await
        .map_err(|e| err(e.to_string()))?;

        let client = reqwest::Client::new();
        let bad = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=abc&state=wrong"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
        let bad_body = bad.text().await.map_err(|e| err(e.to_string()))?;
        assert!(bad_body.contains("State mismatch"));

        // Bad state must not settle the wait; a good callback should still win.
        let good = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=good-code&state=expected-state"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(good.status(), reqwest::StatusCode::OK);
        let page = good.text().await.map_err(|e| err(e.to_string()))?;
        assert!(page.contains("Authentication successful"));

        let code = tokio::time::timeout(Duration::from_secs(2), server.wait_for_code())
            .await
            .map_err(|_| err("timeout"))?
            .ok_or_else(|| err("code"))?;
        assert_eq!(code.code, "good-code");
        assert_eq!(code.state, "expected-state");
        server.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn port_in_use_returns_bind_error_and_soft_fail_is_none() -> TestResult {
        let port = free_port()?;
        let hold = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(|e| err(e.to_string()))?;
        let start_result = OAuthCallbackServer::start(OAuthCallbackConfig {
            port,
            path: "/callback".into(),
            expected_state: "s".into(),
            success_message: "ok".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await;
        let Err(err_value) = start_result else {
            return Err(err("expected port-in-use bind error"));
        };
        assert!(matches!(
            err_value,
            CallbackServerError::Bind { port: p, .. } if p == port
        ));

        let soft = OAuthCallbackServer::start_soft(OAuthCallbackConfig {
            port,
            path: "/callback".into(),
            expected_state: "s".into(),
            success_message: "ok".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await;
        assert!(soft.is_soft_failed());
        assert!(soft.wait_for_code().await.is_none());
        soft.close().await;
        drop(hold);
        Ok(())
    }

    #[tokio::test]
    async fn race_manual_code_wins_and_cancels_callback_wait() -> TestResult {
        let port = free_port()?;
        let server = OAuthCallbackServer::start(OAuthCallbackConfig {
            port,
            path: "/auth/callback".into(),
            expected_state: "state".into(),
            success_message: "done".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await
        .map_err(|e| err(e.to_string()))?;

        let manual = async {
            Ok::<_, AuthError>(Some(OAuthCallbackCode {
                code: "manual".into(),
                state: "state".into(),
            }))
        };
        let winner = race_callback_and_manual(&server, manual)
            .await
            .map_err(|e| err(e.to_string()))?
            .ok_or_else(|| err("code"))?;
        assert_eq!(winner.code, "manual");
        server.close().await;
        Ok(())
    }
    #[tokio::test]
    async fn drop_releases_listener_and_allows_rebind() -> TestResult {
        let server = OAuthCallbackServer::start(OAuthCallbackConfig {
            port: 0,
            path: "/callback".into(),
            expected_state: "s".into(),
            success_message: "ok".into(),
            host: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        })
        .await
        .map_err(|e| err(e.to_string()))?;

        let bound = SocketAddr::new(server.host(), server.port());
        assert_ne!(server.port(), 0);

        // Dropping the server must stop the listener and free the bound address.
        drop(server);

        let mut last_err = None;
        let mut rebound = None;
        for _ in 0..40 {
            match tokio::net::TcpListener::bind(bound).await {
                Ok(listener) => {
                    rebound = Some(listener);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        let _listener = rebound.ok_or_else(|| {
            err(format!(
                "address {bound} still in use after drop: {last_err:?}"
            ))
        })?;
        Ok(())
    }
}
