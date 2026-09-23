//! Runtime registry of image-generation API dispatchers.
//!
//! Ports `packages/ai/src/images-api-registry.ts`: an api-string-keyed map
//! from [`ImagesApi`] identifiers to generation dispatchers. The built-in
//! `OpenRouter` dispatcher is registered when the registry is first touched,
//! standing in for the frozen `providers/images/register-builtins.ts` import
//! side effect; later registrations replace existing entries per key.
//!
//! The frozen `wrapGenerateImages` guard throws on `model.api !== api`. In
//! Rust the dispatcher is only reachable through the exact api key it was
//! registered under, so a mismatch cannot be constructed; the top-level
//! [`generate_images`](crate::images::generate_images) lookup is the single
//! dispatch boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, RwLock};

use futures::future::BoxFuture;

use super::openrouter::openrouter_images_api;
use super::types::{AssistantImages, ImagesApi, ImagesContext, ImagesModel, ImagesOptions};

/// Object-safe image-generation entry point for one [`ImagesApi`] shape.
///
/// The image-side counterpart of the chat [`crate::provider::Provider`]
/// stream functions: implementations own the transport, never panic, and
/// encode every request failure inside the returned [`AssistantImages`] with
/// an `Error` or `Aborted` stop reason.
pub trait ImagesApiDispatch: Send + Sync {
    /// Generate images for `model` from `context`.
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages>;
}

static IMAGES_API_REGISTRY: LazyLock<RwLock<BTreeMap<ImagesApi, Arc<dyn ImagesApiDispatch>>>> =
    LazyLock::new(|| {
        let mut registry: BTreeMap<ImagesApi, Arc<dyn ImagesApiDispatch>> = BTreeMap::new();
        registry.insert(
            super::types::OPENROUTER_IMAGES_API.to_owned(),
            openrouter_images_api(),
        );
        RwLock::new(registry)
    });

/// Register or replace the dispatcher for `api`.
///
/// Mirrors the frozen `registerImagesApiProvider`: the last registration for
/// an api wins, so custom providers can override built-ins.
pub fn register_images_api_provider(
    api: impl Into<ImagesApi>,
    dispatch: Arc<dyn ImagesApiDispatch>,
) {
    let mut registry = IMAGES_API_REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    drop(registry.insert(api.into(), dispatch));
}

/// Look up the dispatcher registered for `api`.
#[must_use]
pub fn get_images_api_provider(api: &str) -> Option<Arc<dyn ImagesApiDispatch>> {
    IMAGES_API_REGISTRY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(api)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::types::OPENROUTER_IMAGES_API;

    #[test]
    fn builtin_openrouter_dispatcher_is_registered_once() {
        let first = get_images_api_provider(OPENROUTER_IMAGES_API);
        let second = get_images_api_provider(OPENROUTER_IMAGES_API);
        assert!(first.is_some());
        assert!(second.is_some());
    }

    #[test]
    fn unknown_api_has_no_dispatcher() {
        assert!(get_images_api_provider("not-an-images-api").is_none());
    }
}
