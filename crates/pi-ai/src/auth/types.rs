//! Auth data contracts and object-safe provider-auth traits.
//!
//! Credentials are the on-disk `auth.json` shape: an internally tagged
//! `type` discriminant with open OAuth extras preserved via flatten. The
//! trait surfaces use [`futures::future::BoxFuture`] so they remain object-safe.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use super::error::AuthError;

/// Provider-scoped environment overrides. Values take precedence over process env.
pub type ProviderEnv = BTreeMap<String, String>;

/// Additional request headers derived from auth resolution.
pub type ProviderHeaders = BTreeMap<String, String>;

/// Request auth for a single model request.
///
/// If a value cannot be expressed as `api_key`, `headers`, or `base_url`, it is
/// provider config, not auth.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    /// Bearer/API key material when the provider accepts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Extra request headers contributed by auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    /// Per-credential base URL override (for example GitHub Copilot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl std::fmt::Debug for ModelAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelAuth")
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("headers", &self.headers.as_ref().map(|_| "[redacted]"))
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Stored API-key credential.
///
/// `env` holds provider-scoped environment/config values such as Cloudflare
/// account/gateway ids. `key` may be a literal, `$ENV`/`${ENV}` template, or
/// `!command` form; stores resolve copies on read and keep the raw form on disk.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyCredential {
    /// Optional API key material or resolvable template/command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Provider-scoped environment/config values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

impl std::fmt::Debug for ApiKeyCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyCredential")
            .field("key", &self.key.as_ref().map(|_| "[redacted]"))
            .field(
                "env",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// Stored OAuth credential with open provider-specific extras.
///
/// Known fields are `refresh`, `access`, and `expires` (epoch milliseconds).
/// Everything else (`accountId`, `enterpriseUrl`, `availableModelIds`, …) is
/// preserved in [`OAuthCredential::extra`] and re-serialized faithfully.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    /// Refresh token (or provider-specific long-lived secret used to mint access).
    pub refresh: String,
    /// Current access token.
    pub access: String,
    /// Access-token expiry as epoch milliseconds.
    pub expires: i64,
    /// Open provider extras preserved across round trips.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl std::fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("refresh", &"[redacted]")
            .field("access", &"[redacted]")
            .field("expires", &self.expires)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One type-tagged credential per provider — the shape of `auth.json` values.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum Credential {
    /// API-key credential.
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    /// OAuth credential.
    #[serde(rename = "oauth")]
    Oauth(OAuthCredential),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(cred) => f.debug_tuple("ApiKey").field(cred).finish(),
            Self::Oauth(cred) => f.debug_tuple("Oauth").field(cred).finish(),
        }
    }
}

impl Credential {
    /// Credential discriminant without exposing secrets.
    #[must_use]
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::ApiKey(_) => CredentialKind::ApiKey,
            Self::Oauth(_) => CredentialKind::Oauth,
        }
    }
}

/// Non-secret credential type tag.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CredentialKind {
    /// API-key credential.
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth credential.
    #[serde(rename = "oauth")]
    Oauth,
}

impl CredentialKind {
    /// Stable wire/text form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Oauth => "oauth",
        }
    }
}

/// Non-secret credential metadata for account/status enumeration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    /// Provider id the credential belongs to.
    pub provider_id: String,
    /// Credential discriminant.
    #[serde(rename = "type")]
    pub kind: CredentialKind,
}

/// Result of resolving auth for a model request.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthResult {
    /// Request auth material.
    pub auth: ModelAuth,
    /// Provider-scoped environment/config values resolved from credentials and ambient context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
    /// Human-readable label for status UI (`ANTHROPIC_API_KEY`, `OAuth`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl std::fmt::Debug for AuthResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthResult")
            .field("auth", &self.auth)
            .field(
                "env",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .field("source", &self.source)
            .finish()
    }
}

/// Side-effect-free availability check result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCheck {
    /// Human-readable source label when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Configured auth type.
    #[serde(rename = "type")]
    pub kind: AuthType,
}

/// Auth mechanism reported by checks and status UI.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuthType {
    /// API-key / ambient key auth.
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth auth.
    #[serde(rename = "oauth")]
    Oauth,
}

/// Selectable option presented during login.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthSelectOption {
    /// Stable option id returned from [`AuthInteraction::prompt`].
    pub id: String,
    /// Display label.
    pub label: String,
    /// Optional longer description.
    pub description: Option<String>,
}

/// Prompt shown to the user during login.
///
/// `signal` lets the flow cancel a pending prompt when an out-of-band event
/// resolves the step (for example a `manual_code` prompt raced against a
/// callback server).
#[derive(Clone, Debug)]
pub enum AuthPrompt {
    /// Free-text prompt.
    Text {
        /// Prompt message.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
        /// Optional cancellation token for this prompt only.
        signal: Option<CancellationToken>,
    },
    /// Secret/password prompt.
    Secret {
        /// Prompt message.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
        /// Optional cancellation token for this prompt only.
        signal: Option<CancellationToken>,
    },
    /// Single-select prompt; the selected option id is returned.
    Select {
        /// Prompt message.
        message: String,
        /// Options presented to the user.
        options: Vec<AuthSelectOption>,
        /// Optional cancellation token for this prompt only.
        signal: Option<CancellationToken>,
    },
    /// Manual OAuth code paste prompt.
    ManualCode {
        /// Prompt message.
        message: String,
        /// Optional input placeholder.
        placeholder: Option<String>,
        /// Optional cancellation token for this prompt only.
        signal: Option<CancellationToken>,
    },
}

/// Optional hyperlink attached to an info auth event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthInfoLink {
    /// Link target.
    pub url: String,
    /// Optional display label.
    pub label: Option<String>,
}

/// Progress/status events emitted during login.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthEvent {
    /// Informational message with optional links.
    Info {
        /// Message body.
        message: String,
        /// Optional related links.
        links: Option<Vec<AuthInfoLink>>,
    },
    /// Browser authorization URL the user should open.
    AuthUrl {
        /// Authorization URL.
        url: String,
        /// Optional human instructions.
        instructions: Option<String>,
    },
    /// Device-code flow details.
    DeviceCode {
        /// User code to enter.
        user_code: String,
        /// Verification URI.
        verification_uri: String,
        /// Suggested polling interval in seconds.
        interval_seconds: Option<u64>,
        /// Device-code lifetime in seconds.
        expires_in_seconds: Option<u64>,
    },
    /// Generic progress update.
    Progress {
        /// Progress message.
        message: String,
    },
}

/// Environment access for auth resolution. Injectable for tests.
pub trait AuthContext: Send + Sync {
    /// Look up an environment value by name.
    ///
    /// Implementations should treat missing and blank values as `None`.
    fn env<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<String>>;

    /// Check whether a filesystem path exists.
    ///
    /// A leading `~` expands to the configured home directory when present.
    fn file_exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, bool>;
}

/// Login interaction callbacks serving both API-key and OAuth flows.
///
/// `prompt` returns the entered/selected string (`select` returns the option
/// id) and fails on cancel/abort. Whole-flow cancellation uses [`Self::signal`];
/// per-prompt cancellation uses [`AuthPrompt`]'s own signal.
pub trait AuthInteraction: Send + Sync {
    /// Prompt the user and return their response.
    fn prompt(&self, prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>>;

    /// Emit a non-blocking login status event.
    fn notify(&self, event: AuthEvent);

    /// Optional whole-flow cancellation token.
    fn signal(&self) -> Option<CancellationToken>;
}

/// API-key auth: stored key/provider env plus ambient sources.
///
/// Ambient-only providers return `None` from [`ApiKeyAuth::login`].
pub trait ApiKeyAuth: Send + Sync {
    /// Display name, for example "Anthropic API key".
    fn name(&self) -> &str;

    /// Interactive setup. `None` means ambient-only (no interactive login).
    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, AuthError>>>;

    /// Optional side-effect-free availability check.
    ///
    /// `None` means callers should probe availability via [`ApiKeyAuth::resolve`].
    fn check<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> Option<BoxFuture<'a, Option<AuthCheck>>>;

    /// Resolve auth from the stored credential and/or ambient sources.
    ///
    /// `None` means the provider is not configured.
    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Option<AuthResult>>;
}

/// OAuth auth with a refresh/to-auth split owned by higher-level resolution.
pub trait OAuthAuth: Send + Sync {
    /// Display name, for example "Anthropic (Claude Pro/Max)".
    fn name(&self) -> &str;

    /// Selector label for the subscription login option.
    fn login_label(&self) -> Option<&str>;

    /// Interactive OAuth login producing a stored credential.
    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>>;

    /// Exchange/refresh tokens. Network call; fails on `invalid_grant` etc.
    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>>;

    /// Side-effect-free derivation of request auth from a valid credential.
    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>>;
}

/// Provider auth handlers. At least one of `api_key`/`oauth` is present for
/// configured providers; ambient-only providers still supply `api_key` auth
/// whose `resolve` reports whether the provider is configured.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    /// API-key / ambient auth handler.
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    /// OAuth auth handler.
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

/// App-owned credential storage, keyed by provider id (one credential each).
///
/// `modify` is the only write path besides `delete`, so every mutation is a
/// serialized read-modify-write. Returning `Ok(None)` from the callback leaves
/// the entry unchanged and never deletes it.
pub trait CredentialStore: Send + Sync {
    /// Read the stored credential, possibly expired.
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, super::error::StoreError>>;

    /// List stored credential metadata without resolving or exposing secrets.
    ///
    /// Implementations must not execute configured API-key commands while listing.
    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, super::error::StoreError>>;

    /// Serialized write — the only general write path.
    ///
    /// The callback sees the current credential. Return `Ok(Some(cred))` to
    /// replace the entry, or `Ok(None)` to leave it unchanged. Callback errors
    /// propagate without writing.
    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: Box<CredentialModifyFn<'a>>,
    ) -> BoxFuture<'a, Result<Option<Credential>, super::error::StoreError>>;

    /// Remove a credential (logout). Serialized against `modify`.
    fn delete<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<(), super::error::StoreError>>;
}

/// Boxed callback used by [`CredentialStore::modify`].
///
/// Returns `Ok(Some(credential))` to replace the entry, or `Ok(None)` to leave
/// the current entry unchanged.
pub type CredentialModifyFn<'a> = dyn FnOnce(Option<Credential>) -> BoxFuture<'static, Result<Option<Credential>, AuthError>>
    + Send
    + 'a;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_open_field_roundtrip_preserves_oauth_extras_and_api_key_env()
    -> Result<(), serde_json::Error> {
        let oauth = Credential::Oauth(OAuthCredential {
            refresh: "refresh-token".into(),
            access: "access-token".into(),
            expires: 1_750_000_000_000,
            extra: BTreeMap::from([
                ("accountId".into(), json!("acct_123")),
                ("enterpriseUrl".into(), json!("https://github.example")),
                (
                    "availableModelIds".into(),
                    json!(["gpt-4.1", "gpt-4.1-mini"]),
                ),
            ]),
        });

        let encoded = serde_json::to_value(&oauth)?;
        assert_eq!(encoded["type"], json!("oauth"));
        assert_eq!(encoded["accountId"], json!("acct_123"));
        assert_eq!(encoded["enterpriseUrl"], json!("https://github.example"));
        assert_eq!(
            encoded["availableModelIds"],
            json!(["gpt-4.1", "gpt-4.1-mini"])
        );

        let decoded: Credential = serde_json::from_value(encoded)?;
        assert_eq!(decoded, oauth);

        let api_key = Credential::ApiKey(ApiKeyCredential {
            key: Some("$OPENAI_API_KEY".into()),
            env: Some(BTreeMap::from([(
                "CLOUDFLARE_ACCOUNT_ID".into(),
                "acct".into(),
            )])),
        });
        let encoded = serde_json::to_value(&api_key)?;
        assert_eq!(encoded["type"], json!("api_key"));
        assert_eq!(encoded["key"], json!("$OPENAI_API_KEY"));
        assert_eq!(encoded["env"]["CLOUDFLARE_ACCOUNT_ID"], json!("acct"));
        let decoded: Credential = serde_json::from_value(encoded)?;
        assert_eq!(decoded, api_key);
        Ok(())
    }

    #[test]
    fn model_auth_and_auth_result_use_camel_case_wire_fields() -> Result<(), serde_json::Error> {
        let result = AuthResult {
            auth: ModelAuth {
                api_key: Some("sk-test".into()),
                headers: Some(BTreeMap::from([("X-Title".into(), "pi".into())])),
                base_url: Some("https://example.test".into()),
            },
            env: Some(BTreeMap::from([("FOO".into(), "bar".into())])),
            source: Some("OAuth".into()),
        };
        let value = serde_json::to_value(&result)?;
        assert_eq!(value["auth"]["apiKey"], json!("sk-test"));
        assert_eq!(value["auth"]["baseUrl"], json!("https://example.test"));
        assert_eq!(value["source"], json!("OAuth"));
        Ok(())
    }
}
