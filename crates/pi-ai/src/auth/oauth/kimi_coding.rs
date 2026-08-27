//! Kimi Code (subscription) OAuth flow.
//!
//! Port of `.references/pi/packages/ai/src/auth/oauth/kimi-coding.ts`: RFC 8628
//! device authorization grant against `https://auth.kimi.com` with JSON
//! responses, `KIMI_CODE_OAUTH_HOST`/`KIMI_OAUTH_HOST` host overrides, no
//! expiry skew, and retrying token refresh with exponential backoff.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use reqwest::Url;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError, AuthHttpResponse};
use super::super::types::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};

/// Fixed public OAuth client id for pi.
pub const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
/// Production OAuth host.
pub const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
/// Device-code lifetime fallback when the response omits `expires_in`.
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
/// Poll interval fallback when the response omits `interval`.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// Refresh retries after the initial attempt.
pub const REFRESH_MAX_RETRIES: u32 = 3;
/// Initial refresh backoff; doubles each retry.
pub const REFRESH_BACKOFF_MS: u64 = 1000;
/// Display name for the Kimi Code subscription OAuth handler.
pub const OAUTH_NAME: &str = "Kimi Code (subscription)";
/// Selector label for the subscription login option.
pub const OAUTH_LOGIN_LABEL: &str = "Sign in with Kimi Code";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Parsed device-code response from Kimi Code.
#[derive(Clone, Debug, Eq, PartialEq)]
struct KimiDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    interval_seconds: Option<u64>,
    expires_in_seconds: u64,
}

/// Kimi Code (subscription) OAuth handler.
#[derive(Clone, Debug)]
pub struct KimiCodingOAuth {
    http: AuthHttpClient,
    oauth_host: String,
}

impl KimiCodingOAuth {
    /// Build with the production OAuth host (env overrides honored) and a
    /// fresh HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the underlying HTTP client cannot be built.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            oauth_host: kimi_oauth_host_from_env(),
        })
    }

    /// Build with an explicit OAuth host (tests / mocks).
    #[must_use]
    pub fn with_endpoints(http: AuthHttpClient, oauth_host: impl Into<String>) -> Self {
        Self {
            http,
            oauth_host: normalize_oauth_host(&oauth_host.into()),
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

    async fn start_device_authorization(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<KimiDeviceCode, AuthError> {
        let mut fields = BTreeMap::new();
        fields.insert("client_id".to_owned(), KIMI_CLIENT_ID.to_owned());

        let response = self
            .http
            .post_form(
                &device_authorization_url(&self.oauth_host),
                &fields,
                None,
                signal,
            )
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        if !response.ok {
            return Err(AuthError::message(format!(
                "Kimi Code device authorization failed with status {}{}",
                response.status,
                status_detail(&response)
            )));
        }
        parse_device_code(&response.body)
    }

    async fn poll_for_token(
        &self,
        device: KimiDeviceCode,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let token_url = token_url(&self.oauth_host);
        let http = self.http.clone();
        let device_code = device.device_code.clone();
        let poll_signal = signal.clone();

        let mut options = OAuthDeviceCodePollOptions::new(move || {
            let token_url = token_url.clone();
            let http = http.clone();
            let device_code = device_code.clone();
            let poll_signal = poll_signal.clone();
            async move {
                let mut fields = BTreeMap::new();
                fields.insert("client_id".to_owned(), KIMI_CLIENT_ID.to_owned());
                fields.insert("device_code".to_owned(), device_code);
                fields.insert("grant_type".to_owned(), DEVICE_GRANT_TYPE.to_owned());

                let response = http
                    .post_form(&token_url, &fields, None, poll_signal.as_ref())
                    .await
                    .map_err(AuthHttpError::into_auth_error)?;

                if response.status >= 500 {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "Kimi Code device token request failed with status {}{}",
                            response.status,
                            status_detail(&response)
                        ),
                    });
                }

                if response.ok
                    && response
                        .body
                        .get("access_token")
                        .is_some_and(Value::is_string)
                {
                    return match credentials_from_token_response(&response.body, now_ms(), "poll")
                    {
                        Ok(value) => Ok(OAuthDeviceCodePollResult::Complete { value }),
                        Err(error) => Ok(OAuthDeviceCodePollResult::Failed {
                            message: error.to_string(),
                        }),
                    };
                }

                let error = response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let description = response
                    .body
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match error {
                    "authorization_pending" => Ok(OAuthDeviceCodePollResult::Pending),
                    "slow_down" => {
                        let interval_seconds = positive_number(&response.body, "interval");
                        Ok(OAuthDeviceCodePollResult::SlowDown { interval_seconds })
                    }
                    "expired_token" => Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code device authorization expired. Please restart login."
                            .to_owned(),
                    }),
                    "access_denied" => Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code login was denied.".to_owned(),
                    }),
                    _ => Ok(OAuthDeviceCodePollResult::Failed {
                        message: format!(
                            "Kimi Code device token request failed (status {}){}",
                            response.status,
                            error_detail_suffix(error, description),
                        ),
                    }),
                }
            }
        });
        options.interval_seconds = device.interval_seconds;
        options.expires_in_seconds = Some(device.expires_in_seconds);
        options.wait_before_first_poll = true;
        options.signal = signal;

        poll_oauth_device_code_flow(options).await
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let mut last_error: Option<AuthError> = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(REFRESH_BACKOFF_MS * (1 << (attempt - 1)));
                tokio::time::sleep(delay).await;
            }
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(AuthError::message("Kimi Code token refresh aborted"));
            }

            let mut fields = BTreeMap::new();
            fields.insert("client_id".to_owned(), KIMI_CLIENT_ID.to_owned());
            fields.insert("grant_type".to_owned(), "refresh_token".to_owned());
            fields.insert("refresh_token".to_owned(), refresh_token.to_owned());

            let response = match self
                .http
                .post_form(&token_url(&self.oauth_host), &fields, None, signal)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(AuthHttpError::into_auth_error(error));
                    continue;
                }
            };

            if response.ok {
                return credentials_from_token_response(&response.body, now_ms(), "refresh");
            }

            // Unauthorized: the stored credential is dead; the caller clears
            // it and prompts re-login.
            let error = response
                .body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if response.status == 401 || response.status == 403 || error == "invalid_grant" {
                let description = response
                    .body
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                return Err(AuthError::message(format!(
                    "Kimi Code token refresh unauthorized (status {}){}",
                    response.status,
                    if description.is_empty() {
                        String::new()
                    } else {
                        format!(": {description}")
                    }
                )));
            }

            if (response.status == 429 || response.status >= 500)
                && attempt < REFRESH_MAX_RETRIES
            {
                last_error = Some(AuthError::message(format!(
                    "Kimi Code token refresh failed with status {}",
                    response.status
                )));
                continue;
            }

            return Err(AuthError::message(format!(
                "Kimi Code token refresh failed with status {}{}",
                response.status,
                status_detail(&response)
            )));
        }
        Err(
            last_error.unwrap_or_else(|| AuthError::message("Kimi Code token refresh failed")),
        )
    }
}

impl Default for KimiCodingOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            http: AuthHttpClient::from_client(reqwest::Client::new()),
            oauth_host: DEFAULT_OAUTH_HOST.to_owned(),
        })
    }
}

impl OAuthAuth for KimiCodingOAuth {
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
            let device = self.start_device_authorization(signal.as_ref()).await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri_complete.clone(),
                interval_seconds: device.interval_seconds,
                expires_in_seconds: Some(device.expires_in_seconds),
            });
            self.poll_for_token(device, signal).await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.refresh_token(&credential.refresh, signal.as_ref())
                .await
        })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        Box::pin(async move {
            let mut headers = BTreeMap::new();
            headers.insert(
                "Authorization".to_owned(),
                Some(format!("Bearer {}", credential.access)),
            );
            Ok(ModelAuth {
                api_key: None,
                headers: Some(headers),
                base_url: None,
            })
        })
    }
}

fn device_authorization_url(oauth_host: &str) -> String {
    format!("{oauth_host}/api/oauth/device_authorization")
}

fn token_url(oauth_host: &str) -> String {
    format!("{oauth_host}/api/oauth/token")
}

/// Resolve the OAuth host from `KIMI_CODE_OAUTH_HOST`/`KIMI_OAUTH_HOST`.
pub fn kimi_oauth_host_from_env() -> String {
    let override_value = std::env::var("KIMI_CODE_OAUTH_HOST")
        .ok()
        .or_else(|| std::env::var("KIMI_OAUTH_HOST").ok())
        .filter(|value| !value.is_empty());
    normalize_oauth_host(&override_value.unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_owned()))
}

fn normalize_oauth_host(host: &str) -> String {
    host.trim_end_matches('/').to_owned()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn positive_number(body: &Value, field: &str) -> Option<u64> {
    body.get(field).and_then(|value| {
        if let Some(number) = value.as_u64() {
            return Some(number);
        }
        let number = value.as_f64()?;
        if !number.is_finite() || number <= 0.0 || number.fract() != 0.0 {
            return None;
        }
        u64::try_from(number as i64).ok()
    })
}

/// The verification URI is opened in the user's browser; only http(s) URLs
/// are trusted.
fn trusted_http_url(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return None;
    }
    Some(url.to_string())
}

fn parse_device_code(body: &Value) -> Result<KimiDeviceCode, AuthError> {
    let invalid =
        || AuthError::message(format!("Invalid Kimi Code device authorization response: {body}"));
    let device_code = body.get("device_code").and_then(Value::as_str);
    let user_code = body.get("user_code").and_then(Value::as_str);
    let verification_uri = body.get("verification_uri").and_then(Value::as_str);
    let verification_uri_complete = body
        .get("verification_uri_complete")
        .and_then(Value::as_str);
    if device_code.unwrap_or_default().is_empty()
        || user_code.unwrap_or_default().is_empty()
        || verification_uri.unwrap_or_default().is_empty()
        || verification_uri_complete.unwrap_or_default().is_empty()
        || verification_uri_complete.is_none_or(|raw| trusted_http_url(raw).is_none())
        || verification_uri.is_none_or(|raw| trusted_http_url(raw).is_none())
    {
        return Err(invalid());
    }
    let interval_seconds = positive_number(body, "interval");
    let expires_in_seconds =
        positive_number(body, "expires_in").unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS);
    Ok(KimiDeviceCode {
        device_code: device_code.unwrap_or_default().to_owned(),
        user_code: user_code.unwrap_or_default().to_owned(),
        verification_uri: verification_uri.unwrap_or_default().to_owned(),
        verification_uri_complete: verification_uri_complete.unwrap_or_default().to_owned(),
        interval_seconds,
        expires_in_seconds,
    })
}

fn credentials_from_token_response(
    body: &Value,
    now: i64,
    operation: &str,
) -> Result<OAuthCredential, AuthError> {
    let access = body.get("access_token").and_then(Value::as_str);
    let refresh = body.get("refresh_token").and_then(Value::as_str);
    let expires_in = positive_number(body, "expires_in");
    if access.is_none_or(str::is_empty)
        || refresh.is_none_or(str::is_empty)
        || expires_in.is_none()
    {
        return Err(AuthError::message(format!(
            "Kimi Code token {operation} response missing fields: {body}"
        )));
    }
    let lifetime_ms = i64::try_from(expires_in.unwrap_or(0).saturating_mul(1000)).unwrap_or(i64::MAX);
    Ok(OAuthCredential {
        access: access.unwrap_or_default().to_owned(),
        refresh: refresh.unwrap_or_default().to_owned(),
        expires: now.saturating_add(lifetime_ms),
        extra: BTreeMap::new(),
    })
}

fn status_detail(response: &AuthHttpResponse) -> String {
    if response.raw_body.is_empty() {
        String::new()
    } else {
        format!(": {}", response.raw_body)
    }
}

fn error_detail_suffix(error: &str, description: &str) -> String {
    match (error.is_empty(), description.is_empty()) {
        (true, _) => String::new(),
        (false, true) => format!(": {error}"),
        (false, false) => format!(": {error}: {description}"),
    }
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

    struct MockInteraction {
        events: Mutex<Vec<AuthEvent>>,
    }

    impl MockInteraction {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
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
        fn prompt(&self, _prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::message("unexpected prompt")) })
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
            401 => "Unauthorized",
            403 => "Forbidden",
            429 => "Too Many Requests",
            _ => "Error",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn oauth(server: &ScriptedServer) -> Result<KimiCodingOAuth, String> {
        Ok(KimiCodingOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            &server.base,
        ))
    }

    fn device_ok() -> String {
        http_json(
            200,
            r#"{"device_code":"dev-1","user_code":"KIMI-1","verification_uri":"https://auth.kimi.com/device","verification_uri_complete":"https://auth.kimi.com/device?user_code=KIMI-1","interval":1,"expires_in":900}"#,
        )
    }

    fn token_ok() -> String {
        http_json(
            200,
            r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#,
        )
    }

    #[tokio::test]
    async fn login_notifies_complete_uri_and_polls_to_credentials() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            device_ok(),
            http_json(400, r#"{"error":"authorization_pending"}"#),
            token_ok(),
        ])?;
        let flow = oauth(&server)?;
        let interaction = MockInteraction::new();
        let credential = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;

        assert_eq!(credential.access, "access-1");
        assert_eq!(credential.refresh, "refresh-1");
        assert!(credential.extra.is_empty());
        assert!(credential.expires > now_ms() + 3_500_000);

        let events = interaction.events()?;
        match events.as_slice() {
            [AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds,
                expires_in_seconds,
            }] => {
                assert_eq!(user_code, "KIMI-1");
                assert_eq!(
                    verification_uri,
                    "https://auth.kimi.com/device?user_code=KIMI-1"
                );
                assert_eq!(*interval_seconds, Some(1));
                assert_eq!(*expires_in_seconds, Some(900));
            }
            other => return Err(err(format!("unexpected events: {other:?}"))),
        }

        let requests = server.requests()?;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("POST /api/oauth/device_authorization"));
        assert!(
            requests[0]
                .contains("client_id=17e5f671-d194-4dfb-9706-5516cb48c098")
        );
        assert!(requests[1].starts_with("POST /api/oauth/token"));
        assert!(requests[1].contains("device_code=dev-1"));
        assert!(
            requests[1]
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_device_authorization_response_is_reported() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{"device_code":"dev-1","user_code":"KIMI-1","verification_uri":"https://auth.kimi.com/device"}"#,
        )])?;
        let flow = oauth(&server)?;
        let error = flow
            .login(&MockInteraction::new())
            .await
            .expect_err("missing verification_uri_complete must fail");
        assert!(
            error
                .to_string()
                .starts_with("Invalid Kimi Code device authorization response:"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn untrusted_verification_uri_is_rejected() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{"device_code":"d","user_code":"u","verification_uri":"javascript:alert(1)","verification_uri_complete":"https://auth.kimi.com/device"}"#,
        )])?;
        let flow = oauth(&server)?;
        let error = flow
            .login(&MockInteraction::new())
            .await
            .expect_err("javascript: verification_uri must fail");
        assert!(
            error
                .to_string()
                .starts_with("Invalid Kimi Code device authorization response:"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn device_authorization_http_failure_includes_status_and_body() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(503, r#"{"error":"down"}"#)])?;
        let flow = oauth(&server)?;
        let error = flow
            .login(&MockInteraction::new())
            .await
            .expect_err("503 device authorization must fail");
        assert!(
            error.to_string().contains("status 503"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn terminal_poll_errors_map_to_upstream_messages() -> TestResult {
        for (body, needle) in [
            (r#"{"error":"expired_token"}"#, "expired. Please restart login"),
            (r#"{"error":"access_denied"}"#, "was denied"),
            (r#"{"error":"server_error"}"#, "status 400"),
        ] {
            let server = ScriptedServer::spawn(vec![device_ok(), http_json(400, body)])?;
            let flow = oauth(&server)?;
            let error = flow
                .login(&MockInteraction::new())
                .await
                .expect_err("terminal poll error must fail");
            assert!(
                error.to_string().contains(needle),
                "unexpected error: {error}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn token_poll_server_error_is_failed_not_pending() -> TestResult {
        let server =
            ScriptedServer::spawn(vec![device_ok(), http_json(500, r#"{"error":"x"}"#)])?;
        let flow = oauth(&server)?;
        let error = flow
            .login(&MockInteraction::new())
            .await
            .expect_err("500 poll must fail immediately");
        assert!(
            error.to_string().contains("status 500"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_unauthorized_is_terminal() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            401,
            r#"{"error":"invalid_grant","error_description":"revoked"}"#,
        )])?;
        let flow = oauth(&server)?;
        let credential = OAuthCredential {
            access: "a".to_owned(),
            refresh: "r".to_owned(),
            expires: 0,
            extra: BTreeMap::new(),
        };
        let error = flow
            .refresh(&credential, None)
            .await
            .expect_err("401 refresh must fail");
        let message = error.to_string();
        assert!(message.contains("unauthorized (status 401)"), "{message}");
        assert!(message.ends_with("unauthorized (status 401): revoked"), "{message}");
        Ok(())
    }

    #[tokio::test]
    async fn refresh_retries_retryable_failures_then_succeeds() -> TestResult {
        let server =
            ScriptedServer::spawn(vec![http_json(429, r#"{"error":"rate_limited"}"#), token_ok()])?;
        let flow = oauth(&server)?;
        let credential = OAuthCredential {
            access: String::new(),
            refresh: "refresh-1".to_owned(),
            expires: 0,
            extra: BTreeMap::new(),
        };
        let refreshed = flow
            .refresh(&credential, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.access, "access-1");
        assert_eq!(refreshed.refresh, "refresh-1");
        Ok(())
    }

    #[test]
    fn expiry_has_no_skew_and_missing_fields_report_operation() -> TestResult {
        let body: Value = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","expires_in":10}"#,
        )
        .map_err(|e| err(e.to_string()))?;
        let credential =
            credentials_from_token_response(&body, 1000, "poll").map_err(|e| err(e.to_string()))?;
        assert_eq!(credential.expires, 11_000);

        let bad: Value =
            serde_json::from_str(r#"{"access_token":"a"}"#).map_err(|e| err(e.to_string()))?;
        let error = credentials_from_token_response(&bad, 0, "refresh")
            .map_err(|e| err(e.to_string()))
            .expect_err("missing refresh_token must fail");
        assert!(
            error
                .to_string()
                .starts_with("Kimi Code token refresh response missing fields:"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[test]
    fn oauth_host_normalization_and_defaults() -> TestResult {
        assert_eq!(
            normalize_oauth_host("https://auth.kimi.com///"),
            "https://auth.kimi.com"
        );
        assert_eq!(DEFAULT_OAUTH_HOST, "https://auth.kimi.com");
        assert_eq!(KIMI_CLIENT_ID, "17e5f671-d194-4dfb-9706-5516cb48c098");
        Ok(())
    }
}
