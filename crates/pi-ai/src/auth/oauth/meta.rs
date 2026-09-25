//! Meta Model API OAuth flow.
//!
//! This ports `.references/pi/packages/ai/src/auth/oauth/meta.ts`: RFC 8628
//! device authorization at `auth.meta.com`, followed by a Muse Code API-key
//! mint. The identity token is stored as `refresh`; the short-lived Model API
//! key is stored as `access` and re-minted during refresh.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use crate::auth::http::{AuthHttpClient, AuthHttpError, AuthHttpResponse};
use crate::auth::types::{
    AuthError, AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential,
};

/// Muse Code CLI public client id.
pub const META_CLIENT_ID: &str = "1031625952748946";
/// Meta identity device-authorization endpoint.
pub const META_DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
/// Meta identity device-token endpoint.
pub const META_DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
/// Muse Code endpoint that mints a Model API key.
pub const META_API_KEY_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
/// Minted Model API keys live for approximately one day.
pub const META_API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
/// Display name for the subscription OAuth handler.
pub const META_OAUTH_NAME: &str = "Meta (Muse subscription)";
/// Selector label for the subscription login option.
pub const META_OAUTH_LOGIN_LABEL: &str = "Sign in with Meta";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetaDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<u64>,
    expires_in_seconds: Option<u64>,
}

/// Meta (Muse subscription) OAuth handler.
#[derive(Clone, Debug)]
pub struct MetaOAuth {
    http: AuthHttpClient,
    device_authorization_url: String,
    device_token_url: String,
    api_key_mint_url: String,
}

impl MetaOAuth {
    /// Build with production endpoints and a fresh HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the HTTP client cannot be constructed.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            device_authorization_url: META_DEVICE_AUTHORIZATION_URL.to_owned(),
            device_token_url: META_DEVICE_TOKEN_URL.to_owned(),
            api_key_mint_url: META_API_KEY_MINT_URL.to_owned(),
        })
    }

    /// Build with explicit endpoints for deterministic tests.
    #[must_use]
    pub fn with_endpoints(
        http: AuthHttpClient,
        device_authorization_url: impl Into<String>,
        device_token_url: impl Into<String>,
        api_key_mint_url: impl Into<String>,
    ) -> Self {
        Self {
            http,
            device_authorization_url: device_authorization_url.into(),
            device_token_url: device_token_url.into(),
            api_key_mint_url: api_key_mint_url.into(),
        }
    }

    /// Share a production-ready handler behind the object-safe auth trait.
    ///
    /// # Errors
    ///
    /// Propagates HTTP client construction failures from [`Self::new`].
    pub fn shared() -> Result<Arc<dyn OAuthAuth>, AuthError> {
        Ok(Arc::new(Self::new()?))
    }

    async fn request_device_code(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<MetaDeviceCode, AuthError> {
        let fields = BTreeMap::from([(String::from("client_id"), META_CLIENT_ID.to_owned())]);
        let response = self
            .http
            .post_form(&self.device_authorization_url, &fields, None, signal)
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        if !response.ok {
            return Err(response_failure("Meta device authorization", &response));
        }
        parse_device_code(&response.body)
    }

    async fn poll_for_identity_token(
        &self,
        device: MetaDeviceCode,
        signal: Option<CancellationToken>,
    ) -> Result<String, AuthError> {
        let http = self.http.clone();
        let token_url = self.device_token_url.clone();
        let device_code = device.device_code.clone();
        let poll_signal = signal.clone();
        let mut options = OAuthDeviceCodePollOptions::new(move || {
            let http = http.clone();
            let token_url = token_url.clone();
            let device_code = device_code.clone();
            let poll_signal = poll_signal.clone();
            async move {
                let fields = BTreeMap::from([
                    ("grant_type".to_owned(), DEVICE_GRANT_TYPE.to_owned()),
                    ("device_code".to_owned(), device_code),
                    ("client_id".to_owned(), META_CLIENT_ID.to_owned()),
                ]);
                let response = http
                    .post_form(&token_url, &fields, None, poll_signal.as_ref())
                    .await
                    .map_err(AuthHttpError::into_auth_error)?;
                let error = response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let access_token = response
                    .body
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|token| !token.is_empty());
                if response.ok
                    && let Some(token) = access_token
                {
                    return Ok(OAuthDeviceCodePollResult::Complete {
                        value: token.to_owned(),
                    });
                }
                match error {
                    "authorization_pending" => Ok(OAuthDeviceCodePollResult::Pending),
                    "slow_down" => Ok(OAuthDeviceCodePollResult::SlowDown {
                        interval_seconds: positive_number(response.body.get("interval")),
                    }),
                    "access_denied" => Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Meta login was denied.".to_owned(),
                    }),
                    "expired_token" => Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Meta device authorization expired. Please restart login."
                            .to_owned(),
                    }),
                    _ => Ok(OAuthDeviceCodePollResult::Failed {
                        message: response_failure("Meta device token request", &response)
                            .to_string(),
                    }),
                }
            }
        });
        options.interval_seconds = device.interval_seconds;
        options.expires_in_seconds = device.expires_in_seconds;
        options.wait_before_first_poll = true;
        options.signal = signal;
        poll_oauth_device_code_flow(options).await
    }

    async fn mint_api_key(
        &self,
        identity_token: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let headers = BTreeMap::from([
            (
                "Authorization".to_owned(),
                format!("Bearer {identity_token}"),
            ),
            ("x-api-version".to_owned(), "1.0.0".to_owned()),
        ]);
        let body = Value::Object(serde_json::Map::new());
        // An empty identity token (corrupted stored credential reaching
        // refresh) must not be used as a replacement pattern: `replace` would
        // interleave the marker between every detail character.
        let sanitize_detail = |raw: &str| -> String {
            let detail = raw_error_detail(raw);
            if identity_token.is_empty() {
                detail
            } else {
                detail.replace(identity_token, "[redacted]")
            }
        };
        let raw = match self
            .http
            .post_json(&self.api_key_mint_url, &body, Some(&headers), signal)
            .await
        {
            Ok(raw) => raw,
            Err(AuthHttpError::Http { status, body, .. }) if status == 401 || status == 403 => {
                // The identity token is not renewable (auth.meta.com answers
                // grant_type=refresh_token with 404 and issues no refresh_token),
                // so a 401/403 from mint means the session is dead; only a fresh
                // device login helps. Keep the server's safe detail text, but
                // redact any reflected credential material.
                let detail = sanitize_detail(&body);
                return Err(AuthError::message(format!(
                    "Meta session expired (status {status}). Run `/login meta` to sign in again.{detail}"
                )));
            }
            Err(AuthHttpError::Http { status, body, .. }) => {
                let detail = sanitize_detail(&body);
                return Err(AuthError::message(format!(
                    "Meta API key mint failed with status {status}{detail}"
                )));
            }
            Err(other) => return Err(other.into_auth_error()),
        };
        // Success bodies are read leniently: upstream treats a non-JSON body
        // the same as a response without an API key.
        let json = serde_json::from_str::<Value>(&raw).ok();
        let api_key = json
            .as_ref()
            .and_then(|body| body.get("api_key"))
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty());
        if let Some(api_key) = api_key {
            return Ok(OAuthCredential {
                refresh: identity_token.to_owned(),
                access: api_key.to_owned(),
                expires: now_ms() + META_API_KEY_LIFETIME_MS,
                extra: BTreeMap::new(),
            });
        }
        let action_url = json
            .as_ref()
            .and_then(|body| body.get("action_url"))
            .and_then(Value::as_str)
            .filter(|url| trusted_http_url(url));
        Err(AuthError::message(match action_url {
            Some(url) => format!("Meta did not issue an API key. Complete setup at {url}"),
            None => "Meta did not issue an API key.".to_owned(),
        }))
    }
}

impl Default for MetaOAuth {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            http: AuthHttpClient::from_client(reqwest::Client::new()),
            device_authorization_url: META_DEVICE_AUTHORIZATION_URL.to_owned(),
            device_token_url: META_DEVICE_TOKEN_URL.to_owned(),
            api_key_mint_url: META_API_KEY_MINT_URL.to_owned(),
        })
    }
}

impl OAuthAuth for MetaOAuth {
    fn name(&self) -> &str {
        META_OAUTH_NAME
    }

    fn login_label(&self) -> Option<&str> {
        Some(META_OAUTH_LOGIN_LABEL)
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let device = self
                .request_device_code(interaction.signal().as_ref())
                .await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri.clone(),
                interval_seconds: device.interval_seconds,
                expires_in_seconds: device.expires_in_seconds,
            });
            let identity_token = self
                .poll_for_identity_token(device, interaction.signal())
                .await?;
            interaction.notify(AuthEvent::Progress {
                message: "Enabling Meta Model API access...".to_owned(),
            });
            self.mint_api_key(&identity_token, interaction.signal().as_ref())
                .await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            self.mint_api_key(&credential.refresh, signal.as_ref())
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

fn parse_device_code(body: &Value) -> Result<MetaDeviceCode, AuthError> {
    let device_code = required_string(body, "device_code")?;
    let user_code = required_string(body, "user_code")?;
    let raw_uri = body
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|raw| trusted_http_url(raw))
        .or_else(|| body.get("verification_uri").and_then(Value::as_str))
        .ok_or_else(|| invalid_device_response(body))?;
    let verification_uri = validate_http_url(raw_uri)?;
    Ok(MetaDeviceCode {
        device_code,
        user_code,
        verification_uri,
        interval_seconds: positive_number(body.get("interval")),
        expires_in_seconds: positive_number(body.get("expires_in")),
    })
}

fn invalid_device_response(body: &Value) -> AuthError {
    AuthError::message(format!(
        "Invalid Meta device authorization response: {body}"
    ))
}

fn required_string(body: &Value, field: &str) -> Result<String, AuthError> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid_device_response(body))
}

fn positive_number(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}

fn validate_http_url(raw: &str) -> Result<String, AuthError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|_| AuthError::message("Untrusted verification_uri in Meta device response"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(AuthError::message(
            "Untrusted verification_uri in Meta device response",
        ));
    }
    Ok(url.to_string())
}

fn trusted_http_url(raw: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn response_failure(action: &str, response: &AuthHttpResponse) -> AuthError {
    AuthError::message(format!(
        "{action} failed with status {}{}",
        response.status,
        error_detail(&response.body)
    ))
}

/// First safe detail string from a Meta error body, formatted for display.
///
/// Mirrors the upstream `errorDetail` probe order; only fixed string fields
/// are surfaced, never token or identity material.
fn error_detail(body: &Value) -> String {
    ["error_description", "detail", "message", "error"]
        .into_iter()
        .find_map(|key| {
            body.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map_or_else(String::new, |value| format!(": {value}"))
}

/// [`error_detail`] over the raw non-success body captured by the transport.
fn raw_error_detail(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .map(|body| error_detail(&body))
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::auth::types::AuthPrompt;

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

    const AUTHORIZE_PATH: &str = "/device/authorize";
    const TOKEN_PATH: &str = "/device/token";
    const MINT_PATH: &str = "/mint";
    const NOT_FOUND_RESPONSE: &[u8] =
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const SCRIPTED_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

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

    fn http_json(status: u16, body: &str) -> String {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Server Error",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Drain one scripted HTTP request before replying; a single `read` can
    /// return a partial segment, and closing with unread body bytes queued
    /// makes Linux send RST instead of FIN, racing the client.
    fn read_http_request(stream: &mut TcpStream) -> Option<String> {
        const MAX_REQUEST_BYTES: usize = 16_384;
        let deadline = Instant::now().checked_add(SCRIPTED_REQUEST_TIMEOUT)?;

        let mut request = Vec::with_capacity(1_024);
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            if remaining.is_zero() {
                return None;
            }
            stream.set_read_timeout(Some(remaining)).ok()?;

            if request.len() == MAX_REQUEST_BYTES {
                return None;
            }
            let mut chunk = [0_u8; 1_024];
            let read_limit = (MAX_REQUEST_BYTES - request.len()).min(chunk.len());
            let read = stream.read(&mut chunk[..read_limit]).ok()?;
            if read == 0 {
                return None;
            }
            request.extend_from_slice(&chunk[..read]);

            let Some(headers_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
            else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..headers_end]).ok()?;
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then_some(value.trim())
            });
            let content_length = content_length
                .map(str::parse::<usize>)
                .transpose()
                .ok()?
                .unwrap_or(0);
            let request_end = headers_end.checked_add(content_length)?;
            if request_end > MAX_REQUEST_BYTES {
                return None;
            }
            if request.len() < request_end {
                continue;
            }
            request.truncate(request_end);
            return String::from_utf8(request).ok();
        }
    }

    /// Loopback stub scripting ordered responses per request path. Only
    /// scripted paths are captured and answered; unsolicited probes get a 404
    /// so a localhost sweep cannot consume the script, and a scripted path
    /// that runs dry answers 500 with an `exhausted` marker.
    struct ScriptedMetaServer {
        requests: Arc<Mutex<Vec<(String, String)>>>,
        base: String,
        _join: thread::JoinHandle<()>,
    }

    impl ScriptedMetaServer {
        fn spawn(scripts: Vec<(&'static str, Vec<String>)>) -> Result<Self, String> {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
            let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let capture = Arc::clone(&requests);
            let scripts = Mutex::new(HashMap::from_iter(
                scripts
                    .into_iter()
                    .map(|(path, responses)| (path, VecDeque::from(responses))),
            ));
            let join = thread::spawn(move || {
                loop {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    serve_scripted(&scripts, &capture, &mut stream);
                }
            });
            Ok(Self {
                requests,
                base: format!("http://{address}"),
                _join: join,
            })
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        fn requests(&self) -> Result<Vec<(String, String)>, String> {
            self.requests
                .lock()
                .map(|guard| guard.clone())
                .map_err(|_| err("requests lock poisoned"))
        }

        fn requests_for(&self, path: &str) -> Result<Vec<String>, String> {
            Ok(self
                .requests()?
                .into_iter()
                .filter(|(hit, _)| hit == path)
                .map(|(_, request)| request)
                .collect())
        }
    }

    fn serve_scripted(
        scripts: &Mutex<HashMap<&'static str, VecDeque<String>>>,
        capture: &Mutex<Vec<(String, String)>>,
        stream: &mut TcpStream,
    ) {
        let Some(request) = read_http_request(stream) else {
            let _ = stream.write_all(NOT_FOUND_RESPONSE);
            return;
        };
        let target = request.split_ascii_whitespace().nth(1).unwrap_or_default();
        let path = target.split('?').next().unwrap_or_default();
        let Ok(mut scripts) = scripts.lock() else {
            let _ = stream.write_all(http_json(500, r#"{"error":"scripts lock"}"#).as_bytes());
            return;
        };
        let Some(queue) = scripts.get_mut(path) else {
            let _ = stream.write_all(NOT_FOUND_RESPONSE);
            return;
        };
        if let Ok(mut guard) = capture.lock() {
            guard.push((path.to_owned(), request));
        }
        let response = queue
            .pop_front()
            .unwrap_or_else(|| http_json(500, r#"{"error":"exhausted"}"#));
        let _ = stream.write_all(response.as_bytes());
    }

    fn meta_with(server: &ScriptedMetaServer) -> Result<MetaOAuth, String> {
        Ok(MetaOAuth::with_endpoints(
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            server.url(AUTHORIZE_PATH),
            server.url(TOKEN_PATH),
            server.url(MINT_PATH),
        ))
    }

    fn device_grant_json(device_code: &str) -> String {
        serde_json::json!({
            "device_code": device_code,
            "user_code": "ABCD-EFGH",
            "verification_uri": "http://auth.meta.local/device?user_code=ABCD-EFGH",
            "interval": 1,
            "expires_in": 600
        })
        .to_string()
    }

    fn oauth_credential(refresh: &str) -> OAuthCredential {
        OAuthCredential {
            refresh: refresh.to_owned(),
            access: "stale-key".to_owned(),
            expires: 0,
            extra: BTreeMap::new(),
        }
    }

    fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request
            .lines()
            .skip(1)
            .take_while(|line| !line.is_empty())
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name).then(|| value.trim())
            })
    }

    fn request_body(request: &str) -> &str {
        request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
    }

    #[test]
    fn parse_device_code_prefers_complete_http_uri_and_optional_numbers() {
        let body = serde_json::json!({
            "device_code": "device-1",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://auth.meta.com/device",
            "verification_uri_complete": "https://auth.meta.com/device?code=ABCD-EFGH",
            "interval": 5,
            "expires_in": 600
        });
        let device = parse_device_code(&body).expect("valid Meta device response");
        assert_eq!(device.device_code, "device-1");
        assert_eq!(device.user_code, "ABCD-EFGH");
        assert_eq!(
            device.verification_uri,
            "https://auth.meta.com/device?code=ABCD-EFGH"
        );
        assert_eq!(device.interval_seconds, Some(5));
        assert_eq!(device.expires_in_seconds, Some(600));
    }

    #[test]
    fn parse_device_code_rejects_untrusted_uri() {
        let body = serde_json::json!({
            "device_code": "device-1",
            "user_code": "ABCD-EFGH",
            "verification_uri": "file:///tmp/phish"
        });
        let error = parse_device_code(&body).expect_err("file URI must not be shown");
        assert!(error.to_string().contains("Untrusted verification_uri"));
    }

    #[test]
    fn parse_device_code_falls_back_to_plain_uri_when_complete_is_untrusted() {
        let body = serde_json::json!({
            "device_code": "device-1",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://auth.meta.com/device",
            "verification_uri_complete": "file:///tmp/phish"
        });
        let device = parse_device_code(&body).expect("plain verification_uri must be used");
        assert_eq!(device.verification_uri, "https://auth.meta.com/device");
    }

    #[test]
    fn parse_device_code_reports_invalid_response_with_body() {
        let body = serde_json::json!({ "user_code": "ABCD-EFGH" });
        let error = parse_device_code(&body).expect_err("missing device_code");
        assert!(
            error
                .to_string()
                .starts_with("Invalid Meta device authorization response: ")
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "scripted end-to-end login sequence reads as one scenario"
    )]
    #[tokio::test]
    async fn login_polls_pending_and_slow_down_to_success_then_mints() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![
            (
                AUTHORIZE_PATH,
                vec![http_json(200, &device_grant_json("device-live"))],
            ),
            (
                TOKEN_PATH,
                vec![
                    http_json(400, r#"{"error":"authorization_pending"}"#),
                    http_json(400, r#"{"error":"slow_down","interval":1}"#),
                    http_json(200, r#"{"access_token":"identity-live"}"#),
                ],
            ),
            (
                MINT_PATH,
                vec![http_json(200, r#"{"api_key":"minted-key"}"#)],
            ),
        ])?;
        let flow = meta_with(&server)?;
        let interaction = MockInteraction::new();
        let before = now_ms();
        let credential = flow.login(&interaction).await.map_err(|error| {
            let requests = server.requests().unwrap_or_else(|read_error| {
                vec![(String::new(), format!("capture failed: {read_error}"))]
            });
            err(format!("{error}; requests={requests:?}"))
        })?;
        let after = now_ms();

        assert_eq!(credential.refresh, "identity-live");
        assert_eq!(credential.access, "minted-key");
        let expected_min = before + META_API_KEY_LIFETIME_MS - 5_000;
        let expected_max = after + META_API_KEY_LIFETIME_MS + 5_000;
        assert!(
            credential.expires >= expected_min && credential.expires <= expected_max,
            "expiry {} outside [{expected_min}, {expected_max}]",
            credential.expires
        );

        let events = interaction.events()?;
        assert_eq!(events.len(), 2, "events: {events:?}");
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
                    "http://auth.meta.local/device?user_code=ABCD-EFGH"
                );
                assert_eq!(*interval_seconds, Some(1));
                assert_eq!(*expires_in_seconds, Some(600));
            }
            other => return Err(err(format!("expected device_code event, got {other:?}"))),
        }
        match &events[1] {
            AuthEvent::Progress { message } => {
                assert_eq!(message, "Enabling Meta Model API access...");
            }
            other => return Err(err(format!("expected progress event, got {other:?}"))),
        }

        let authorize = server.requests_for(AUTHORIZE_PATH)?;
        assert_eq!(authorize.len(), 1, "authorize requests: {authorize:?}");
        assert_eq!(
            header_value(&authorize[0], "accept"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&authorize[0], "content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert!(
            request_body(&authorize[0]).contains(&format!("client_id={META_CLIENT_ID}")),
            "authorize body: {}",
            request_body(&authorize[0])
        );

        let polls = server.requests_for(TOKEN_PATH)?;
        assert_eq!(polls.len(), 3, "poll requests: {polls:?}");
        assert_eq!(header_value(&polls[0], "accept"), Some("application/json"));
        assert_eq!(
            header_value(&polls[0], "content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert!(
            request_body(&polls[0])
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
            "poll body: {}",
            request_body(&polls[0])
        );
        assert!(
            request_body(&polls[0]).contains("device_code=device-live")
                && request_body(&polls[0]).contains(&format!("client_id={META_CLIENT_ID}")),
            "poll body: {}",
            request_body(&polls[0])
        );

        let mints = server.requests_for(MINT_PATH)?;
        assert_eq!(mints.len(), 1, "mint requests: {mints:?}");
        assert_eq!(header_value(&mints[0], "accept"), Some("application/json"));
        assert_eq!(
            header_value(&mints[0], "content-type"),
            Some("application/json")
        );
        assert_eq!(
            header_value(&mints[0], "authorization"),
            Some("Bearer identity-live")
        );
        assert_eq!(header_value(&mints[0], "x-api-version"), Some("1.0.0"));
        assert_eq!(request_body(&mints[0]), "{}");
        Ok(())
    }

    #[tokio::test]
    async fn login_maps_access_denied() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![
            (
                AUTHORIZE_PATH,
                vec![http_json(200, &device_grant_json("device-deny"))],
            ),
            (
                TOKEN_PATH,
                vec![http_json(400, r#"{"error":"access_denied"}"#)],
            ),
        ])?;
        let flow = meta_with(&server)?;
        let error = expect_err(flow.login(&MockInteraction::new()).await, "denied")?;
        assert_eq!(error.to_string(), "Meta login was denied.");
        Ok(())
    }

    #[tokio::test]
    async fn login_maps_expired_device_token() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![
            (
                AUTHORIZE_PATH,
                vec![http_json(200, &device_grant_json("device-exp"))],
            ),
            (
                TOKEN_PATH,
                vec![http_json(400, r#"{"error":"expired_token"}"#)],
            ),
        ])?;
        let flow = meta_with(&server)?;
        let error = expect_err(flow.login(&MockInteraction::new()).await, "expired")?;
        assert_eq!(
            error.to_string(),
            "Meta device authorization expired. Please restart login."
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_surfaces_device_authorization_failure_with_safe_detail() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![(
            AUTHORIZE_PATH,
            vec![http_json(
                429,
                r#"{"error":"slow_down","error_description":"too many requests"}"#,
            )],
        )])?;
        let flow = meta_with(&server)?;
        let error = expect_err(flow.login(&MockInteraction::new()).await, "throttled")?;
        assert_eq!(
            error.to_string(),
            "Meta device authorization failed with status 429: too many requests"
        );
        Ok(())
    }

    #[tokio::test]
    async fn login_cancelled_during_pending_poll_returns_cancelled() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![
            (
                AUTHORIZE_PATH,
                vec![http_json(200, &device_grant_json("device-cancel"))],
            ),
            (
                TOKEN_PATH,
                vec![http_json(400, r#"{"error":"authorization_pending"}"#)],
            ),
        ])?;
        let flow = meta_with(&server)?;
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let error = expect_err(
            flow.login(&MockInteraction::with_signal(token)).await,
            "cancelled",
        )?;
        assert!(matches!(error, AuthError::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_mints_access_from_identity_and_renews_expiry() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![http_json(200, r#"{"api_key":"minted-next"}"#)],
        )])?;
        let flow = meta_with(&server)?;
        let current = oauth_credential("identity-held");
        let before = now_ms();
        let refreshed = flow
            .refresh(&current, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        let after = now_ms();

        assert_eq!(refreshed.refresh, "identity-held");
        assert_eq!(refreshed.access, "minted-next");
        let expected_min = before + META_API_KEY_LIFETIME_MS - 5_000;
        let expected_max = after + META_API_KEY_LIFETIME_MS + 5_000;
        assert!(
            refreshed.expires >= expected_min && refreshed.expires <= expected_max,
            "expiry {} outside [{expected_min}, {expected_max}]",
            refreshed.expires
        );

        let mints = server.requests_for(MINT_PATH)?;
        assert_eq!(mints.len(), 1, "mint requests: {mints:?}");
        assert_eq!(
            header_value(&mints[0], "authorization"),
            Some("Bearer identity-held")
        );
        assert_eq!(header_value(&mints[0], "x-api-version"), Some("1.0.0"));
        assert_eq!(
            header_value(&mints[0], "content-type"),
            Some("application/json")
        );
        assert_eq!(header_value(&mints[0], "accept"), Some("application/json"));
        Ok(())
    }

    #[tokio::test]
    async fn mint_401_maps_to_session_expired_guidance_without_token_leak() -> TestResult {
        // Hostile/buggy servers can reflect the submitted bearer token inside
        // the error detail; the surfaced message must redact it while keeping
        // the rest of the safe detail text.
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![http_json(
                401,
                r#"{"error":"invalid_session","error_description":"identity sk-identity-echo-7f3a rejected"}"#,
            )],
        )])?;
        let flow = meta_with(&server)?;
        let error = expect_err(
            flow.refresh(&oauth_credential("sk-identity-echo-7f3a"), None)
                .await,
            "session expired",
        )?;
        let message = error.to_string();
        assert_eq!(
            message,
            "Meta session expired (status 401). Run `/login meta` to sign in again.: identity [redacted] rejected"
        );
        assert!(
            !message.contains("sk-identity-echo-7f3a"),
            "error must not leak the identity token: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mint_403_with_non_json_body_maps_to_session_expired() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![http_json(
                403,
                "gateway challenge sk-identity-echo-b29 rejected",
            )],
        )])?;
        let flow = meta_with(&server)?;
        let error = expect_err(
            flow.refresh(&oauth_credential("sk-identity-echo-b29"), None)
                .await,
            "forbidden",
        )?;
        let message = error.to_string();
        assert_eq!(
            message,
            "Meta session expired (status 403). Run `/login meta` to sign in again."
        );
        assert!(
            !message.contains("sk-identity-echo-b29"),
            "error must not leak the identity token: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mint_error_with_empty_identity_keeps_detail_intact() -> TestResult {
        // A corrupted stored credential can reach refresh with an empty
        // refresh token; the empty string must not become a redaction pattern
        // that interleaves the marker between every detail character.
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![http_json(
                401,
                r#"{"error_description":"expired 2026-01-01"}"#,
            )],
        )])?;
        let flow = meta_with(&server)?;
        let error = expect_err(flow.refresh(&oauth_credential(""), None).await, "empty id")?;
        assert_eq!(
            error.to_string(),
            "Meta session expired (status 401). Run `/login meta` to sign in again.: expired 2026-01-01"
        );
        assert!(
            !error.to_string().contains("[redacted]"),
            "empty pattern must not inject markers: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mint_generic_failure_includes_status_and_safe_detail() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![
                http_json(
                    500,
                    r#"{"message":"mint exploded for sk-identity-echo-c41"}"#,
                ),
                http_json(503, "no json here"),
            ],
        )])?;
        let flow = meta_with(&server)?;
        let first = expect_err(
            flow.refresh(&oauth_credential("sk-identity-echo-c41"), None)
                .await,
            "500 failure",
        )?;
        assert_eq!(
            first.to_string(),
            "Meta API key mint failed with status 500: mint exploded for [redacted]"
        );
        assert!(
            !first.to_string().contains("sk-identity-echo-c41"),
            "error must not leak the identity token: {first}"
        );
        let second = expect_err(
            flow.refresh(&oauth_credential("identity-2"), None).await,
            "503 failure",
        )?;
        assert_eq!(
            second.to_string(),
            "Meta API key mint failed with status 503"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mint_success_without_api_key_offers_only_safe_action_url() -> TestResult {
        let server = ScriptedMetaServer::spawn(vec![(
            MINT_PATH,
            vec![
                http_json(200, r#"{"action_url":"https://meta.example/setup"}"#),
                http_json(200, r#"{"action_url":"javascript:alert(1)"}"#),
                http_json(200, "gateway html shell"),
            ],
        )])?;
        let flow = meta_with(&server)?;
        let first = expect_err(
            flow.refresh(&oauth_credential("identity-1"), None).await,
            "action url",
        )?;
        assert_eq!(
            first.to_string(),
            "Meta did not issue an API key. Complete setup at https://meta.example/setup"
        );
        let second = expect_err(
            flow.refresh(&oauth_credential("identity-2"), None).await,
            "script url",
        )?;
        assert_eq!(second.to_string(), "Meta did not issue an API key.");
        let third = expect_err(
            flow.refresh(&oauth_credential("identity-3"), None).await,
            "non-json body",
        )?;
        assert_eq!(third.to_string(), "Meta did not issue an API key.");
        Ok(())
    }
}
