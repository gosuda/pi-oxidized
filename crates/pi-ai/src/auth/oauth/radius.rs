//! Radius gateway OAuth flow.
//!
//! Radius is a pi-messages gateway. OAuth endpoints are discovered from
//! `GET <gateway>/v1/oauth`. Login offers browser (PKCE + loopback callback) or
//! device-code paths; tokens are exchanged as form-urlencoded posts.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::AuthHttpClient;
use super::super::types::{
    AuthEvent, AuthInteraction, AuthPrompt, AuthSelectOption, ModelAuth, OAuthAuth, OAuthCredential,
};
use super::callback_server::{OAuthCallbackConfig, OAuthCallbackServer};
use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};
use super::pkce::generate_pkce;

/// Fixed loopback host used by Radius callback and redirect URI.
pub const CALLBACK_HOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Fixed callback port used by Radius browser login.
pub const CALLBACK_PORT: u16 = 1456;

/// Callback path used by Radius browser login.
pub const CALLBACK_PATH: &str = "/oauth/callback";

/// Redirect URI registered for the browser authorization-code flow.
pub const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";

/// Access-token expiry skew applied after every successful token response.
pub const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;

const LOGIN_METHOD_BROWSER: &str = "browser";
const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";

/// Factory options for [`create_radius_oauth`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RadiusOAuthOptions {
    /// Display name (for example `"Radius"`).
    pub name: String,
    /// Gateway base URL or host; normalized before use.
    pub gateway: String,
}

/// Radius OAuth implementation discovered from a gateway.
#[derive(Clone, Debug)]
pub struct RadiusOAuth {
    name: String,
    gateway: String,
    http: AuthHttpClient,
    /// Production uses [`CALLBACK_PORT`]; tests may override for free ports.
    callback_port: u16,
}

/// OAuth endpoints advertised by `GET <gateway>/v1/oauth`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RadiusOAuthConfig {
    /// OAuth issuer identifier.
    pub issuer: String,
    /// Browser authorization endpoint.
    pub authorization_endpoint: String,
    /// Token endpoint used for code, device, and refresh exchanges.
    pub token_endpoint: String,
    /// Device authorization endpoint.
    pub device_authorization_endpoint: String,
    /// Optional device-authorization events endpoint (preserved from config).
    #[serde(default)]
    pub device_authorization_events_endpoint: String,
    /// Fallback verification URI when the device response omits one.
    pub verification_endpoint: String,
    /// OAuth client id for this gateway.
    pub client_id: String,
    /// Space-delimited scope string requested at authorize/device time.
    pub scope: String,
    /// `grant_type` value used while polling the device token endpoint.
    pub device_code_grant_type: String,
}

#[derive(Clone, Debug)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Clone, Debug)]
struct OAuthResponseError {
    oauth_error: Option<String>,
    message: String,
}

impl std::fmt::Display for OAuthResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Normalize a Radius gateway URL: ensure `http(s)://` and strip trailing slashes.
#[must_use]
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let trimmed = value.trim();
    let with_scheme = if trimmed
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || trimmed
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_owned()
}

fn is_loopback_destination(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_radius_verification_uri(value: &str, gateway: &str) -> Result<String, AuthError> {
    let untrusted = || AuthError::message("Untrusted verification URI in Radius OAuth response");
    let uri = reqwest::Url::parse(value).map_err(|_| untrusted())?;
    if !uri.username().is_empty() || uri.password().is_some() || uri.host_str().is_none() {
        return Err(untrusted());
    }
    if uri.scheme() == "https" {
        return Ok(uri.to_string());
    }
    if uri.scheme() == "http" && is_loopback_destination(&uri) {
        let configured_gateway = reqwest::Url::parse(gateway).map_err(|_| untrusted())?;
        if configured_gateway.scheme() == "http" && is_loopback_destination(&configured_gateway) {
            return Ok(uri.to_string());
        }
    }
    Err(untrusted())
}

/// Build the fixed (or test-overridden) redirect URI for a callback port.
#[must_use]
pub fn radius_redirect_uri(port: u16) -> String {
    if port == CALLBACK_PORT {
        REDIRECT_URI.to_owned()
    } else {
        format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
    }
}

/// Create a Radius OAuth flow for `options.gateway`.
///
/// # Errors
///
/// Returns an error when the shared HTTP client cannot be constructed.
pub fn create_radius_oauth(options: RadiusOAuthOptions) -> Result<RadiusOAuth, AuthError> {
    RadiusOAuth::new(options)
}

impl RadiusOAuth {
    /// Create a Radius OAuth flow with the shared HTTP client and fixed port.
    ///
    /// # Errors
    ///
    /// Returns an error when the shared HTTP client cannot be constructed.
    pub fn new(options: RadiusOAuthOptions) -> Result<Self, AuthError> {
        let http =
            AuthHttpClient::new().map_err(super::super::http::AuthHttpError::into_auth_error)?;
        Ok(Self::with_client(options, http, CALLBACK_PORT))
    }

    /// Create a Radius OAuth flow with an injected HTTP client (tests/mocks).
    #[must_use]
    pub fn with_client(
        options: RadiusOAuthOptions,
        http: AuthHttpClient,
        callback_port: u16,
    ) -> Self {
        Self {
            name: options.name,
            gateway: normalize_radius_gateway_url(&options.gateway),
            http,
            callback_port,
        }
    }

    /// Normalized gateway base URL used for config discovery.
    #[must_use]
    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    /// Callback port used by browser login.
    #[must_use]
    pub fn callback_port(&self) -> u16 {
        self.callback_port
    }

    /// Discover OAuth endpoints from the gateway.
    ///
    /// # Errors
    ///
    /// Returns a message error when the gateway is unreachable or returns a
    /// non-success / invalid payload.
    pub async fn load_oauth_config(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<RadiusOAuthConfig, AuthError> {
        load_radius_oauth_config(&self.http, &self.gateway, signal).await
    }

    async fn login_with_browser(
        &self,
        oauth: &RadiusOAuthConfig,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, AuthError> {
        let pkce = generate_pkce()?;
        let state = random_uuid_v4()?;
        let redirect_uri = radius_redirect_uri(self.callback_port);
        let authorize_url = build_authorize_url(oauth, &pkce.challenge, &state, &redirect_uri);

        let callback_server = OAuthCallbackServer::start_soft(OAuthCallbackConfig {
            port: self.callback_port,
            path: CALLBACK_PATH.to_owned(),
            expected_state: state.clone(),
            success_message: "Signed in to Radius. You may now close this page.".to_owned(),
            host: Some(CALLBACK_HOST),
        })
        .await;

        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OAuth callback on {redirect_uri}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url,
            instructions: Some("Continue in your browser.".to_owned()),
        });

        let wait = callback_server.wait_for_code();
        let code = if let Some(signal) = interaction.signal() {
            tokio::select! {
                () = signal.cancelled() => {
                    callback_server.close().await;
                    return Err(AuthError::Cancelled);
                }
                code = wait => code,
            }
        } else {
            wait.await
        };

        callback_server.close().await;

        let Some(code) = code else {
            if interaction
                .signal()
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(AuthError::Cancelled);
            }
            return Err(AuthError::message("OAuth callback did not complete."));
        };

        // State is validated by the callback server before settling.
        if code.state != state {
            return Err(AuthError::message("OAuth state mismatch"));
        }

        request_oauth_token(
            &self.http,
            oauth,
            form_fields([
                ("grant_type", "authorization_code"),
                ("client_id", oauth.client_id.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("code", code.code.as_str()),
                ("code_verifier", pkce.verifier.as_str()),
            ]),
            interaction.signal().as_ref(),
        )
        .await
        .map_err(map_token_error)
    }

    async fn login_with_device_code(
        &self,
        oauth: &RadiusOAuthConfig,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, AuthError> {
        let device =
            request_device_authorization(&self.http, oauth, interaction.signal().as_ref()).await?;

        let verification_uri = device
            .verification_uri
            .as_deref()
            .unwrap_or(oauth.verification_endpoint.as_str());
        let verification_uri = validate_radius_verification_uri(verification_uri, &self.gateway)?;

        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri,
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
        });

        let http = self.http.clone();
        let oauth = oauth.clone();
        let device_code = device.device_code.clone();
        let signal = interaction.signal();

        let mut options = OAuthDeviceCodePollOptions::new(move || {
            let http = http.clone();
            let oauth = oauth.clone();
            let device_code = device_code.clone();
            let signal = signal.clone();
            async move {
                match request_oauth_token(
                    &http,
                    &oauth,
                    form_fields([
                        ("grant_type", oauth.device_code_grant_type.as_str()),
                        ("client_id", oauth.client_id.as_str()),
                        ("device_code", device_code.as_str()),
                    ]),
                    signal.as_ref(),
                )
                .await
                {
                    Ok(credentials) => {
                        Ok(OAuthDeviceCodePollResult::Complete { value: credentials })
                    }
                    Err(error) => match error.oauth_error.as_deref() {
                        Some("authorization_pending") => Ok(OAuthDeviceCodePollResult::Pending),
                        Some("slow_down") => Ok(OAuthDeviceCodePollResult::SlowDown {
                            interval_seconds: None,
                        }),
                        Some("expired_token") => Ok(OAuthDeviceCodePollResult::Failed {
                            message: "Device authorization expired.".to_owned(),
                        }),
                        Some("access_denied") => Ok(OAuthDeviceCodePollResult::Failed {
                            message: "Device authorization was denied.".to_owned(),
                        }),
                        _ => Err(AuthError::message(error.message)),
                    },
                }
            }
        });
        options.interval_seconds = device.interval;
        options.expires_in_seconds = Some(device.expires_in);
        options.wait_before_first_poll = false;
        options.signal = interaction.signal();

        poll_oauth_device_code_flow(options).await
    }
}

impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login_label(&self) -> Option<&str> {
        None
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let oauth = self
                .load_oauth_config(interaction.signal().as_ref())
                .await?;
            let login_method = interaction
                .prompt(AuthPrompt::Select {
                    message: format!("Sign in to {}:", self.name),
                    options: vec![
                        AuthSelectOption {
                            id: LOGIN_METHOD_BROWSER.to_owned(),
                            label: "Sign in with browser (recommended)".to_owned(),
                            description: None,
                        },
                        AuthSelectOption {
                            id: LOGIN_METHOD_DEVICE_CODE.to_owned(),
                            label: "Sign in with device code (when signing in from another device)"
                                .to_owned(),
                            description: None,
                        },
                    ],
                    signal: None,
                })
                .await?;

            if login_method == LOGIN_METHOD_DEVICE_CODE {
                self.login_with_device_code(&oauth, interaction).await
            } else if login_method == LOGIN_METHOD_BROWSER {
                self.login_with_browser(&oauth, interaction).await
            } else {
                Err(AuthError::message(format!(
                    "Unknown {} sign-in method: {login_method}",
                    self.name
                )))
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            let oauth = self.load_oauth_config(signal.as_ref()).await?;
            request_oauth_token(
                &self.http,
                &oauth,
                form_fields([
                    ("grant_type", "refresh_token"),
                    ("client_id", oauth.client_id.as_str()),
                    ("refresh_token", credential.refresh.as_str()),
                ]),
                signal.as_ref(),
            )
            .await
            .map_err(map_token_error)
        })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        // pi-messages uses the access token as a Bearer API key. Gateway base
        // URL lives on the provider/model config, not ModelAuth; open extras
        // (for example scope) stay on the stored credential.
        Box::pin(async move {
            Ok(ModelAuth {
                api_key: Some(credential.access.clone()),
                headers: None,
                base_url: None,
            })
        })
    }
}

/// Load and parse gateway OAuth discovery config.
///
/// # Errors
///
/// Returns a message error for transport failures, non-success responses, or
/// JSON that is not a Radius OAuth config object.
pub async fn load_radius_oauth_config(
    http: &AuthHttpClient,
    gateway: &str,
    signal: Option<&CancellationToken>,
) -> Result<RadiusOAuthConfig, AuthError> {
    let url = format!("{}/v1/oauth", gateway.trim_end_matches('/'));
    let response = http
        .get_json(&url, None, signal)
        .await
        .map_err(super::super::http::AuthHttpError::into_auth_error)?;

    if !response.ok {
        return Err(AuthError::message(format!(
            "Could not load Radius OAuth config from {gateway}: {} {}",
            response.status, response.raw_body
        )));
    }

    serde_json::from_value(response.body).map_err(|error| {
        AuthError::message(format!(
            "Could not load Radius OAuth config from {gateway}: invalid JSON ({error})"
        ))
    })
}

fn build_authorize_url(
    oauth: &RadiusOAuthConfig,
    challenge: &str,
    state: &str,
    redirect_uri: &str,
) -> String {
    let mut params = BTreeMap::new();
    params.insert("response_type".to_owned(), "code".to_owned());
    params.insert("client_id".to_owned(), oauth.client_id.clone());
    params.insert("redirect_uri".to_owned(), redirect_uri.to_owned());
    params.insert("scope".to_owned(), oauth.scope.clone());
    params.insert("code_challenge".to_owned(), challenge.to_owned());
    params.insert("code_challenge_method".to_owned(), "S256".to_owned());
    params.insert("handoff".to_owned(), "url".to_owned());
    params.insert("state".to_owned(), state.to_owned());

    let query = encode_query(&params);
    if oauth.authorization_endpoint.contains('?') {
        format!("{}&{query}", oauth.authorization_endpoint)
    } else {
        format!("{}?{query}", oauth.authorization_endpoint)
    }
}

async fn request_device_authorization(
    http: &AuthHttpClient,
    oauth: &RadiusOAuthConfig,
    signal: Option<&CancellationToken>,
) -> Result<DeviceAuthorizationResponse, AuthError> {
    let response = http
        .post_form(
            &oauth.device_authorization_endpoint,
            &form_fields([
                ("client_id", oauth.client_id.as_str()),
                ("scope", oauth.scope.as_str()),
            ]),
            None,
            signal,
        )
        .await
        .map_err(super::super::http::AuthHttpError::into_auth_error)?;

    if !response.ok {
        return Err(AuthError::message(
            read_oauth_response_error(
                response.status,
                &response.body,
                &response.raw_body,
                "Radius OAuth device authorization failed",
            )
            .message,
        ));
    }

    let device_code = response
        .body
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let user_code = response
        .body
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let expires_in = response
        .body
        .get("expires_in")
        .and_then(json_u64)
        .filter(|value| *value > 0);

    let (Some(device_code), Some(user_code), Some(expires_in)) =
        (device_code, user_code, expires_in)
    else {
        return Err(AuthError::message(
            "Radius OAuth device authorization response is missing required fields",
        ));
    };

    Ok(DeviceAuthorizationResponse {
        device_code,
        user_code,
        verification_uri: response
            .body
            .get("verification_uri")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_in,
        interval: response.body.get("interval").and_then(json_u64),
    })
}

async fn request_oauth_token(
    http: &AuthHttpClient,
    oauth: &RadiusOAuthConfig,
    body: BTreeMap<String, String>,
    signal: Option<&CancellationToken>,
) -> Result<OAuthCredential, OAuthResponseError> {
    let response = http
        .post_form(&oauth.token_endpoint, &body, None, signal)
        .await
        .map_err(|error| match error {
            super::super::http::AuthHttpError::Cancelled => OAuthResponseError {
                oauth_error: None,
                message: "Login cancelled".to_owned(),
            },
            other => OAuthResponseError {
                oauth_error: None,
                message: other.to_string(),
            },
        })?;

    if !response.ok {
        return Err(read_oauth_response_error(
            response.status,
            &response.body,
            &response.raw_body,
            "Radius OAuth token request failed",
        ));
    }

    credential_from_token_body(&response.body).map_err(|message| OAuthResponseError {
        oauth_error: None,
        message,
    })
}

fn credential_from_token_body(body: &Value) -> Result<OAuthCredential, String> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Radius OAuth token response is missing access_token".to_owned())?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Radius OAuth token response is missing refresh_token".to_owned())?;
    let expires_in = body
        .get("expires_in")
        .and_then(json_u64)
        .ok_or_else(|| "Radius OAuth token response is missing expires_in".to_owned())?;

    let mut extra = BTreeMap::new();
    if let Some(scope) = body.get("scope").and_then(Value::as_str) {
        extra.insert("scope".to_owned(), Value::String(scope.to_owned()));
    }

    Ok(OAuthCredential {
        refresh: refresh.to_owned(),
        access: access.to_owned(),
        expires: now_ms()
            .saturating_add(i64::try_from(expires_in.saturating_mul(1000)).unwrap_or(i64::MAX))
            .saturating_sub(TOKEN_EXPIRY_SKEW_MS),
        extra,
    })
}

fn read_oauth_response_error(
    status: u16,
    body: &Value,
    raw_body: &str,
    message: &str,
) -> OAuthResponseError {
    let oauth_error = body.get("error").and_then(Value::as_str).map(str::to_owned);
    let description = body
        .get("error_description")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            if oauth_error.is_none() && !raw_body.trim().is_empty() {
                Some(raw_body.to_owned())
            } else {
                None
            }
        });

    let detail = match (&oauth_error, &description) {
        (Some(error), Some(description)) => format!("{error}: {description}"),
        (Some(error), None) => error.clone(),
        (None, Some(description)) => description.clone(),
        (None, None) => status.to_string(),
    };

    OAuthResponseError {
        oauth_error,
        message: format!("{message}: {detail}"),
    }
}

fn map_token_error(error: OAuthResponseError) -> AuthError {
    if error.message == "Login cancelled" {
        AuthError::Cancelled
    } else {
        AuthError::message(error.message)
    }
}

fn form_fields<'a, I>(pairs: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn encode_query(fields: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (index, (key, value)) in fields.iter().enumerate() {
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
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|n| u64::try_from(n).ok())),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// RFC 4122 UUID v4 string (same shape as `crypto.randomUUID()`).
fn random_uuid_v4() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AuthError::message(format!("failed to generate OAuth state: {error}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
}

#[cfg(test)]
fn looks_like_uuid_v4(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[14] == b'4'
        && bytes[18] == b'-'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        && bytes[23] == b'-'
        && value.chars().enumerate().all(|(index, ch)| match index {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

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

    #[derive(Default)]
    struct MockInteraction {
        select: Mutex<Option<String>>,
        events: Mutex<Vec<AuthEvent>>,
        signal: Option<CancellationToken>,
    }

    impl MockInteraction {
        fn browser() -> Self {
            Self {
                select: Mutex::new(Some(LOGIN_METHOD_BROWSER.to_owned())),
                events: Mutex::new(Vec::new()),
                signal: None,
            }
        }

        fn device() -> Self {
            Self {
                select: Mutex::new(Some(LOGIN_METHOD_DEVICE_CODE.to_owned())),
                events: Mutex::new(Vec::new()),
                signal: None,
            }
        }

        fn with_signal(mut self, signal: CancellationToken) -> Self {
            self.signal = Some(signal);
            self
        }

        fn events(&self) -> Result<Vec<AuthEvent>, String> {
            lock_mutex(&self.events, "events").map(|guard| guard.clone())
        }
    }

    impl AuthInteraction for MockInteraction {
        fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
            Box::pin(async move {
                match prompt {
                    AuthPrompt::Select { .. } => self
                        .select
                        .lock()
                        .map_err(|_| AuthError::message("select lock poisoned"))?
                        .clone()
                        .ok_or_else(|| AuthError::message("missing select")),
                    AuthPrompt::ManualCode { .. } => {
                        Err(AuthError::message("manual code not expected for radius"))
                    }
                    other => Err(AuthError::message(format!("unexpected prompt: {other:?}"))),
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

    type MockRoute = (String, u16, String, Option<String>);

    fn sample_config(token_url: &str, device_url: &str, authorize_url: &str) -> Value {
        serde_json::json!({
            "issuer": "https://issuer.example",
            "authorizationEndpoint": authorize_url,
            "tokenEndpoint": token_url,
            "deviceAuthorizationEndpoint": device_url,
            "deviceAuthorizationEventsEndpoint": "https://issuer.example/events",
            "verificationEndpoint": "https://issuer.example/device",
            "clientId": "radius-client",
            "scope": "openid profile offline_access",
            "deviceCodeGrantType": "urn:ietf:params:oauth:grant-type:device_code"
        })
    }

    fn spawn_json_server(routes: Arc<Mutex<Vec<MockRoute>>>) -> Result<String, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        let address = listener.local_addr().map_err(|e| err(e.to_string()))?;
        thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let first = request.lines().next().unwrap_or_default().to_owned();
                let body_start = request.find("\r\n\r\n").map_or(n, |idx| idx + 4);
                let req_body = request.get(body_start..).unwrap_or("").to_owned();

                let Ok(mut routes) = routes.lock() else {
                    continue;
                };
                let Some(index) = routes
                    .iter()
                    .position(|(method_path, _, _, _)| first.starts_with(method_path))
                else {
                    let response =
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes());
                    continue;
                };
                let (_method_path, status, body, expected_body_substr) = routes.remove(index);
                if let Some(expected) = expected_body_substr
                    && !req_body.contains(&expected)
                {
                    continue;
                }
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Ok(format!("http://{address}"))
    }

    fn free_port() -> Result<u16, String> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| err(e.to_string()))?;
        Ok(listener
            .local_addr()
            .map_err(|e| err(e.to_string()))?
            .port())
    }

    fn radius_with(gateway: String, port: u16) -> Result<RadiusOAuth, String> {
        Ok(RadiusOAuth::with_client(
            RadiusOAuthOptions {
                name: "Radius".into(),
                gateway,
            },
            AuthHttpClient::new().map_err(|e| err(e.to_string()))?,
            port,
        ))
    }

    fn push_oauth_config(
        routes: &Mutex<Vec<MockRoute>>,
        token_url: &str,
        device_url: &str,
        authorize_url: &str,
    ) -> TestResult {
        let mut routes = lock_mutex(routes, "routes")?;
        routes.push((
            "GET /v1/oauth".to_owned(),
            200,
            sample_config(token_url, device_url, authorize_url).to_string(),
            None,
        ));
        Ok(())
    }

    fn browser_token_server() -> Result<String, String> {
        let token_routes = Arc::new(Mutex::new(vec![(
            "POST /token".to_owned(),
            200,
            serde_json::json!({
                "access_token": "browser-access",
                "refresh_token": "browser-refresh",
                "expires_in": 120,
                "scope": "openid"
            })
            .to_string(),
            Some("grant_type=authorization_code".to_owned()),
        )]));
        Ok(format!("{}/token", spawn_json_server(token_routes)?))
    }

    async fn wait_for_browser_auth_url<F>(
        interaction: &MockInteraction,
        login: &mut Pin<&mut F>,
    ) -> Result<String, String>
    where
        F: Future<Output = Result<OAuthCredential, AuthError>>,
    {
        // The login future must be polled so config discovery, callback bind, and
        // AuthUrl notification can run. Sleep-only waiting never advances it.
        loop {
            if let Some(url) = interaction
                .events()?
                .into_iter()
                .find_map(|event| match event {
                    AuthEvent::AuthUrl { url, .. } => Some(url),
                    _ => None,
                })
            {
                return Ok(url);
            }
            tokio::task::yield_now().await;
            match tokio::time::timeout(Duration::from_millis(10), login.as_mut()).await {
                Ok(Ok(_)) => return Err(err("login finished before callback")),
                Ok(Err(e)) => return Err(err(e.to_string())),
                Err(_) => {}
            }
        }
    }

    #[test]
    fn normalize_gateway_adds_https_and_strips_slash() {
        assert_eq!(
            normalize_radius_gateway_url("radius.pi.dev/"),
            "https://radius.pi.dev"
        );
        assert_eq!(
            normalize_radius_gateway_url("http://localhost:8080///"),
            "http://localhost:8080"
        );
        assert_eq!(
            normalize_radius_gateway_url("https://gw.example"),
            "https://gw.example"
        );
    }

    #[test]
    fn verification_destination_requires_https_except_loopback_development() -> TestResult {
        assert_eq!(
            validate_radius_verification_uri(
                "https://identity.example/device",
                "https://gateway.example",
            )
            .map_err(|error| error.to_string())?,
            "https://identity.example/device"
        );
        assert_eq!(
            validate_radius_verification_uri(
                "http://127.0.0.1:9000/device",
                "http://localhost:8000",
            )
            .map_err(|error| error.to_string())?,
            "http://127.0.0.1:9000/device"
        );
        for value in [
            "http://identity.example/device",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user@identity.example/device",
        ] {
            let error = expect_err(
                validate_radius_verification_uri(value, "https://gateway.example"),
                "untrusted verification destination",
            )?;
            assert_eq!(
                error.to_string(),
                "Untrusted verification URI in Radius OAuth response"
            );
        }
        assert!(
            validate_radius_verification_uri(
                "http://localhost:9000/device",
                "https://gateway.example",
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn redirect_uri_defaults_to_fixed_radius_callback() {
        assert_eq!(radius_redirect_uri(CALLBACK_PORT), REDIRECT_URI);
        assert_eq!(
            radius_redirect_uri(3456),
            "http://127.0.0.1:3456/oauth/callback"
        );
        assert_eq!(CALLBACK_HOST, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!CALLBACK_HOST.is_unspecified());
    }

    #[test]
    fn authorize_url_includes_pkce_uuid_state_and_handoff() -> TestResult {
        let config = RadiusOAuthConfig {
            issuer: "https://issuer".into(),
            authorization_endpoint: "https://issuer/authorize".into(),
            token_endpoint: "https://issuer/token".into(),
            device_authorization_endpoint: "https://issuer/device".into(),
            device_authorization_events_endpoint: String::new(),
            verification_endpoint: "https://issuer/verify".into(),
            client_id: "client".into(),
            scope: "openid".into(),
            device_code_grant_type: "urn:ietf:params:oauth:grant-type:device_code".into(),
        };
        let state = random_uuid_v4().map_err(|e| err(e.to_string()))?;
        let url = build_authorize_url(&config, "challenge", &state, REDIRECT_URI);
        assert!(url.starts_with("https://issuer/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client"));
        assert!(url.contains("code_challenge=challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("handoff=url"));
        assert!(url.contains(&format!("state={state}")));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1456%2Foauth%2Fcallback"));
        // UUID state is distinct from PKCE challenge/verifier.
        assert_ne!(state, "challenge");
        assert!(looks_like_uuid_v4(&state));
        Ok(())
    }

    #[test]
    fn credential_from_token_preserves_scope_extra_and_skew() -> TestResult {
        let before = now_ms();
        let credential = credential_from_token_body(&serde_json::json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 3600,
            "scope": "openid profile"
        }))
        .map_err(err)?;
        let after = now_ms();
        assert_eq!(credential.access, "access-1");
        assert_eq!(credential.refresh, "refresh-1");
        assert_eq!(
            credential.extra.get("scope"),
            Some(&Value::String("openid profile".into()))
        );
        let expected_low = before + 3600 * 1000 - TOKEN_EXPIRY_SKEW_MS;
        let expected_high = after + 3600 * 1000 - TOKEN_EXPIRY_SKEW_MS;
        assert!(credential.expires >= expected_low);
        assert!(credential.expires <= expected_high);

        let encoded =
            serde_json::to_value(crate::auth::types::Credential::Oauth(credential.clone()))
                .map_err(|e| err(e.to_string()))?;
        assert_eq!(encoded["type"], "oauth");
        assert_eq!(encoded["scope"], "openid profile");
        assert!(encoded.get("gatewayConfig").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn config_discovery_loads_camel_case_endpoints() -> TestResult {
        let routes = Arc::new(Mutex::new(vec![(
            "GET /v1/oauth".to_owned(),
            200,
            sample_config(
                "https://issuer/token",
                "https://issuer/device",
                "https://issuer/authorize",
            )
            .to_string(),
            None,
        )]));
        let gateway = spawn_json_server(routes)?;
        let http = AuthHttpClient::new().map_err(|e| err(e.to_string()))?;
        let config = load_radius_oauth_config(&http, &gateway, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(config.client_id, "radius-client");
        assert_eq!(config.token_endpoint, "https://issuer/token");
        assert_eq!(
            config.device_code_grant_type,
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(config.scope, "openid profile offline_access");
        Ok(())
    }

    #[tokio::test]
    async fn config_discovery_error_includes_status_and_body() -> TestResult {
        let routes = Arc::new(Mutex::new(vec![(
            "GET /v1/oauth".to_owned(),
            503,
            "gateway down".to_owned(),
            None,
        )]));
        let gateway = spawn_json_server(routes)?;
        let http = AuthHttpClient::new().map_err(|e| err(e.to_string()))?;
        let err_value = expect_err(
            load_radius_oauth_config(&http, &gateway, None).await,
            "should fail",
        )?;
        let message = err_value.to_string();
        assert!(message.contains("Could not load Radius OAuth config from"));
        assert!(message.contains("503"));
        assert!(message.contains("gateway down"));
        Ok(())
    }

    #[tokio::test]
    async fn browser_callback_rejects_wrong_state() -> TestResult {
        let port = free_port()?;
        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let token_url = browser_token_server()?;
        push_oauth_config(
            &routes,
            &token_url,
            "https://issuer/device",
            "https://issuer/authorize",
        )?;
        let oauth = radius_with(gateway, port)?;
        let interaction = MockInteraction::browser();
        let login = oauth.login(&interaction);
        tokio::pin!(login);
        let auth_url = wait_for_browser_auth_url(&interaction, &mut login).await?;
        let state = auth_url
            .split('&')
            .find_map(|part| part.strip_prefix("state="))
            .ok_or_else(|| err("state"))?
            .to_owned();
        assert!(looks_like_uuid_v4(&state));
        assert!(auth_url.contains("handoff=url"));

        let client = reqwest::Client::new();
        let bad = client
            .get(format!(
                "http://127.0.0.1:{port}{CALLBACK_PATH}?code=bad&state=not-the-state"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
        let bad_html = bad.text().await.map_err(|e| err(e.to_string()))?;
        assert!(
            bad_html.contains("State mismatch") || bad_html.contains("state"),
            "{bad_html}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn browser_callback_exchanges_code_and_to_auth() -> TestResult {
        let port = free_port()?;
        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let token_url = browser_token_server()?;
        push_oauth_config(
            &routes,
            &token_url,
            "https://issuer/device",
            "https://issuer/authorize",
        )?;
        let oauth = radius_with(gateway, port)?;
        let interaction = MockInteraction::browser();
        let login = oauth.login(&interaction);
        tokio::pin!(login);
        let auth_url = wait_for_browser_auth_url(&interaction, &mut login).await?;
        let state = auth_url
            .split('&')
            .find_map(|part| part.strip_prefix("state="))
            .ok_or_else(|| err("state"))?
            .to_owned();

        let client = reqwest::Client::new();
        let good = client
            .get(format!(
                "http://127.0.0.1:{port}{CALLBACK_PATH}?code=auth-code&state={state}"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(good.status(), reqwest::StatusCode::OK);

        let credential = match tokio::time::timeout(Duration::from_secs(3), login).await {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => return Err(err(e.to_string())),
            Err(_) => return Err(err("login timeout")),
        };
        assert_eq!(credential.access, "browser-access");
        assert_eq!(credential.refresh, "browser-refresh");
        assert_eq!(
            credential.extra.get("scope"),
            Some(&Value::String("openid".into()))
        );

        let model_auth = oauth
            .to_auth(&credential)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(model_auth.api_key.as_deref(), Some("browser-access"));
        assert!(model_auth.base_url.is_none());
        assert!(model_auth.headers.is_none());

        let events = interaction.events()?;
        assert!(events.iter().any(|event| matches!(
            event,
            AuthEvent::Progress { message } if message.contains("Listening for OAuth callback")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AuthEvent::AuthUrl {
                instructions: Some(text),
                ..
            } if text == "Continue in your browser."
        )));
        Ok(())
    }

    #[tokio::test]
    async fn browser_state_mismatch_does_not_settle_callback() -> TestResult {
        let port = free_port()?;
        let server = OAuthCallbackServer::start(OAuthCallbackConfig {
            port,
            path: CALLBACK_PATH.into(),
            expected_state: "expected-uuid-state".into(),
            success_message: "Signed in to Radius. You may now close this page.".into(),
            host: Some(CALLBACK_HOST),
        })
        .await
        .map_err(|e| err(e.to_string()))?;

        let client = reqwest::Client::new();
        let bad = client
            .get(format!(
                "http://127.0.0.1:{port}{CALLBACK_PATH}?code=x&state=other"
            ))
            .send()
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

        let wait = server.wait_for_code();
        let timed_out = tokio::time::timeout(Duration::from_millis(100), wait).await;
        assert!(timed_out.is_err(), "bad state must not settle wait");
        server.close().await;
        Ok(())
    }

    fn device_issuer_routes(
        token_body_initial: String,
        token_body_refresh: String,
    ) -> Vec<MockRoute> {
        vec![
            (
                "POST /device".to_owned(),
                200,
                serde_json::json!({
                    "device_code": "dev-code",
                    "user_code": "ABCD-EFGH",
                    "verification_uri": "https://issuer.example/device",
                    "expires_in": 600,
                    "interval": 1
                })
                .to_string(),
                Some("client_id=radius-client".to_owned()),
            ),
            (
                "POST /token".to_owned(),
                400,
                serde_json::json!({
                    "error": "authorization_pending"
                })
                .to_string(),
                Some(
                    "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code".to_owned(),
                ),
            ),
            (
                "POST /token".to_owned(),
                200,
                token_body_initial,
                Some("device_code=dev-code".to_owned()),
            ),
            (
                "POST /token".to_owned(),
                200,
                token_body_refresh,
                Some("grant_type=refresh_token".to_owned()),
            ),
        ]
    }

    #[tokio::test]
    async fn device_login_exchange_completes_with_scope() -> TestResult {
        let token_body_initial = serde_json::json!({
            "access_token": "device-access",
            "refresh_token": "device-refresh",
            "expires_in": 90,
            "scope": "openid profile"
        })
        .to_string();
        let token_body_refresh = serde_json::json!({
            "access_token": "refreshed-access",
            "refresh_token": "refreshed-refresh",
            "expires_in": 90,
            "scope": "openid profile offline"
        })
        .to_string();

        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let issuer = spawn_json_server(Arc::new(Mutex::new(device_issuer_routes(
            token_body_initial,
            token_body_refresh,
        ))))?;
        let device_url = format!("{issuer}/device");
        let token_url = format!("{issuer}/token");
        {
            let mut routes = lock_mutex(&routes, "routes")?;
            for _ in 0..2 {
                routes.push((
                    "GET /v1/oauth".to_owned(),
                    200,
                    sample_config(&token_url, &device_url, "https://issuer/authorize").to_string(),
                    None,
                ));
            }
        }

        let oauth = radius_with(gateway, free_port()?)?;
        let interaction = MockInteraction::device();
        let credential = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(credential.access, "device-access");
        assert_eq!(credential.refresh, "device-refresh");
        assert_eq!(
            credential.extra.get("scope"),
            Some(&Value::String("openid profile".into()))
        );
        let events = interaction.events()?;
        assert!(events.iter().any(|event| matches!(
            event,
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds: Some(1),
                expires_in_seconds: Some(600),
            } if user_code == "ABCD-EFGH" && verification_uri == "https://issuer.example/device"
        )));
        Ok(())
    }

    #[tokio::test]
    async fn device_login_rejects_untrusted_verification_destination() -> TestResult {
        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let issuer_routes = Arc::new(Mutex::new(vec![(
            "POST /device".to_owned(),
            200,
            serde_json::json!({
                "device_code": "dev-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "http://attacker.example/phish",
                "expires_in": 600,
                "interval": 1
            })
            .to_string(),
            Some("client_id=radius-client".to_owned()),
        )]));
        let issuer = spawn_json_server(issuer_routes)?;
        let device_url = format!("{issuer}/device");
        {
            let mut routes = lock_mutex(&routes, "routes")?;
            routes.push((
                "GET /v1/oauth".to_owned(),
                200,
                sample_config(
                    "https://issuer.example/token",
                    &device_url,
                    "https://issuer.example/authorize",
                )
                .to_string(),
                None,
            ));
        }

        let oauth = radius_with(gateway, free_port()?)?;
        let interaction = MockInteraction::device();
        let error = expect_err(oauth.login(&interaction).await, "untrusted destination")?;
        assert_eq!(
            error.to_string(),
            "Untrusted verification URI in Radius OAuth response"
        );
        assert!(
            !interaction
                .events()?
                .iter()
                .any(|event| matches!(event, AuthEvent::DeviceCode { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn device_refresh_rotates_tokens_and_scope() -> TestResult {
        let token_body_initial = serde_json::json!({
            "access_token": "device-access",
            "refresh_token": "device-refresh",
            "expires_in": 90,
            "scope": "openid profile"
        })
        .to_string();
        let token_body_refresh = serde_json::json!({
            "access_token": "refreshed-access",
            "refresh_token": "refreshed-refresh",
            "expires_in": 90,
            "scope": "openid profile offline"
        })
        .to_string();

        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let issuer = spawn_json_server(Arc::new(Mutex::new(device_issuer_routes(
            token_body_initial,
            token_body_refresh,
        ))))?;
        let device_url = format!("{issuer}/device");
        let token_url = format!("{issuer}/token");
        {
            let mut routes = lock_mutex(&routes, "routes")?;
            for _ in 0..2 {
                routes.push((
                    "GET /v1/oauth".to_owned(),
                    200,
                    sample_config(&token_url, &device_url, "https://issuer/authorize").to_string(),
                    None,
                ));
            }
        }

        let oauth = radius_with(gateway, free_port()?)?;
        let interaction = MockInteraction::device();
        let credential = oauth
            .login(&interaction)
            .await
            .map_err(|e| err(e.to_string()))?;
        let refreshed = oauth
            .refresh(&credential, None)
            .await
            .map_err(|e| err(e.to_string()))?;
        assert_eq!(refreshed.access, "refreshed-access");
        assert_eq!(refreshed.refresh, "refreshed-refresh");
        assert_eq!(
            refreshed.extra.get("scope"),
            Some(&Value::String("openid profile offline".into()))
        );
        Ok(())
    }

    #[tokio::test]
    async fn device_access_denied_error() -> TestResult {
        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let device_routes = Arc::new(Mutex::new(vec![
            (
                "POST /device".to_owned(),
                200,
                serde_json::json!({
                    "device_code": "dev-code",
                    "user_code": "ZZZZ",
                    "expires_in": 30,
                    "interval": 1
                })
                .to_string(),
                None,
            ),
            (
                "POST /token".to_owned(),
                400,
                serde_json::json!({
                    "error": "access_denied",
                    "error_description": "user said no"
                })
                .to_string(),
                None,
            ),
        ]));
        let issuer = spawn_json_server(device_routes)?;
        {
            let mut routes = lock_mutex(&routes, "routes")?;
            routes.push((
                "GET /v1/oauth".to_owned(),
                200,
                sample_config(
                    &format!("{issuer}/token"),
                    &format!("{issuer}/device"),
                    "https://issuer/authorize",
                )
                .to_string(),
                None,
            ));
        }

        let oauth = radius_with(gateway, free_port()?)?;
        let err_value = expect_err(oauth.login(&MockInteraction::device()).await, "denied")?;
        assert_eq!(err_value.to_string(), "Device authorization was denied.");
        Ok(())
    }

    #[tokio::test]
    async fn device_cancel_before_config_load() -> TestResult {
        let routes = Arc::new(Mutex::new(Vec::new()));
        let gateway = spawn_json_server(routes.clone())?;
        let issuer = spawn_json_server(Arc::new(Mutex::new(Vec::new())))?;
        {
            let mut routes = lock_mutex(&routes, "routes")?;
            routes.push((
                "GET /v1/oauth".to_owned(),
                200,
                sample_config(
                    &format!("{issuer}/token"),
                    &format!("{issuer}/device"),
                    "https://issuer/authorize",
                )
                .to_string(),
                None,
            ));
        }
        let oauth = radius_with(gateway, free_port()?)?;
        let token = CancellationToken::new();
        token.cancel();
        let cancelled = expect_err(
            oauth
                .login(&MockInteraction::device().with_signal(token))
                .await,
            "cancelled",
        )?;
        assert!(matches!(cancelled, AuthError::Cancelled));
        assert_eq!(cancelled.to_string(), "Login cancelled");
        Ok(())
    }

    #[tokio::test]
    async fn unknown_login_method_errors_with_provider_name() -> TestResult {
        let routes = Arc::new(Mutex::new(vec![(
            "GET /v1/oauth".to_owned(),
            200,
            sample_config(
                "https://issuer/token",
                "https://issuer/device",
                "https://issuer/authorize",
            )
            .to_string(),
            None,
        )]));
        let gateway = spawn_json_server(routes)?;
        let oauth = radius_with(gateway, free_port()?)?;
        let interaction = MockInteraction {
            select: Mutex::new(Some("sms".into())),
            events: Mutex::new(Vec::new()),
            signal: None,
        };
        let err_value = expect_err(oauth.login(&interaction).await, "unknown")?;
        assert_eq!(err_value.to_string(), "Unknown Radius sign-in method: sms");
        Ok(())
    }

    #[tokio::test]
    async fn soft_failed_callback_reports_incomplete() -> TestResult {
        let hold_port = free_port()?;
        let _hold =
            TcpListener::bind(format!("127.0.0.1:{hold_port}")).map_err(|e| err(e.to_string()))?;
        let routes = Arc::new(Mutex::new(vec![(
            "GET /v1/oauth".to_owned(),
            200,
            sample_config(
                "https://issuer/token",
                "https://issuer/device",
                "https://issuer/authorize",
            )
            .to_string(),
            None,
        )]));
        let gateway = spawn_json_server(routes)?;
        let oauth = radius_with(gateway, hold_port)?;
        let err_value = expect_err(oauth.login(&MockInteraction::browser()).await, "soft fail")?;
        assert_eq!(err_value.to_string(), "OAuth callback did not complete.");
        Ok(())
    }
}
