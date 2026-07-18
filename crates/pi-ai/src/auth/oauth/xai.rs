//! xAI OAuth device-code flow.
//!
//! Ports `.references/pi/packages/ai/src/auth/oauth/xai.ts`: fixed client id,
//! scopes, device/token endpoints, required `referrer=pi`, polling with
//! `wait_before_first_poll`, default 3600s lifetime, 5-minute refresh skew,
//! and refresh-token reuse when the server omits rotation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Space-delimited OAuth scopes requested at device authorization.
pub const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// Production device-code endpoint.
pub const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
/// Production token endpoint (device poll + refresh).
pub const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Refresh slightly before reported expiry to avoid mid-request death.
pub const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
/// Used when a token response omits `expires_in`.
pub const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// Required device-authorization form field.
pub const XAI_REFERRER: &str = "pi";
/// Display name for the xAI subscription OAuth handler.
pub const OAUTH_NAME: &str = "xAI (Grok/X subscription)";
/// Selector label for the subscription login option.
pub const OAUTH_LOGIN_LABEL: &str = "Sign in with SuperGrok or X Premium";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Parsed device-code response from xAI.
#[derive(Clone, Debug, Eq, PartialEq)]
struct XaiDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<u64>,
    expires_in_seconds: u64,
}

/// xAI (Grok/X subscription) OAuth handler.
#[derive(Clone, Debug)]
pub struct XaiOAuth {
    http: AuthHttpClient,
    device_code_url: String,
    token_url: String,
}

impl XaiOAuth {
    /// Build with production endpoints and a fresh HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the underlying HTTP client cannot be built.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            device_code_url: XAI_DEVICE_CODE_URL.to_owned(),
            token_url: XAI_TOKEN_URL.to_owned(),
        })
    }

    /// Build with explicit endpoints (tests / mocks).
    #[must_use]
    pub fn with_endpoints(
        http: AuthHttpClient,
        device_code_url: impl Into<String>,
        token_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            device_code_url: device_code_url.into(),
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

    async fn request_device_code(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<XaiDeviceCode, AuthError> {
        let mut fields = BTreeMap::new();
        fields.insert("client_id".into(), XAI_CLIENT_ID.to_owned());
        fields.insert("scope".into(), XAI_SCOPE.to_owned());
        fields.insert("referrer".into(), XAI_REFERRER.to_owned());

        let response = self
            .http
            .post_form(&self.device_code_url, &fields, None, signal)
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        if !response.ok {
            return Err(request_failure("device authorization", &response));
        }
        parse_device_code(&response.body)
    }

    async fn poll_for_tokens(
        &self,
        device: XaiDeviceCode,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let token_url = self.token_url.clone();
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
                fields.insert("grant_type".into(), DEVICE_GRANT_TYPE.to_owned());
                fields.insert("client_id".into(), XAI_CLIENT_ID.to_owned());
                fields.insert("device_code".into(), device_code);

                let response = http
                    .post_form(&token_url, &fields, None, poll_signal.as_ref())
                    .await
                    .map_err(AuthHttpError::into_auth_error)?;

                if response.ok {
                    return Ok(OAuthDeviceCodePollResult::Complete {
                        value: credentials_from_token_response(&response.body, None, now_ms())?,
                    });
                }

                let error = response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if error == "authorization_pending" {
                    return Ok(OAuthDeviceCodePollResult::Pending);
                }
                if error == "slow_down" {
                    let interval_seconds = response
                        .body
                        .get("interval")
                        .and_then(json_number_as_u64)
                        .filter(|value| *value > 0);
                    return Ok(OAuthDeviceCodePollResult::SlowDown { interval_seconds });
                }
                if error == "access_denied" || error == "authorization_denied" {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "xAI device authorization was denied".into(),
                    });
                }
                if error == "expired_token" {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "xAI device code expired".into(),
                    });
                }
                Ok(OAuthDeviceCodePollResult::Failed {
                    message: request_failure("device token polling", &response).to_string(),
                })
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
        let mut fields = BTreeMap::new();
        fields.insert("grant_type".into(), "refresh_token".to_owned());
        fields.insert("client_id".into(), XAI_CLIENT_ID.to_owned());
        fields.insert("refresh_token".into(), refresh_token.to_owned());

        let response = self
            .http
            .post_form(&self.token_url, &fields, None, signal)
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        if !response.ok {
            return Err(request_failure("token refresh", &response));
        }
        credentials_from_token_response(&response.body, Some(refresh_token), now_ms())
    }
}

impl Default for XaiOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            http: AuthHttpClient::from_client(reqwest::Client::new()),
            device_code_url: XAI_DEVICE_CODE_URL.to_owned(),
            token_url: XAI_TOKEN_URL.to_owned(),
        })
    }
}

impl OAuthAuth for XaiOAuth {
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
            let device = self.request_device_code(signal.as_ref()).await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device
                    .verification_uri_complete
                    .clone()
                    .unwrap_or_else(|| device.verification_uri.clone()),
                interval_seconds: device.interval_seconds,
                expires_in_seconds: Some(device.expires_in_seconds),
            });
            self.poll_for_tokens(device, signal).await
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
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: None,
            })
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn required_string(body: &Value, field: &str) -> Result<String, AuthError> {
    match body.get(field).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_owned()),
        _ => Err(AuthError::message(format!(
            "Invalid xAI OAuth response field: {field}"
        ))),
    }
}

fn positive_number(body: &Value, field: &str) -> Result<u64, AuthError> {
    body.get(field)
        .and_then(json_number_as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| AuthError::message(format!("Invalid xAI OAuth response field: {field}")))
}

fn json_number_as_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(number) = value.as_i64() {
        return u64::try_from(number).ok();
    }
    if let Some(number) = value.as_f64()
        && number.is_finite()
        && number > 0.0
        && number.fract() == 0.0
    {
        return number.to_string().parse::<u64>().ok();
    }
    if let Some(text) = value.as_str() {
        return text.parse::<u64>().ok();
    }
    None
}

/// The verification URI is opened in the user's browser; force https so a
/// malicious response cannot make `open` launch something else.
fn validate_verification_uri(raw: &str) -> Result<String, AuthError> {
    let url = Url::parse(raw)
        .map_err(|_| AuthError::message("Untrusted verification URI in xAI OAuth response"))?;
    if url.scheme() != "https" {
        return Err(AuthError::message(
            "Untrusted verification URI in xAI OAuth response",
        ));
    }
    Ok(url.to_string())
}

fn parse_device_code(body: &Value) -> Result<XaiDeviceCode, AuthError> {
    // RFC 8628 allows interval 0; fall back to the poller's default instead of
    // failing on non-positive or malformed values.
    let interval_seconds = body
        .get("interval")
        .and_then(json_number_as_u64)
        .filter(|value| *value > 0);
    let verification_uri_complete = match body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
    {
        Some(raw) if !raw.is_empty() => Some(validate_verification_uri(raw)?),
        _ => None,
    };
    Ok(XaiDeviceCode {
        device_code: required_string(body, "device_code")?,
        user_code: required_string(body, "user_code")?,
        verification_uri: validate_verification_uri(&required_string(body, "verification_uri")?)?,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds: positive_number(body, "expires_in")?,
    })
}

fn credentials_from_token_response(
    body: &Value,
    previous_refresh_token: Option<&str>,
    now: i64,
) -> Result<OAuthCredential, AuthError> {
    let access = required_string(body, "access_token")?;
    // xAI may omit refresh_token on refresh when the token is not rotated.
    let refresh = if body.get("refresh_token").is_none() {
        match previous_refresh_token {
            Some(previous) => previous.to_owned(),
            None => required_string(body, "refresh_token")?,
        }
    } else {
        required_string(body, "refresh_token")?
    };
    let expires_in_seconds = if body.get("expires_in").is_none() {
        DEFAULT_TOKEN_LIFETIME_SECONDS
    } else {
        positive_number(body, "expires_in")?
    };
    let lifetime_ms = i64::try_from(expires_in_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
    let expires = now
        .saturating_add(lifetime_ms)
        .saturating_sub(REFRESH_SKEW_MS);
    Ok(OAuthCredential {
        access,
        refresh,
        expires,
        extra: BTreeMap::new(),
    })
}

fn request_failure(action: &str, response: &AuthHttpResponse) -> AuthError {
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
    let detail = [error, description]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    AuthError::message(if detail.is_empty() {
        format!("xAI OAuth {action} failed (HTTP {})", response.status)
    } else {
        format!(
            "xAI OAuth {action} failed (HTTP {}): {detail}",
            response.status
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::auth::types::{AuthEvent, AuthPrompt};

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

    struct MockInteraction {
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<CancellationToken>,
    }

    impl MockInteraction {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                signal: None,
            }
        }

        fn with_signal(signal: CancellationToken) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                signal: Some(signal),
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
            self.signal.clone()
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

        fn device_url(&self) -> String {
            format!("{}/oauth2/device/code", self.base)
        }

        fn token_url(&self) -> String {
            format!("{}/oauth2/token", self.base)
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
            _ => "Error",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn oauth() -> Result<XaiOAuth, String> {
        Ok(XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            XAI_DEVICE_CODE_URL,
            XAI_TOKEN_URL,
        ))
    }

    #[test]
    fn credentials_default_expires_when_absent() -> TestResult {
        let body = serde_json::json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
        });
        let now = 1_700_000_000_000_i64;
        let cred =
            credentials_from_token_response(&body, None, now).map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.access, "access-1");
        assert_eq!(cred.refresh, "refresh-1");
        assert_eq!(
            cred.expires,
            now + i64::try_from(DEFAULT_TOKEN_LIFETIME_SECONDS * 1000)
                .map_err(|e| err(e.to_string()))?
                - REFRESH_SKEW_MS
        );
        Ok(())
    }

    #[test]
    fn credentials_use_provided_expires_in() -> TestResult {
        let body = serde_json::json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "expires_in": 120,
        });
        let now = 1_700_000_000_000_i64;
        let cred =
            credentials_from_token_response(&body, None, now).map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.expires, now + 120_000 - REFRESH_SKEW_MS);
        Ok(())
    }

    #[test]
    fn credentials_reuse_previous_refresh_when_omitted() -> TestResult {
        let body = serde_json::json!({
            "access_token": "access-3",
            "expires_in": 3600,
        });
        let cred = credentials_from_token_response(&body, Some("keep-me"), 0)
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.refresh, "keep-me");
        assert_eq!(cred.access, "access-3");
        Ok(())
    }

    #[test]
    fn credentials_rotate_refresh_when_present() -> TestResult {
        let body = serde_json::json!({
            "access_token": "access-4",
            "refresh_token": "rotated",
            "expires_in": 3600,
        });
        let cred = credentials_from_token_response(&body, Some("old"), 0)
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(cred.refresh, "rotated");
        Ok(())
    }

    #[test]
    fn parse_device_code_requires_https_verification_uri() -> TestResult {
        let body = serde_json::json!({
            "device_code": "dc",
            "user_code": "UC-1",
            "verification_uri": "http://evil.example/device",
            "expires_in": 600,
        });
        let err_value = expect_err(parse_device_code(&body), "http rejected")?;
        assert!(err_value.to_string().contains("Untrusted verification URI"));
        Ok(())
    }

    #[test]
    fn parse_device_code_accepts_https_and_optional_complete() -> TestResult {
        let body = serde_json::json!({
            "device_code": "dc",
            "user_code": "UC-1",
            "verification_uri": "https://auth.x.ai/device",
            "verification_uri_complete": "https://auth.x.ai/device?user_code=UC-1",
            "interval": 3,
            "expires_in": 900,
        });
        let parsed = parse_device_code(&body).map_err(|e| err(e.to_string()))?;
        assert_eq!(parsed.device_code, "dc");
        assert_eq!(parsed.user_code, "UC-1");
        assert_eq!(parsed.interval_seconds, Some(3));
        assert_eq!(parsed.expires_in_seconds, 900);
        assert_eq!(
            parsed.verification_uri_complete.as_deref(),
            Some("https://auth.x.ai/device?user_code=UC-1")
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_device_request_includes_referrer_and_polls_to_complete() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            http_json(
                200,
                r#"{
                    "device_code":"device-abc",
                    "user_code":"ABCD-EFGH",
                    "verification_uri":"https://auth.x.ai/device",
                    "verification_uri_complete":"https://auth.x.ai/device?user_code=ABCD-EFGH",
                    "interval":1,
                    "expires_in":600
                }"#,
            ),
            http_json(400, r#"{"error":"authorization_pending"}"#),
            http_json(
                200,
                r#"{
                    "access_token":"access-live",
                    "refresh_token":"refresh-live",
                    "expires_in":7200
                }"#,
            ),
        ])?;

        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let interaction = MockInteraction::new();
        let before = now_ms();
        let cred = flow
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        let after = now_ms();

        assert_eq!(cred.access, "access-live");
        assert_eq!(cred.refresh, "refresh-live");
        let expected_min = before + 7_200_000 - REFRESH_SKEW_MS - 5_000;
        let expected_max = after + 7_200_000 - REFRESH_SKEW_MS + 5_000;
        assert!(cred.expires >= expected_min && cred.expires <= expected_max);

        let events = interaction.events()?;
        assert_eq!(events.len(), 1);
        match &events[0] {
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds,
                expires_in_seconds,
            } => {
                assert_eq!(user_code, "ABCD-EFGH");
                assert_eq!(
                    verification_uri,
                    "https://auth.x.ai/device?user_code=ABCD-EFGH"
                );
                assert_eq!(*interval_seconds, Some(1));
                assert_eq!(*expires_in_seconds, Some(600));
            }
            other => return Err(err(format!("expected device_code event, got {other:?}"))),
        }

        let requests = server.requests()?;
        assert!(requests.len() >= 3);
        assert!(
            requests[0].contains("referrer=pi") || requests[0].contains("referrer%3Dpi"),
            "device request must include referrer=pi: {}",
            requests[0]
        );
        assert!(
            requests[0].contains("client_id=") && requests[0].contains(XAI_CLIENT_ID),
            "device request client_id: {}",
            requests[0]
        );
        assert!(
            requests[0].contains("scope=") && requests[0].contains("offline_access"),
            "device request scope: {}",
            requests[0]
        );
        assert!(
            requests[1].contains("grant_type=")
                && (requests[1].contains("device_code")
                    || requests[1]
                        .contains("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
                    || requests[1].contains("urn:ietf:params:oauth:grant-type:device_code")),
            "poll grant type: {}",
            requests[1]
        );
        assert!(
            requests[1].contains("device-abc"),
            "poll device_code: {}",
            requests[1]
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_rotates_when_server_returns_new_refresh() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{
                "access_token":"new-access",
                "refresh_token":"new-refresh",
                "expires_in":1800
            }"#,
        )])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let current = OAuthCredential {
            refresh: "old-refresh".into(),
            access: "old-access".into(),
            expires: 0,
            extra: BTreeMap::new(),
        };
        let refreshed = flow
            .refresh(&current, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.access, "new-access");
        assert_eq!(refreshed.refresh, "new-refresh");

        let requests = server.requests()?;
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains("grant_type=refresh_token"),
            "refresh grant: {}",
            requests[0]
        );
        assert!(
            requests[0].contains("old-refresh"),
            "refresh token field: {}",
            requests[0]
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_reuses_previous_when_omitted() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{
                "access_token":"new-access-only",
                "expires_in":3600
            }"#,
        )])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let current = OAuthCredential {
            refresh: "stable-refresh".into(),
            access: "old".into(),
            expires: 0,
            extra: BTreeMap::new(),
        };
        let refreshed = flow
            .refresh(&current, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.access, "new-access-only");
        assert_eq!(refreshed.refresh, "stable-refresh");
        Ok(())
    }

    #[tokio::test]
    async fn to_auth_returns_bearer_access_as_api_key() -> TestResult {
        let flow = oauth()?;
        let cred = OAuthCredential {
            refresh: "r".into(),
            access: "bearer-token".into(),
            expires: now_ms() + 60_000,
            extra: BTreeMap::new(),
        };
        let auth = flow.to_auth(&cred).await.map_err(|e| err(e.to_string()))?;
        assert_eq!(auth.api_key.as_deref(), Some("bearer-token"));
        assert!(auth.headers.is_none());
        assert!(auth.base_url.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn login_maps_access_denied() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            http_json(
                200,
                r#"{
                    "device_code":"device-deny",
                    "user_code":"DENY",
                    "verification_uri":"https://auth.x.ai/device",
                    "interval":1,
                    "expires_in":600
                }"#,
            ),
            http_json(400, r#"{"error":"access_denied"}"#),
        ])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let err_value = expect_err(flow.login(&MockInteraction::new()).await, "denied")?;
        assert_eq!(err_value.to_string(), "xAI device authorization was denied");
        Ok(())
    }

    #[tokio::test]
    async fn login_maps_expired_token() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            http_json(
                200,
                r#"{
                    "device_code":"device-exp",
                    "user_code":"EXP1",
                    "verification_uri":"https://auth.x.ai/device",
                    "interval":1,
                    "expires_in":600
                }"#,
            ),
            http_json(400, r#"{"error":"expired_token"}"#),
        ])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let err_value = expect_err(flow.login(&MockInteraction::new()).await, "expired")?;
        assert_eq!(err_value.to_string(), "xAI device code expired");
        Ok(())
    }

    #[tokio::test]
    async fn login_cancelled_before_device_request() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            200,
            r#"{
                "device_code":"device-x",
                "user_code":"X",
                "verification_uri":"https://auth.x.ai/device",
                "expires_in":600
            }"#,
        )])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let token = CancellationToken::new();
        token.cancel();
        let err_value = expect_err(
            flow.login(&MockInteraction::with_signal(token)).await,
            "cancelled",
        )?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_error_surfaces_status_detail() -> TestResult {
        let server = ScriptedServer::spawn(vec![http_json(
            401,
            r#"{"error":"invalid_grant","error_description":"token revoked"}"#,
        )])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let current = OAuthCredential {
            refresh: "bad".into(),
            access: "a".into(),
            expires: 0,
            extra: BTreeMap::new(),
        };
        let err_value = expect_err(flow.refresh(&current, None).await, "fail")?;
        let message = err_value.to_string();
        assert!(message.contains("token refresh"));
        assert!(message.contains("401"));
        assert!(message.contains("invalid_grant"));
        assert!(message.contains("token revoked"));
        Ok(())
    }

    #[tokio::test]
    async fn cancel_during_poll_wait_returns_cancelled() -> TestResult {
        let server = ScriptedServer::spawn(vec![
            http_json(
                200,
                r#"{
                    "device_code":"device-cancel",
                    "user_code":"CANC",
                    "verification_uri":"https://auth.x.ai/device",
                    "interval":30,
                    "expires_in":600
                }"#,
            ),
            http_json(400, r#"{"error":"authorization_pending"}"#),
        ])?;
        let flow = XaiOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.device_url(),
            server.token_url(),
        );
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let err_value = expect_err(
            flow.login(&MockInteraction::with_signal(token)).await,
            "cancelled",
        )?;
        assert!(matches!(err_value, AuthError::Cancelled));
        Ok(())
    }

    #[test]
    fn display_name_and_login_label() -> TestResult {
        let flow = oauth()?;
        assert_eq!(flow.name(), "xAI (Grok/X subscription)");
        assert_eq!(
            flow.login_label(),
            Some("Sign in with SuperGrok or X Premium")
        );
        Ok(())
    }
}
