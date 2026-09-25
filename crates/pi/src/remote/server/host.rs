//! Application-owned capabilities consumed by the routed server.

use super::errors::HostError;
use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::service::{
    delta::DeltaOp,
    value::JsonValue,
    wire::{ServiceCall, ServiceProviderUpdate},
};
use pi_agent::session::traits::SessionMetadataLike;
use std::sync::Arc;

/// Publishes a decoded service update for one subscription.
pub type PublishUpdate = Arc<
    dyn Fn(String, ServiceProviderUpdate<DeltaOp>, Context) -> BoxFuture<'static, ()> + Send + Sync,
>;

/// One presentation's capability for a hosted session.
pub trait RoutedSessionAttachment: Send + Sync {
    /// Invokes a service operation through this attachment.
    fn invoke_service(
        &self,
        call: ServiceCall,
        publish: PublishUpdate,
        cx: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, HostError>>;
    /// Releases the presentation capability.
    fn release(&self, cx: Context) -> BoxFuture<'_, Result<(), HostError>>;
}

/// Routing capabilities given to each server-service presentation.
pub trait RoutedServerPresentation: Send + Sync {
    /// Selects a session for this presentation.
    fn attach_session(
        &self,
        session_id: String,
        cx: Context,
    ) -> BoxFuture<'_, Result<(), HostError>>;
    /// Releases the selected session.
    fn detach_session(&self, cx: Context) -> BoxFuture<'_, Result<(), HostError>>;
    /// Releases attachments and handles before durable metadata removal.
    fn prepare_session_removal(
        &self,
        session_id: String,
        cx: Context,
    ) -> BoxFuture<'_, Result<(), HostError>>;
}

/// One connection's server-scoped service endpoint.
pub trait RoutedServerServiceAttachment: Send + Sync {
    /// Invokes a server-scoped service operation.
    fn invoke_service(
        &self,
        call: ServiceCall,
        publish: PublishUpdate,
        cx: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, HostError>>;
    /// Releases connection-owned services and subscriptions.
    fn release(&self, cx: Context) -> BoxFuture<'_, Result<(), HostError>>;
}

/// Acquires server-scoped service capabilities per connection.
pub trait RoutedServerServiceHost: Send + Sync {
    /// Creates an endpoint with routing authority limited to one presentation.
    fn attach_client(
        &self,
        presentation: Arc<dyn RoutedServerPresentation>,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn RoutedServerServiceAttachment>, HostError>>;
}

/// Process-safe handle acquiring presentation-scoped session capabilities.
pub trait RoutedSessionHandle: Send + Sync {
    /// Acquires one client attachment.
    fn attach_client(
        &self,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn RoutedSessionAttachment>, HostError>>;
    /// Optional one-shot termination signal; an error denotes unexpected termination.
    fn terminated(&self) -> Option<BoxFuture<'static, Option<HostError>>>;
    /// Closes the hosted session handle.
    fn close(&self, cx: Context) -> BoxFuture<'_, Result<(), HostError>>;
}

/// Repository and application composition for a routed server.
pub trait ServerHost: Send + Sync + 'static {
    /// Complete backend record, retained unchanged between resolution and opening.
    type Metadata: SessionMetadataLike;
    /// Reads canonical identity without discarding repository-specific fields.
    fn metadata_id<'a>(&self, metadata: &'a Self::Metadata) -> &'a str;
    /// Supplies the server-service capability factory.
    fn server_services(&self) -> &dyn RoutedServerServiceHost;
    /// Resolves a durable session identifier to its complete backend record.
    fn resolve_session(
        &self,
        session_id: String,
        cx: Context,
    ) -> BoxFuture<'_, Result<Self::Metadata, HostError>>;
    /// Opens the exact record returned by resolution.
    fn open_session(
        &self,
        metadata: Self::Metadata,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn RoutedSessionHandle>, HostError>>;
}
