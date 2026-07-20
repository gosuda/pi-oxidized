//! Provider-auth resolution with stored-credential ownership.
//!
//! Ports `.references/pi/packages/ai/src/auth/resolve.ts`:
//! - request overrides win when an API-key handler exists
//! - a stored credential owns the provider (no ambient/env fallback)
//! - OAuth uses a zero-lock fast path when unexpired, otherwise
//!   `CredentialStore::modify` with double-checked expiry so one refresh wins
//! - API-key credentials are expanded on copies (config-value templates /
//!   commands) without mutating disk/store contents

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config_value::{resolve_config_value, resolve_headers};
use super::error::{AuthError, ModelsError, ModelsErrorCode, StoreError};
use super::types::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthResult, Credential, CredentialStore, OAuthAuth,
    OAuthCredential, ProviderAuth, ProviderEnv,
};
use tokio_util::sync::CancellationToken;

/// Optional per-request auth overrides.
#[derive(Clone, Debug, Default)]
pub struct AuthResolutionOverrides {
    /// Explicit API key that bypasses stored credentials when the provider has
    /// an API-key handler.
    pub api_key: Option<String>,
    /// Provider-scoped environment overlay applied during resolution.
    pub env: Option<ProviderEnv>,
}

/// Resolve auth for a provider.
///
/// Precedence (first match wins):
/// 1. `overrides.api_key` when the provider has API-key auth
/// 2. stored credential (`oauth` / `api_key`); type mismatch → `None` (no env fallback)
/// 3. ambient API-key resolve when nothing is stored
///
/// A stored credential owns the provider. Failed OAuth refresh does not fall
/// back to environment keys.
///
/// # Errors
///
/// Returns [`ModelsErrorCode::Auth`] when the credential store fails, and
/// [`ModelsErrorCode::Oauth`] when refresh or auth derivation fails.
pub async fn resolve_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
) -> Result<Option<AuthResult>, ModelsError> {
    resolve_provider_auth_with_signal(
        provider_id,
        auth,
        credentials,
        auth_context,
        overrides,
        None,
    )
    .await
}

/// Resolve provider auth while allowing an in-flight stored OAuth refresh to be cancelled.
///
/// This additive entry point keeps [`resolve_provider_auth`] source-compatible for callers
/// without request-scoped cancellation.
///
/// # Errors
///
/// Returns [`ModelsErrorCode::Auth`] when the credential store fails, and
/// [`ModelsErrorCode::Oauth`] when refresh, cancellation, or auth derivation fails.
pub async fn resolve_provider_auth_with_signal(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
    signal: Option<CancellationToken>,
) -> Result<Option<AuthResult>, ModelsError> {
    let overlay_env = overrides.and_then(|value| value.env.as_ref());
    let request_context = EnvOverlayAuthContext {
        base: auth_context,
        env: overlay_env,
    };

    if let Some(api_key_override) = overrides.and_then(|value| value.api_key.as_ref())
        && let Some(api_key_auth) = auth.api_key.as_ref()
    {
        let credential = ApiKeyCredential {
            key: Some(api_key_override.clone()),
            env: overrides.and_then(|value| value.env.clone()),
        };
        return resolve_api_key(&request_context, api_key_auth.as_ref(), Some(&credential)).await;
    }

    let stored = read_credential(credentials, provider_id).await?;
    if let Some(stored) = stored {
        match stored {
            Credential::Oauth(oauth_cred) => {
                if let Some(oauth_auth) = auth.oauth.as_ref() {
                    return resolve_stored_oauth(
                        credentials,
                        provider_id,
                        Arc::clone(oauth_auth),
                        oauth_cred,
                        signal,
                    )
                    .await;
                }
                // Stored type without a matching handler — no ambient fallback.
                return Ok(None);
            }
            Credential::ApiKey(api_key_cred) => {
                if let Some(api_key_auth) = auth.api_key.as_ref() {
                    let credential = if let Some(override_env) = overlay_env {
                        merge_api_key_env(api_key_cred, override_env)
                    } else {
                        api_key_cred
                    };
                    return resolve_api_key(
                        &request_context,
                        api_key_auth.as_ref(),
                        Some(&credential),
                    )
                    .await;
                }
                // Stored type without a matching handler — no ambient fallback.
                return Ok(None);
            }
        }
    }

    // Ambient (env vars, AWS profiles, ADC files).
    if let Some(api_key_auth) = auth.api_key.as_ref() {
        return resolve_api_key(&request_context, api_key_auth.as_ref(), None).await;
    }
    Ok(None)
}

struct EnvOverlayAuthContext<'a> {
    base: &'a dyn AuthContext,
    env: Option<&'a ProviderEnv>,
}

impl AuthContext for EnvOverlayAuthContext<'_> {
    fn env<'a>(&'a self, name: &'a str) -> futures::future::BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if let Some(env) = self.env
                && let Some(value) = env.get(name)
                && !value.is_empty()
            {
                return Some(value.clone());
            }
            self.base.env(name).await
        })
    }

    fn file_exists<'a>(&'a self, path: &'a str) -> futures::future::BoxFuture<'a, bool> {
        self.base.file_exists(path)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn merge_api_key_env(
    mut credential: ApiKeyCredential,
    override_env: &ProviderEnv,
) -> ApiKeyCredential {
    let mut env = credential.env.take().unwrap_or_default();
    for (key, value) in override_env {
        env.insert(key.clone(), value.clone());
    }
    credential.env = Some(env);
    credential
}

/// Expand API-key credential fields on a **copy**.
///
/// Stored raw templates (`$ENV`, `!command`) remain unchanged in the store.
fn expand_api_key_copy(credential: &ApiKeyCredential) -> ApiKeyCredential {
    let env = credential.env.clone();
    let key = credential
        .key
        .as_ref()
        .and_then(|value| resolve_config_value(value, env.as_ref()));
    ApiKeyCredential { key, env }
}

/// Expand header maps on a **copy** with the same config-value rules.
fn expand_auth_result(mut result: AuthResult) -> AuthResult {
    if let Some(headers) = result.auth.headers.take() {
        result.auth.headers = resolve_headers(Some(&headers), result.env.as_ref());
    }
    result
}

async fn resolve_stored_oauth(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    oauth: Arc<dyn OAuthAuth>,
    stored: OAuthCredential,
    signal: Option<CancellationToken>,
) -> Result<Option<AuthResult>, ModelsError> {
    let mut credential = stored;

    if now_ms() >= credential.expires {
        // Optimistic check said expired; the authoritative check runs under the lock.
        let provider_id_owned = provider_id.to_owned();
        let oauth_for_refresh = Arc::clone(&oauth);
        let refresh_signal = signal.clone();
        let post = match credentials
            .modify(
                provider_id,
                Box::new(move |current| {
                    let oauth_for_refresh = Arc::clone(&oauth_for_refresh);
                    Box::pin(async move {
                        let Some(Credential::Oauth(current_oauth)) = current else {
                            // Logged out meanwhile — leave entry unchanged.
                            return Ok(None);
                        };
                        if now_ms() < current_oauth.expires {
                            // Another process/request refreshed — keep it.
                            return Ok(None);
                        }
                        oauth_for_refresh
                            .refresh(&current_oauth, refresh_signal)
                            .await
                            .map(|refreshed| Some(Credential::Oauth(refreshed)))
                    })
                }),
            )
            .await
        {
            Ok(post) => post,
            Err(StoreError::Auth(AuthError::Cancelled)) => {
                return Err(ModelsError::cancelled());
            }
            Err(StoreError::Auth(err)) => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Oauth,
                    format!("OAuth refresh failed for {provider_id_owned}: {err}"),
                ));
            }
            Err(err) => {
                return Err(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id_owned}: {err}"),
                ));
            }
        };

        match post {
            Some(Credential::Oauth(refreshed)) => credential = refreshed,
            // Logged out meanwhile, or non-oauth post state.
            _ => return Ok(None),
        }
    }

    match oauth.to_auth(&credential).await {
        Ok(model_auth) => Ok(Some(expand_auth_result(AuthResult {
            auth: model_auth,
            env: None,
            source: Some("OAuth".to_owned()),
        }))),
        Err(err) => Err(ModelsError::new(
            ModelsErrorCode::Oauth,
            format!("OAuth auth derivation failed for {provider_id}: {err}"),
        )),
    }
}

async fn resolve_api_key(
    auth_context: &dyn AuthContext,
    api_key: &dyn ApiKeyAuth,
    credential: Option<&ApiKeyCredential>,
) -> Result<Option<AuthResult>, ModelsError> {
    // Expand templates/commands on a copy so disk/store retain raw forms.
    let expanded = credential.map(expand_api_key_copy);
    // ApiKeyAuth::resolve is infallible at the trait surface. The TypeScript
    // try/catch maps thrown errors to ModelsError("auth"); Rust handlers return
    // Option without panicking under normal operation.
    let result = api_key
        .resolve(auth_context, expanded.as_ref())
        .await
        .map(expand_auth_result);
    Ok(result)
}

async fn read_credential(
    credentials: &dyn CredentialStore,
    provider_id: &str,
) -> Result<Option<Credential>, ModelsError> {
    credentials.read(provider_id).await.map_err(|err| {
        ModelsError::new(
            ModelsErrorCode::Auth,
            format!("Credential store read failed for {provider_id}: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::context::MapAuthContext;
    use crate::auth::credential_store::InMemoryCredentialStore;
    use crate::auth::env_keys::env_api_key_auth;
    use crate::auth::error::{AuthError, StoreError};
    use crate::auth::types::{CredentialInfo, ModelAuth};
    use futures::future::BoxFuture;
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    type TestResult = Result<(), Box<dyn Error>>;

    fn required<T>(value: Option<T>, message: &'static str) -> Result<T, io::Error> {
        value.ok_or_else(|| io::Error::other(message))
    }

    fn oauth_credential(access: &str, expires: i64) -> Credential {
        Credential::Oauth(OAuthCredential {
            refresh: "refresh".to_owned(),
            access: access.to_owned(),
            expires,
            extra: BTreeMap::new(),
        })
    }

    async fn put(
        store: &InMemoryCredentialStore,
        provider_id: &'static str,
        credential: Credential,
    ) -> Result<(), StoreError> {
        store
            .modify(
                provider_id,
                Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
            )
            .await?;
        Ok(())
    }

    fn api_key_auth(env_vars: &'static [&'static str]) -> ProviderAuth {
        ProviderAuth {
            api_key: Some(env_api_key_auth("API key", env_vars)),
            oauth: None,
        }
    }

    fn mixed_auth(oauth: Arc<dyn OAuthAuth>) -> ProviderAuth {
        ProviderAuth {
            api_key: Some(env_api_key_auth(
                "Anthropic API key",
                &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
            )),
            oauth: Some(oauth),
        }
    }

    struct MockOauth {
        refresh_count: AtomicUsize,
        fail_refresh: bool,
    }

    impl MockOauth {
        fn working() -> Self {
            Self {
                refresh_count: AtomicUsize::new(0),
                fail_refresh: false,
            }
        }

        fn failing() -> Self {
            Self {
                refresh_count: AtomicUsize::new(0),
                fail_refresh: true,
            }
        }
    }

    impl OAuthAuth for MockOauth {
        fn name(&self) -> &'static str {
            "Mock OAuth"
        }

        fn login_label(&self) -> Option<&str> {
            None
        }

        fn login<'a>(
            &'a self,
            _interaction: &'a dyn super::super::types::AuthInteraction,
        ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
            Box::pin(async {
                Ok(OAuthCredential {
                    refresh: "refresh".to_owned(),
                    access: "access".to_owned(),
                    expires: now_ms() + 60_000,
                    extra: BTreeMap::new(),
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            credential: &'a OAuthCredential,
            signal: Option<CancellationToken>,
        ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
            Box::pin(async move {
                if self.fail_refresh {
                    return Err(AuthError::message("invalid_grant"));
                }
                self.refresh_count.fetch_add(1, Ordering::SeqCst);
                if let Some(signal) = signal {
                    tokio::select! {
                        () = signal.cancelled() => return Err(AuthError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(OAuthCredential {
                    refresh: format!("{}-rotated", credential.refresh),
                    access: format!("{}-new", credential.access),
                    expires: now_ms() + 3_600_000,
                    extra: credential.extra.clone(),
                })
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

    struct CountingStore {
        inner: InMemoryCredentialStore,
        modify_count: AtomicUsize,
    }

    impl CountingStore {
        fn new(inner: InMemoryCredentialStore) -> Self {
            Self {
                inner,
                modify_count: AtomicUsize::new(0),
            }
        }
    }

    impl CredentialStore for CountingStore {
        fn read<'a>(
            &'a self,
            provider_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
            self.inner.read(provider_id)
        }

        fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
            self.inner.list()
        }

        fn modify<'a>(
            &'a self,
            provider_id: &'a str,
            operation: Box<
                dyn FnOnce(
                        Option<Credential>,
                    )
                        -> BoxFuture<'static, Result<Option<Credential>, AuthError>>
                    + Send
                    + 'a,
            >,
        ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
            self.modify_count.fetch_add(1, Ordering::SeqCst);
            self.inner.modify(provider_id, operation)
        }

        fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
            self.inner.delete(provider_id)
        }
    }

    #[tokio::test]
    async fn stored_api_key_beats_env_and_mismatch_blocks_fallback() -> TestResult {
        let stored = InMemoryCredentialStore::new();
        put(
            &stored,
            "openai",
            Credential::ApiKey(ApiKeyCredential {
                key: Some("stored".to_owned()),
                env: None,
            }),
        )
        .await?;
        let context = MapAuthContext::new().with_env("OPENAI_API_KEY", "ambient");
        let auth = api_key_auth(&["OPENAI_API_KEY"]);
        let resolved = required(
            resolve_provider_auth("openai", &auth, &stored, &context, None).await?,
            "stored auth",
        )?;
        assert_eq!(resolved.auth.api_key.as_deref(), Some("stored"));

        put(
            &stored,
            "openai",
            oauth_credential("oauth", now_ms() + 60_000),
        )
        .await?;
        assert!(
            resolve_provider_auth("openai", &auth, &stored, &context, None)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_failure_blocks_env_fallback() -> TestResult {
        let store = InMemoryCredentialStore::new();
        put(
            &store,
            "anthropic",
            oauth_credential("expired", now_ms() - 1),
        )
        .await?;
        let oauth = Arc::new(MockOauth::failing());
        let context = MapAuthContext::new().with_env("ANTHROPIC_API_KEY", "ambient");
        let result =
            resolve_provider_auth("anthropic", &mixed_auth(oauth), &store, &context, None).await;
        let Err(error) = result else {
            return Err(io::Error::other("refresh unexpectedly succeeded").into());
        };
        assert_eq!(error.code, ModelsErrorCode::Oauth);
        Ok(())
    }

    #[tokio::test]
    async fn unexpired_oauth_skips_modify_lock() -> TestResult {
        let inner = InMemoryCredentialStore::new();
        put(
            &inner,
            "anthropic",
            oauth_credential("valid", now_ms() + 60_000),
        )
        .await?;
        let store = CountingStore::new(inner);
        let oauth = Arc::new(MockOauth::working());
        let resolved = required(
            resolve_provider_auth(
                "anthropic",
                &mixed_auth(oauth.clone()),
                &store,
                &MapAuthContext::new(),
                None,
            )
            .await?,
            "valid oauth",
        )?;
        assert_eq!(resolved.auth.api_key.as_deref(), Some("valid"));
        assert_eq!(store.modify_count.load(Ordering::SeqCst), 0);
        assert_eq!(oauth.refresh_count.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_expired_resolves_refresh_once() -> TestResult {
        let store = InMemoryCredentialStore::new();
        put(
            &store,
            "anthropic",
            oauth_credential("expired", now_ms() - 1),
        )
        .await?;
        let oauth = Arc::new(MockOauth::working());
        let auth = mixed_auth(oauth.clone());
        let context = MapAuthContext::new();
        let (first, second) = tokio::join!(
            resolve_provider_auth("anthropic", &auth, &store, &context, None),
            resolve_provider_auth("anthropic", &auth, &store, &context, None),
        );
        let first = required(first?, "first auth")?;
        let second = required(second?, "second auth")?;
        assert_eq!(first.auth.api_key, second.auth.api_key);
        assert_eq!(oauth.refresh_count.load(Ordering::SeqCst), 1);

        let stored = store.read("anthropic").await?;
        let Some(Credential::Oauth(stored)) = stored else {
            return Err(io::Error::other("oauth credential missing").into());
        };
        assert!(stored.expires > now_ms());
        assert!(stored.refresh.ends_with("-rotated"));
        Ok(())
    }

    #[tokio::test]
    async fn stored_refresh_honors_request_cancellation() -> TestResult {
        let store = InMemoryCredentialStore::new();
        put(
            &store,
            "anthropic",
            oauth_credential("expired", now_ms() - 1),
        )
        .await?;
        let oauth = Arc::new(MockOauth::working());
        let auth = mixed_auth(oauth.clone());
        let context = MapAuthContext::new();
        let signal = CancellationToken::new();

        let resolve = resolve_provider_auth_with_signal(
            "anthropic",
            &auth,
            &store,
            &context,
            None,
            Some(signal.clone()),
        );
        let cancel = async {
            while oauth.refresh_count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            signal.cancel();
        };
        let (result, ()) = tokio::join!(resolve, cancel);
        let error = result
            .err()
            .ok_or_else(|| io::Error::other("cancelled refresh succeeded"))?;
        assert_eq!(error.code, ModelsErrorCode::Oauth);
        assert_eq!(error.message(), "Login cancelled");
        assert!(error.is_cancelled());

        let stored = store.read("anthropic").await?;
        let Some(Credential::Oauth(stored)) = stored else {
            return Err(io::Error::other("oauth credential missing").into());
        };
        assert_eq!(stored.access, "expired");
        assert!(stored.expires <= now_ms());
        Ok(())
    }

    #[tokio::test]
    async fn api_key_expansion_uses_copy_and_preserves_raw_store() -> TestResult {
        let store = InMemoryCredentialStore::new();
        let env = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "expanded".to_owned())]);
        put(
            &store,
            "openai",
            Credential::ApiKey(ApiKeyCredential {
                key: Some("$OPENAI_API_KEY".to_owned()),
                env: Some(env),
            }),
        )
        .await?;
        let resolved = required(
            resolve_provider_auth(
                "openai",
                &api_key_auth(&["OPENAI_API_KEY"]),
                &store,
                &MapAuthContext::new(),
                None,
            )
            .await?,
            "expanded auth",
        )?;
        assert_eq!(resolved.auth.api_key.as_deref(), Some("expanded"));

        let stored = store.read("openai").await?;
        let Some(Credential::ApiKey(stored)) = stored else {
            return Err(io::Error::other("api-key credential missing").into());
        };
        assert_eq!(stored.key.as_deref(), Some("$OPENAI_API_KEY"));
        Ok(())
    }
}
