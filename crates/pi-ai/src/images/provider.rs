//! Image-generation provider contract and the parts-based provider builder.
//!
//! Ports `packages/ai/src/images-models.ts` provider half: an
//! [`ImagesProvider`] owns id/name metadata, auth handlers, the current model
//! list, and generation behavior. [`create_images_provider`] assembles one
//! from parts, with the frozen shared-in-flight refresh semantics: concurrent
//! refresh calls share one fetch, the stored list stays at its last-known
//! state when the fetch fails, and a later call retries.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::OnceCell;

use super::api_registry::ImagesApiDispatch;
use super::types::{AssistantImages, ImagesContext, ImagesModel, ImagesOptions};
use crate::auth::error::ModelsError;
use crate::auth::types::ProviderAuth;

/// An image-generation provider: the image-side counterpart of the chat
/// [`crate::provider::Provider`] collection surface.
///
/// Implementations must be [`Send`] + [`Sync`] because providers are shared
/// across tasks. [`ImagesProvider::get_models`] is a sync, best-effort read:
/// static providers return their catalog and dynamic providers return the
/// list as of the last successful refresh (empty before the first).
pub trait ImagesProvider: Send + Sync {
    /// Provider identifier used by catalogs and auth storage.
    fn id(&self) -> &str;

    /// Display name.
    fn name(&self) -> &str;

    /// Auth handlers. At least one of `api_key`/`oauth` is present for
    /// configured providers; [`crate::images::ImagesModels::get_auth`]
    /// resolves to `None` when the provider is unconfigured.
    fn auth(&self) -> &ProviderAuth;

    /// Current known models.
    fn get_models(&self) -> Vec<ImagesModel>;

    /// Fetch and update the model list.
    ///
    /// `None` marks a static provider (refresh is a no-op). A failed refresh
    /// keeps the stored list at its last-known state; the error propagates to
    /// the caller and a later call retries.
    fn refresh_models(&self) -> Option<BoxFuture<'_, Result<Vec<ImagesModel>, ModelsError>>> {
        None
    }

    /// Generate images through this provider.
    ///
    /// Implementations never panic and never return infrastructure errors:
    /// request failures are encoded in the returned [`AssistantImages`] with
    /// an `Error` or `Aborted` stop reason.
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages>;
}

/// Closure fetching the current model list for a dynamic provider.
pub type ImagesRefreshFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<ImagesModel>, ModelsError>> + Send + Sync>;

/// One in-flight model-list fetch shared by concurrent refresh callers.
type SharedRefreshCell = Arc<OnceCell<Result<Vec<ImagesModel>, ModelsError>>>;

/// Shared-in-flight refresh state with the frozen semantics.
///
/// Concurrent callers awaiting the same refresh share one fetch via a
/// once-cell; the slot is cleared when the fetch settles so a later call
/// retries.
struct SharedRefresh {
    inflight: Mutex<Option<SharedRefreshCell>>,
    refresh: Option<ImagesRefreshFn>,
}

impl SharedRefresh {
    fn new(refresh: Option<ImagesRefreshFn>) -> Self {
        Self {
            inflight: Mutex::new(None),
            refresh,
        }
    }

    fn has_refresh(&self) -> bool {
        self.refresh.is_some()
    }

    async fn run(&self) -> Result<Vec<ImagesModel>, ModelsError> {
        let Some(refresh) = self.refresh.clone() else {
            return Ok(Vec::new());
        };

        let cell = {
            let mut slot = self
                .inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cell) = slot.as_ref() {
                Arc::clone(cell)
            } else {
                let cell = Arc::new(OnceCell::new());
                *slot = Some(Arc::clone(&cell));
                cell
            }
        };

        let result = cell
            .get_or_init(|| async move { refresh().await })
            .await
            .clone();

        let mut slot = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &cell))
        {
            *slot = None;
        }
        result
    }
}

struct BuiltImagesProvider {
    id: String,
    name: String,
    auth: ProviderAuth,
    models: Mutex<Vec<ImagesModel>>,
    refresh: SharedRefresh,
    api: Arc<dyn ImagesApiDispatch>,
}

impl ImagesProvider for BuiltImagesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<ImagesModel> {
        // Upstream treats a throwing getModels as "no models"; a sync native
        // read cannot throw, so the list is returned directly.
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn refresh_models(&self) -> Option<BoxFuture<'_, Result<Vec<ImagesModel>, ModelsError>>> {
        if !self.refresh.has_refresh() {
            return None;
        }
        Some(Box::pin(async move {
            let models = self.refresh.run().await?;
            // The frozen builder assigns `models = await refreshModels()` on
            // success only; a failed fetch leaves the previous list stored.
            self.models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone_from(&models);
            Ok(models)
        }))
    }

    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages> {
        let api = Arc::clone(&self.api);
        let model = model.clone();
        Box::pin(async move { api.generate_images(&model, context, options).await })
    }
}

/// Parts for [`create_images_provider`], mirroring the frozen
/// `CreateImagesProviderOptions`.
pub struct CreateImagesProviderOptions {
    /// Provider identifier.
    pub id: String,
    /// Display name. Default: `id`.
    pub name: Option<String>,
    /// Auth handlers. Every provider has auth semantics, even ambient or
    /// keyless ones.
    pub auth: ProviderAuth,
    /// Initial model list (empty for purely dynamic providers).
    pub models: Vec<ImagesModel>,
    /// Dynamic providers: fetch the current list.
    pub refresh_models: Option<ImagesRefreshFn>,
    /// API dispatcher used for generation.
    pub api: Arc<dyn ImagesApiDispatch>,
}

/// Build an image-generation provider from parts.
#[must_use]
pub fn create_images_provider(options: CreateImagesProviderOptions) -> Arc<dyn ImagesProvider> {
    Arc::new(BuiltImagesProvider {
        name: options.name.unwrap_or_else(|| options.id.clone()),
        id: options.id,
        auth: options.auth,
        models: Mutex::new(options.models),
        refresh: SharedRefresh::new(options.refresh_models),
        api: options.api,
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::auth::error::ModelsErrorCode;
    use crate::images::types::{ImagesOutput, OPENROUTER_IMAGES_API};
    use crate::types::ModelInput;

    fn model(id: &str) -> ImagesModel {
        ImagesModel::new(
            id,
            "Test",
            OPENROUTER_IMAGES_API,
            "openrouter",
            "https://image.example.test",
            vec![ModelInput::Text],
            vec![ImagesOutput::Image],
        )
    }

    struct NoopDispatch;

    impl ImagesApiDispatch for NoopDispatch {
        fn generate_images(
            &self,
            _model: &ImagesModel,
            _context: ImagesContext,
            _options: ImagesOptions,
        ) -> BoxFuture<'static, AssistantImages> {
            Box::pin(async { unreachable!("provider builder test never generates") })
        }
    }

    fn provider_with(
        models: Vec<ImagesModel>,
        refresh: Option<ImagesRefreshFn>,
    ) -> Arc<dyn ImagesProvider> {
        create_images_provider(CreateImagesProviderOptions {
            id: "openrouter".to_owned(),
            name: Some("OpenRouter".to_owned()),
            auth: ProviderAuth::default(),
            models,
            refresh_models: refresh,
            api: Arc::new(NoopDispatch),
        })
    }

    #[tokio::test]
    async fn dynamic_refresh_shares_one_inflight_fetch() {
        let fetches = Arc::new(AtomicU32::new(0));
        let fetches_for_closure = Arc::clone(&fetches);
        let refresh: ImagesRefreshFn = Arc::new(move || {
            let fetches = Arc::clone(&fetches_for_closure);
            Box::pin(async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Ok(vec![model("fetched-1")])
            })
        });
        let provider = provider_with(Vec::new(), Some(refresh));

        let [Some(first), Some(second), Some(third)] = [
            provider.refresh_models(),
            provider.refresh_models(),
            provider.refresh_models(),
        ] else {
            unreachable!("dynamic provider exposes refresh");
        };
        let (first, second, third) = tokio::join!(first, second, third);
        assert_eq!(first.expect("refresh succeeds").len(), 1);
        assert_eq!(second.expect("refresh succeeds").len(), 1);
        assert_eq!(third.expect("refresh succeeds").len(), 1);
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "one shared fetch");
        assert_eq!(provider.get_models().len(), 1);
        assert_eq!(provider.get_models()[0].id, "fetched-1");

        // A completed refresh clears the slot: the next call fetches again.
        provider
            .refresh_models()
            .expect("dynamic provider exposes refresh")
            .await
            .expect("second refresh succeeds");
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_last_known_models() {
        let fail: ImagesRefreshFn = Arc::new(|| {
            Box::pin(async {
                Err(ModelsError::new(
                    ModelsErrorCode::ModelSource,
                    "fetch failed",
                ))
            })
        });
        let provider = provider_with(vec![model("static-1")], Some(fail));

        let error = provider
            .refresh_models()
            .expect("dynamic provider exposes refresh")
            .await
            .expect_err("refresh fails");
        assert_eq!(error.message(), "fetch failed");
        assert_eq!(provider.get_models().len(), 1);
        assert_eq!(provider.get_models()[0].id, "static-1");
    }

    #[test]
    fn static_provider_has_no_refresh_and_defaults_name_to_id() {
        let provider = create_images_provider(CreateImagesProviderOptions {
            id: "openrouter".to_owned(),
            name: None,
            auth: ProviderAuth::default(),
            models: Vec::new(),
            refresh_models: None,
            api: Arc::new(NoopDispatch),
        });
        assert_eq!(provider.name(), "openrouter");
        assert!(provider.refresh_models().is_none());
        assert!(provider.get_models().is_empty());
    }
}
