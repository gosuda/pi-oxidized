//! Integration tests for `ModelRuntime::login` and hardened `logout`.
//!
//! Covers: API-key and OAuth login credential persistence, unknown-provider
//! and unsupported-method error paths, and the logout synchronization-failure
//! wording when the post-delete refresh cannot complete within the timeout.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use pi::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsJsonConfig};
use pi_ai::auth::types::CredentialModifyFn;
use pi_ai::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthType, Credential, CredentialInfo,
    CredentialKind, CredentialStore, InMemoryCredentialStore, ModelAuth, OAuthAuth,
    OAuthCredential, StoreError,
};
use pi_ai::models_store::InMemoryModelsStore;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Queued-prompt `AuthInteraction` for deterministic login tests.
///
/// Each `prompt` call pops the next scripted answer. Events are recorded for
/// assertions. A whole-flow cancellation token can be attached.
struct ScriptedInteraction {
    prompts: Mutex<VecDeque<Result<String, AuthError>>>,
    events: Mutex<Vec<AuthEvent>>,
    signal: Option<CancellationToken>,
}

impl ScriptedInteraction {
    fn new(prompts: Vec<Result<String, AuthError>>) -> Self {
        Self {
            prompts: Mutex::new(prompts.into()),
            events: Mutex::new(Vec::new()),
            signal: None,
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "test: single-threaded Mutex lock never poisons"
)]
impl AuthInteraction for ScriptedInteraction {
    fn prompt(&self, _prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
        let answer = self
            .prompts
            .lock()
            .expect("prompts lock")
            .pop_front()
            .unwrap_or_else(|| Err(AuthError::message("no more scripted prompts")));
        Box::pin(async move { answer })
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().expect("events lock").push(event);
    }

    fn signal(&self) -> Option<CancellationToken> {
        self.signal.clone()
    }
}

/// Minimal `OAuthAuth` whose `login` always succeeds with a fake credential.
struct FakeOAuthAuth;

impl OAuthAuth for FakeOAuthAuth {
    fn name(&self) -> &'static str {
        "Fake OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Fake")
    }

    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move {
            Ok(OAuthCredential {
                refresh: "fake-refresh".into(),
                access: "fake-access".into(),
                expires: i64::MAX,
                extra: BTreeMap::new(),
            })
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

/// Credential store whose `list` never completes.
///
/// Used to stall the post-delete `refresh_availability` call inside `logout`
/// so the 15-second timeout fires. With `start_paused = true`, tokio
/// fast-forwards simulated time past the deadline almost instantly.
struct HangingListCredentialStore {
    inner: InMemoryCredentialStore,
}

impl HangingListCredentialStore {
    fn new() -> Self {
        Self {
            inner: InMemoryCredentialStore::new(),
        }
    }
}

impl CredentialStore for HangingListCredentialStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        self.inner.read(provider_id)
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        // Sleep long enough that the 15s logout timeout fires first.
        // Uses tokio::time::sleep so the paused clock can advance past it.
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            self.inner.list().await
        })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: Box<CredentialModifyFn<'a>>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        self.inner.modify(provider_id, f)
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        self.inner.delete(provider_id)
    }
}

fn find_credential<'a>(
    credentials: &'a [CredentialInfo],
    provider_id: &str,
) -> Option<&'a CredentialInfo> {
    credentials
        .iter()
        .find(|entry| entry.provider_id == provider_id)
}

// ---------------------------------------------------------------------------
// Login tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_api_key_writes_credential() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(Arc::new(InMemoryCredentialStore::new())),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;

    let interaction = Arc::new(ScriptedInteraction::new(vec![Ok("sk-test-key".into())]))
        as Arc<dyn AuthInteraction>;
    runtime
        .login("openai", AuthType::ApiKey, interaction)
        .await?;

    let credentials = runtime.list_credentials().await?;
    let entry =
        find_credential(&credentials, "openai").ok_or("openai credential missing after login")?;
    assert_eq!(entry.kind, CredentialKind::ApiKey);
    Ok(())
}

#[tokio::test]
async fn login_oauth_writes_credential() -> Result<(), Box<dyn std::error::Error>> {
    let mut handlers = std::collections::HashMap::new();
    handlers.insert(
        "fake-oauth".to_owned(),
        Arc::new(FakeOAuthAuth) as Arc<dyn OAuthAuth>,
    );

    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(Arc::new(InMemoryCredentialStore::new())),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        oauth_handlers: Some(handlers),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;

    let interaction = Arc::new(ScriptedInteraction::new(vec![])) as Arc<dyn AuthInteraction>;
    runtime
        .login("fake-oauth", AuthType::Oauth, interaction)
        .await?;

    let credentials = runtime.list_credentials().await?;
    let entry = find_credential(&credentials, "fake-oauth")
        .ok_or("fake-oauth credential missing after login")?;
    assert_eq!(entry.kind, CredentialKind::Oauth);
    Ok(())
}

// ---------------------------------------------------------------------------
// Error-path tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_unknown_provider_oauth_returns_error() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(Arc::new(InMemoryCredentialStore::new())),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;

    let interaction = Arc::new(ScriptedInteraction::new(vec![])) as Arc<dyn AuthInteraction>;
    let Err(error) = runtime
        .login("nonexistent-provider", AuthType::Oauth, interaction)
        .await
    else {
        return Err("unknown provider OAuth should fail".into());
    };

    let message = error.to_string();
    assert!(
        message.contains("nonexistent-provider"),
        "error must name the provider: {message}"
    );
    assert!(
        message.contains("OAuth"),
        "error must mention OAuth: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn login_unsupported_oauth_for_api_key_only_provider()
-> Result<(), Box<dyn std::error::Error>> {
    // "openai" has API-key auth but no builtin OAuth handler.
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(Arc::new(InMemoryCredentialStore::new())),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;
    let interaction = Arc::new(ScriptedInteraction::new(vec![])) as Arc<dyn AuthInteraction>;
    let Err(error) = runtime.login("openai", AuthType::Oauth, interaction).await else {
        return Err("OAuth on api-key-only provider should fail".into());
    };

    let message = error.to_string();
    assert!(
        message.contains("openai"),
        "error must name the provider: {message}"
    );
    assert!(
        message.contains("OAuth"),
        "error must mention OAuth: {message}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Logout tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_succeeds_after_login() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(Arc::new(InMemoryCredentialStore::new())),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;

    let interaction = Arc::new(ScriptedInteraction::new(vec![Ok("sk-test-key".into())]))
        as Arc<dyn AuthInteraction>;
    runtime
        .login("openai", AuthType::ApiKey, interaction)
        .await?;

    // Credential should be present.
    assert!(
        find_credential(&runtime.list_credentials().await?, "openai").is_some(),
        "credential should exist before logout"
    );

    runtime.logout("openai").await?;

    // Credential should be gone.
    assert!(
        find_credential(&runtime.list_credentials().await?, "openai").is_none(),
        "credential should be removed after logout"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn logout_sync_error_when_refresh_times_out() -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(HangingListCredentialStore::new());

    // Pre-populate a credential so delete has something to remove.
    store
        .modify(
            "openai",
            Box::new(|_| {
                Box::pin(async {
                    Ok(Some(Credential::ApiKey(pi_ai::auth::ApiKeyCredential {
                        key: Some("sk-pre".into()),
                        env: None,
                    })))
                })
            }),
        )
        .await?;

    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(store),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        models_config: Some(ModelsJsonConfig::empty()),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await?;

    // Spawn logout so the main task can advance the paused clock past the
    // 15-second timeout while the spawned task is blocked on the hanging
    // `list()` inside `refresh_availability`.
    let rt = runtime.clone();
    let handle = tokio::spawn(async move { rt.logout("openai").await });

    // Yield to let the spawned task progress through delete + rebuild and
    // register the 15s timeout timer.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // Advance past the 15-second timeout.
    tokio::time::advance(std::time::Duration::from_secs(16)).await;

    let error = match handle.await {
        Ok(Err(error)) => error,
        Ok(Ok(())) => return Err("logout should return sync error when refresh hangs".into()),
        Err(_) => return Err("logout task should not panic".into()),
    };

    let message = error.to_string();
    assert!(
        matches!(
            error,
            pi::core::model_runtime::ModelRuntimeError::CredentialSynchronization { .. }
        ),
        "logout timeout must surface the typed sync error: {message}"
    );
    assert!(
        message.contains("Credential logout committed for openai"),
        "error must name the operation and provider: {message}"
    );
    assert!(
        message.contains("timed out"),
        "error must mention timeout: {message}"
    );
    Ok(())
}
