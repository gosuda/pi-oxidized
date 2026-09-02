//! Kimi Code (subscription) OAuth device-code flow.
//!
//! Ports `.references/pi/packages/ai/src/auth/oauth/kimi-coding.ts`: fixed
//! client id, device/token endpoints under a configurable host, polling with
//! `wait_before_first_poll`, 15-minute device-code lifetime, retry-on-5xx
//! refresh with exponential backoff, and `Bearer` auth header derivation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::super::error::AuthError;
use super::super::http::{AuthHttpClient, AuthHttpError, AuthHttpResponse};
use super::super::types::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use super::device_code::{
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, poll_oauth_device_code_flow,
};

/// Fixed public OAuth client id for Kimi Code.
pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

/// Default OAuth host.
pub const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";

/// Environment variable for overriding the OAuth host.
pub const OAUTH_HOST_ENV: &str = "KIMI_CODE_OAUTH_HOST";

/// Device-code timeout in seconds (15 minutes).
pub const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;

/// Default polling interval in seconds.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

/// Maximum refresh retries.
pub const REFRESH_MAX_RETRIES: u32 = 3;

/// Display name for the Kimi Code OAuth handler.
pub const OAUTH_NAME: &str = "Kimi Code (subscription)";

/// Selector label for the subscription login option.
pub const OAUTH_LOGIN_LABEL: &str = "Sign in with Kimi Code";

const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_GRANT_TYPE: &str = "refresh_token";

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
    /// Build with production endpoints and a fresh HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError`] when the underlying HTTP client cannot be built.
    pub fn new() -> Result<Self, AuthError> {
        Ok(Self {
            http: AuthHttpClient::new().map_err(AuthHttpError::into_auth_error)?,
            oauth_host: oauth_host_from_env(),
        })
    }

    /// Build with explicit endpoints (tests / mocks).
    #[must_use]
    pub fn with_endpoints(http: AuthHttpClient, oauth_host: impl Into<String>) -> Self {
        Self {
            http,
            oauth_host: oauth_host.into(),
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

    fn device_authorization_url(&self) -> String {
        format!("{}/api/oauth/device_authorization", self.oauth_host)
    }

    fn token_url(&self) -> String {
        format!("{}/api/oauth/token", self.oauth_host)
    }

    async fn request_device_code(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<KimiDeviceCode, AuthError> {
        let mut fields = BTreeMap::new();
        fields.insert("client_id".into(), CLIENT_ID.to_owned());

        let response = self
            .http
            .post_form(&self.device_authorization_url(), &fields, None, signal)
            .await
            .map_err(AuthHttpError::into_auth_error)?;
        if !response.ok {
            return Err(request_failure("device authorization", &response));
        }
        parse_device_code(&response.body)
    }

    async fn poll_for_tokens(
        &self,
        device: KimiDeviceCode,
        signal: Option<CancellationToken>,
    ) -> Result<OAuthCredential, AuthError> {
        let token_url = self.token_url();
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
                fields.insert("client_id".into(), CLIENT_ID.to_owned());
                fields.insert("device_code".into(), device_code);
                fields.insert("grant_type".into(), DEVICE_GRANT_TYPE.to_owned());

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
                if error == "expired_token" {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code device authorization expired. Please restart login."
                            .into(),
                    });
                }
                if error == "access_denied" {
                    return Ok(OAuthDeviceCodePollResult::Failed {
                        message: "Kimi Code login was denied.".into(),
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
        let mut last_error: Option<AuthError> = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(1 << (attempt - 1));
                tokio::time::sleep(delay).await;
            }
            if let Some(sig) = signal
                && sig.is_cancelled()
            {
                return Err(AuthError::message("Kimi Code token refresh aborted"));
            }

            let mut fields = BTreeMap::new();
            fields.insert("client_id".into(), CLIENT_ID.to_owned());
            fields.insert("grant_type".into(), REFRESH_GRANT_TYPE.to_owned());
            fields.insert("refresh_token".into(), refresh_token.to_owned());

            let response = match self
                .http
                .post_form(&self.token_url(), &fields, None, signal)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(AuthHttpError::into_auth_error(error));
                    continue;
                }
            };

            if response.ok {
                return credentials_from_token_response(
                    &response.body,
                    Some(refresh_token),
                    now_ms(),
                );
            }

            let status = response.status;
            let error = response
                .body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if status == 401 || status == 403 || error == "invalid_grant" {
                let description = response
                    .body
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let detail = if description.is_empty() {
                    String::new()
                } else {
                    format!(": {description}")
                };
                return Err(AuthError::message(format!(
                    "Kimi Code token refresh unauthorized (status {status}){detail}"
                )));
            }

            if (status == 429 || status >= 500) && attempt < REFRESH_MAX_RETRIES {
                last_error = Some(request_failure("token refresh", &response));
                continue;
            }

            return Err(request_failure("token refresh", &response));
        }
        Err(last_error.unwrap_or_else(|| AuthError::message("Kimi Code token refresh failed")))
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
            let device = self.request_device_code(signal.as_ref()).await?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri_complete.clone(),
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

fn oauth_host_from_env() -> String {
    std::env::var(OAUTH_HOST_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.to_owned())
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
            "Invalid Kimi Code OAuth response field: {field}"
        ))),
    }
}

fn positive_number(body: &Value, field: &str) -> Result<u64, AuthError> {
    body.get(field)
        .and_then(json_number_as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            AuthError::message(format!("Invalid Kimi Code OAuth response field: {field}"))
        })
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

fn validate_http_url(raw: &str) -> Result<String, AuthError> {
    let url = reqwest::Url::parse(raw).map_err(|_| {
        AuthError::message("Untrusted verification URI in Kimi Code OAuth response")
    })?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(AuthError::message(
            "Untrusted verification URI in Kimi Code OAuth response",
        ));
    }
    Ok(url.to_string())
}

fn parse_device_code(body: &Value) -> Result<KimiDeviceCode, AuthError> {
    let interval_seconds = body
        .get("interval")
        .and_then(json_number_as_u64)
        .filter(|value| *value > 0);
    let verification_uri = validate_http_url(&required_string(body, "verification_uri")?)?;
    let verification_uri_complete =
        validate_http_url(&required_string(body, "verification_uri_complete")?)?;
    let expires_in_seconds = if body.get("expires_in").is_none() {
        DEVICE_CODE_TIMEOUT_SECONDS
    } else {
        positive_number(body, "expires_in")?
    };
    Ok(KimiDeviceCode {
        device_code: required_string(body, "device_code")?,
        user_code: required_string(body, "user_code")?,
        verification_uri,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds,
    })
}

fn credentials_from_token_response(
    body: &Value,
    previous_refresh_token: Option<&str>,
    now: i64,
) -> Result<OAuthCredential, AuthError> {
    let access = required_string(body, "access_token")?;
    let refresh = if body.get("refresh_token").is_none() {
        match previous_refresh_token {
            Some(previous) => previous.to_owned(),
            None => required_string(body, "refresh_token")?,
        }
    } else {
        required_string(body, "refresh_token")?
    };
    let expires_in_seconds = if body.get("expires_in").is_none() {
        3600_u64
    } else {
        positive_number(body, "expires_in")?
    };
    let lifetime_ms = i64::try_from(expires_in_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
    let expires = now.saturating_add(lifetime_ms);
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
        format!("Kimi Code OAuth {action} failed (HTTP {})", response.status)
    } else {
        format!(
            "Kimi Code OAuth {action} failed (HTTP {}): {detail}",
            response.status
        )
    })
}
