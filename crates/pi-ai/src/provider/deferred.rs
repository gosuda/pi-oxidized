//! Optional deferred-response callbacks on the [`Provider`] boundary.
//!
//! TypeScript parity (`ProviderStreams.fetchDeferred` / `cancelDeferred` in
//! `types.ts`): each callback is independently optional and a provider
//! advertises the capability by registering it — there is no separate
//! capability flag to keep in sync. `fetch` performs a single non-waiting
//! poll (`wait = 0`, see [`crate::provider::StreamOptionKey::WAIT`]) of a
//! deferred response; it never starts a fresh generation. `cancel` is a
//! best-effort cancellation of the same response.
//!
//! The record travels inside the provider object, so provider replacement or
//! removal drops stale callbacks with it. Built-in adapters register no
//! callbacks; dispatch then produces the source's unsupported errors.
//!
//! [`Provider`]: crate::provider::Provider
//! [`StreamOptionKey::WAIT`]: crate::provider::StreamOptionKey::WAIT

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::provider::{ProviderError, StreamOptions, error_event_stream};
use crate::types::{AssistantMessageEvent, DeferredHandle, Model};

/// Polls a provider-owned deferred response once without waiting.
///
/// Mirrors `ProviderStreams.fetchDeferred`: the returned stream carries the
/// canonical [`AssistantMessageEvent`] sequence for the polled response,
/// stamped with the model's provider metadata. The callback receives the
/// request's [`StreamOptions`] — including `signal`, `timeout_ms`, `headers`,
/// and `on_response` — and honors them the same way `stream` does.
pub type FetchDeferredFn = Arc<
    dyn for<'a> Fn(
            &'a Model,
            DeferredHandle,
            StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>
        + Send
        + Sync,
>;

/// Best-effort cancellation of a provider-owned deferred response.
///
/// Mirrors `ProviderStreams.cancelDeferred`.
pub type CancelDeferredFn = Arc<
    dyn for<'a> Fn(
            &'a Model,
            DeferredHandle,
            StreamOptions,
        ) -> BoxFuture<'static, Result<(), ProviderError>>
        + Send
        + Sync,
>;

/// Deferred-response callbacks registered on a provider.
///
/// Each entry is independently optional: capability is the presence of the
/// callback, never a separately maintained boolean. Providers keep the record
/// inside the provider object so re-registration or removal cannot leave a
/// stale callback behind.
#[derive(Clone, Default)]
pub struct DeferredCallbacks {
    /// Single non-waiting poll of a deferred response (`wait = 0`).
    pub fetch: Option<FetchDeferredFn>,
    /// Best-effort cancellation of a deferred response.
    pub cancel: Option<CancelDeferredFn>,
}

impl DeferredCallbacks {
    /// Whether a deferred fetch callback is currently registered.
    #[must_use]
    pub fn supports_fetch(&self) -> bool {
        self.fetch.is_some()
    }

    /// Whether a deferred cancel callback is currently registered.
    #[must_use]
    pub fn supports_cancel(&self) -> bool {
        self.cancel.is_some()
    }
}

/// The deferred operation a dispatch failure concerns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredOperation {
    /// `fetch_deferred` dispatch.
    Fetch,
    /// `cancel_deferred` dispatch.
    Cancel,
}

/// The source's unsupported-deferred message.
///
/// `api = None` mirrors `ModelsImpl` (`Provider X does not support deferred
/// responses`, raised when the provider has no matching callback at all);
/// `api = Some(..)` mirrors `createProvider`'s per-API dispatch.
pub(crate) fn unsupported_message(
    model: &Model,
    operation: DeferredOperation,
    api: Option<&str>,
) -> String {
    match (operation, api) {
        (DeferredOperation::Fetch | DeferredOperation::Cancel, None) => {
            format!(
                "Provider {} does not support deferred responses",
                model.provider
            )
        }
        (DeferredOperation::Fetch, Some(api)) => format!(
            "Provider {} does not support deferred responses for \"{api}\"",
            model.provider
        ),
        (DeferredOperation::Cancel, Some(api)) => format!(
            "Provider {} cannot cancel deferred responses for \"{api}\"",
            model.provider
        ),
    }
}

/// A fetch dispatch that ends in the source's unsupported error event.
///
/// The error surfaces as a terminal [`AssistantMessageEvent::Error`], matching
/// how `lazyStream` converts a thrown `ModelsError` into an error event.
pub(crate) fn unsupported_fetch_stream(
    model: &Model,
    api: Option<&str>,
) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
    error_event_stream(
        model,
        unsupported_message(model, DeferredOperation::Fetch, api),
    )
}

/// A cancel dispatch that fails with the source's unsupported error.
pub(crate) fn unsupported_cancel_future(
    model: &Model,
    api: Option<&str>,
) -> BoxFuture<'static, Result<(), ProviderError>> {
    let message = unsupported_message(model, DeferredOperation::Cancel, api);
    Box::pin(async move { Err(ProviderError::new(message)) })
}
