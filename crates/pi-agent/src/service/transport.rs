//! Native loopback transport traits shared by providers and bindings.

use std::sync::Arc;

use crate::context::Context;
use futures::future::BoxFuture;

use super::delta::DeltaOp;
use super::error::ServiceError;
use super::value::{JsString, JsonValue};
use super::wire::{ServiceCall, ServiceMode, ServiceProviderUpdate, ServiceSubscriptionSnapshot};

/// Listener invoked for provider updates.
pub type ServiceUpdateListener =
    Arc<dyn Fn(&ServiceProviderUpdate<DeltaOp>, &Context) + Send + Sync>;

/// A buffered provider subscription.
pub trait ServiceSubscription: Send + Sync {
    /// Returns the snapshot captured before activation.
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot<DeltaOp>;
    /// Enables delivery of queued and subsequent updates.
    fn activate(&self);
    /// Closes the subscription and releases its provider registration.
    fn close(&self, cx: Context) -> BoxFuture<'_, Result<(), ServiceError>>;
}

/// Invocation and subscription seam consumed by the native binding.
pub trait RemoteServiceTransport: Send + Sync {
    /// Invokes one method, preserving `None` versus `Some(JsonValue::Null)`.
    fn invoke(
        &self,
        call: ServiceCall,
        cx: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, ServiceError>>;
    /// Opens a buffered subscription whose snapshot precedes activation.
    fn subscribe(
        &self,
        service_id: JsString,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn ServiceSubscription>, ServiceError>>;
}
