//! Runtime collection of image-generation providers.
//!
//! Ports the `ImagesModels`/`MutableImagesModels` half of
//! `packages/ai/src/images-models.ts`: provider registration, best-effort
//! model reads, refresh fan-out, auth resolution, and the never-rejecting
//! generation wrapper that resolves auth, merges explicit request options
//! per field, and returns failures as error [`AssistantImages`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, join_all};
use indexmap::IndexMap;

use super::provider::ImagesProvider;
use super::types::{AssistantImages, ImagesContext, ImagesModel, ImagesOptions};
use crate::auth::context::DefaultAuthContext;
use crate::auth::credential_store::InMemoryCredentialStore;
use crate::auth::error::{ModelsError, ModelsErrorCode};
use crate::auth::resolve::{
    AuthResolutionOverrides, resolve_provider_auth, resolve_provider_auth_with_signal,
};
use crate::auth::types::{AuthContext, AuthResult, CredentialStore, ProviderEnv, ProviderHeaders};
use crate::provider::now_millis;

/// Construction options for [`create_images_models`], mirroring the subset of
/// the frozen `CreateModelsOptions` that the image collection consumes.
#[derive(Clone, Default)]
pub struct ImagesModelsOptions {
    /// App-owned credential storage. Default: in-memory store.
    pub credentials: Option<Arc<dyn CredentialStore>>,
    /// Environment access for auth resolution. Default: process environment.
    pub auth_context: Option<Arc<dyn AuthContext>>,
}

/// Read-only collection of image-generation providers with auth resolution
/// and generation convenience.
pub trait ImagesModels: Send + Sync {
    /// Every registered provider, in registration order.
    fn get_providers(&self) -> Vec<Arc<dyn ImagesProvider>>;

    /// Look up one provider by id.
    fn get_provider(&self, id: &str) -> Option<Arc<dyn ImagesProvider>>;

    /// Sync read of last-known models from one provider or all providers.
    ///
    /// Best-effort: a provider whose model read fails yields no models.
    fn get_models(&self, provider: Option<&str>) -> Vec<ImagesModel>;

    /// Sync runtime model lookup against last-known lists.
    fn get_model(&self, provider: &str, id: &str) -> Option<ImagesModel>;

    /// Ask dynamic providers to re-fetch their model lists.
    ///
    /// With a provider id, a failed refresh fails with the provider's
    /// [`ModelsError`]; unknown or static providers are no-ops. Without one,
    /// every provider refreshes concurrently best-effort and failures are
    /// captured without failing the call.
    fn refresh<'a>(&'a self, provider: Option<&'a str>) -> BoxFuture<'a, Result<(), ModelsError>>;

    /// Resolve request auth by provider id.
    ///
    /// `Ok(None)` when the provider is unknown or unconfigured;
    /// [`ModelsError`] with the `auth`/`oauth` codes on real failures.
    fn get_auth<'a>(
        &'a self,
        provider_id: &'a str,
        overrides: Option<&'a AuthResolutionOverrides>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, ModelsError>>;

    /// Resolve request auth for the provider owning `model`.
    fn get_auth_for_model<'a>(
        &'a self,
        model: &'a ImagesModel,
        overrides: Option<&'a AuthResolutionOverrides>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, ModelsError>> {
        Box::pin(self.get_auth(&model.provider, overrides))
    }

    /// Generate images through the owning provider with auth resolved and
    /// merged (explicit options win per field).
    ///
    /// Never panics: every failure — unknown provider, auth resolution,
    /// transport — is returned as an [`AssistantImages`] with an `Error`
    /// stop reason.
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'_, AssistantImages>;
}

/// Mutable provider registration on an [`ImagesModels`] collection.
pub trait MutableImagesModels: ImagesModels {
    /// Upsert a provider; provider ids are unique, so the last registration
    /// for an id replaces the previous entry.
    fn set_provider(&self, provider: Arc<dyn ImagesProvider>);

    /// Remove one provider by id.
    fn delete_provider(&self, id: &str);

    /// Remove every provider.
    fn clear_providers(&self);
}

struct ImagesModelsImpl {
    providers: Mutex<IndexMap<String, Arc<dyn ImagesProvider>>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
}

impl ImagesModels for ImagesModelsImpl {
    fn get_providers(&self) -> Vec<Arc<dyn ImagesProvider>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn get_provider(&self, id: &str) -> Option<Arc<dyn ImagesProvider>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    fn get_models(&self, provider: Option<&str>) -> Vec<ImagesModel> {
        let Some(provider_id) = provider else {
            let mut models = Vec::new();
            for entry in self.get_providers() {
                models.extend(entry.get_models());
            }
            return models;
        };
        match self.get_provider(provider_id) {
            Some(entry) => entry.get_models(),
            None => Vec::new(),
        }
    }

    fn get_model(&self, provider: &str, id: &str) -> Option<ImagesModel> {
        self.get_models(Some(provider))
            .into_iter()
            .find(|model| model.id == id)
    }

    fn refresh<'a>(&'a self, provider: Option<&'a str>) -> BoxFuture<'a, Result<(), ModelsError>> {
        Box::pin(async move {
            let Some(provider_id) = provider else {
                // The frozen all-providers path never rejects: every
                // per-provider failure is captured by the fan-out.
                let entries = self.get_providers();
                let refreshes = entries.iter().filter_map(|entry| entry.refresh_models());
                drop(join_all(refreshes).await);
                return Ok(());
            };

            let Some(entry) = self.get_provider(provider_id) else {
                return Ok(());
            };
            let Some(refresh) = entry.refresh_models() else {
                return Ok(());
            };
            refresh.await.map(drop)
        })
    }

    fn get_auth<'a>(
        &'a self,
        provider_id: &'a str,
        overrides: Option<&'a AuthResolutionOverrides>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, ModelsError>> {
        Box::pin(async move {
            let Some(provider) = self.get_provider(provider_id) else {
                return Ok(None);
            };
            resolve_provider_auth(
                provider_id,
                provider.auth(),
                self.credentials.as_ref(),
                self.auth_context.as_ref(),
                overrides,
            )
            .await
        })
    }

    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'_, AssistantImages> {
        let model = model.clone();
        Box::pin(async move {
            let result = generate_with_auth(self, &model, context, options).await;
            match result {
                Ok(result) => result,
                Err(error) => {
                    let mut failed = AssistantImages::new(&model, now_millis());
                    failed.fail(
                        crate::images::types::ImagesStopReason::Error,
                        error.message(),
                    );
                    failed
                }
            }
        })
    }
}

impl MutableImagesModels for ImagesModelsImpl {
    fn set_provider(&self, provider: Arc<dyn ImagesProvider>) {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.id().to_owned(), provider);
    }

    fn delete_provider(&self, id: &str) {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shift_remove(id);
    }

    fn clear_providers(&self) {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

async fn generate_with_auth(
    models: &ImagesModelsImpl,
    model: &ImagesModel,
    context: ImagesContext,
    mut options: ImagesOptions,
) -> Result<AssistantImages, ModelsError> {
    let Some(provider) = models.get_provider(&model.provider) else {
        return Err(ModelsError::new(
            ModelsErrorCode::Provider,
            format!("Unknown provider: {}", model.provider),
        ));
    };

    let overrides = AuthResolutionOverrides {
        api_key: options.api_key.clone(),
        env: options.env.clone(),
    };
    let resolution = resolve_provider_auth_with_signal(
        &model.provider,
        provider.auth(),
        models.credentials.as_ref(),
        models.auth_context.as_ref(),
        Some(&overrides),
        options.signal.clone(),
    )
    .await?;

    let Some(resolution) = resolution else {
        // Unconfigured provider: the adapter reports its own missing-auth
        // error through the normal result path.
        return Ok(provider.generate_images(model, context, options).await);
    };

    let auth = &resolution.auth;
    // Per-credential base URL override applies before option merge.
    let request_model = match auth.base_url.as_deref() {
        Some(base_url) => {
            let mut overridden = model.clone();
            base_url.clone_into(&mut overridden.base_url);
            overridden
        }
        None => model.clone(),
    };

    // Explicit request options win per field; headers/env merge per key.
    // The provider's registered model contributes its headers as model
    // defaults under explicit option headers.
    let api_key = options.api_key.clone().or_else(|| auth.api_key.clone());
    let registered_headers = provider
        .get_models()
        .into_iter()
        .find(|registered| registered.id == model.id)
        .and_then(|registered| registered.headers)
        .or_else(|| model.headers.clone())
        .map(|headers| {
            headers
                .into_iter()
                .map(|(name, value)| (name, Some(value)))
                .collect()
        });
    let headers = merge_optional_headers(
        merge_optional_headers(auth.headers.as_ref(), registered_headers).as_ref(),
        options.headers.take(),
    );
    let env = merge_optional_env(resolution.env.as_ref(), options.env.take());
    options.api_key = api_key;
    options.headers = headers;
    options.env = env;

    Ok(provider
        .generate_images(&request_model, context, options)
        .await)
}

fn merge_optional_headers(
    base: Option<&ProviderHeaders>,
    override_headers: Option<ProviderHeaders>,
) -> Option<ProviderHeaders> {
    if base.is_none() && override_headers.is_none() {
        return None;
    }
    let mut merged: ProviderHeaders = base.map_or_else(BTreeMap::new, <_>::clone);
    if let Some(override_headers) = override_headers {
        for (name, value) in override_headers {
            merged.insert(name, value);
        }
    }
    Some(merged)
}

fn merge_optional_env(
    base: Option<&ProviderEnv>,
    override_env: Option<ProviderEnv>,
) -> Option<ProviderEnv> {
    if base.is_none() && override_env.is_none() {
        return None;
    }
    let mut merged: ProviderEnv = base.map_or_else(BTreeMap::new, <_>::clone);
    if let Some(override_env) = override_env {
        for (name, value) in override_env {
            merged.insert(name, value);
        }
    }
    Some(merged)
}

/// Create a runtime image-generation collection with defaults from
/// `options`.
#[must_use]
pub fn create_images_models(options: ImagesModelsOptions) -> Arc<dyn MutableImagesModels> {
    Arc::new(ImagesModelsImpl {
        providers: Mutex::new(IndexMap::new()),
        credentials: options
            .credentials
            .unwrap_or_else(|| Arc::new(InMemoryCredentialStore::default())),
        auth_context: options
            .auth_context
            .unwrap_or_else(|| Arc::new(DefaultAuthContext::from_process())),
    })
}
