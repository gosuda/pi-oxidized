//! Provider attachment that routes one `RemoteServiceEndpoint` behind the
//! routed-server `RoutedServerServiceAttachment` seam.
//!
//! The adapter is deliberately thin: the endpoint owns subscriptions and
//! subscription control, while this module only adapts the host `PublishUpdate`
//! callback (which uses `String` subscription ids) to the endpoint's
//! `ServiceUpdatePublisher` (which uses canonical `JsString` ids), maps service
//! errors to `HostError`, and performs an idempotent awaited release that
//! finishes `endpoint.dispose`, `provider.dispose` and `on_release` even when
//! the observer drops the returned future.

use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::service::delta::DeltaOp;
use pi_agent::service::endpoint::{RemoteServiceEndpoint, ServiceUpdatePublisher};
use pi_agent::service::error::ServiceError;
use pi_agent::service::provider::RemoteServiceProvider;
use pi_agent::service::value::{JsString, JsonValue};
use pi_agent::service::wire::{ServiceCall, ServiceProviderUpdate};
use tokio::sync::Notify;

use crate::remote::server::errors::{duplicate_host_error, HostError};
use crate::remote::server::host::{PublishUpdate, RoutedServerServiceAttachment};

/// Releases the endpoint and provider once, then invokes the owner-supplied
/// `on_release` callback.
pub struct ProviderAttachment {
    endpoint: Arc<RemoteServiceEndpoint>,
    provider: Arc<RemoteServiceProvider>,
    on_release: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>,
    release_state: Arc<Mutex<ReleaseState>>,
}

#[derive(Clone)]
enum ReleaseState {
    Idle,
    Releasing(Arc<Notify>),
    Done(Result<(), Arc<HostError>>),
}

impl ProviderAttachment {
    /// Creates an attachment over one provider.
    #[must_use]
    pub fn new(
        provider: Arc<RemoteServiceProvider>,
        on_release: impl FnOnce() + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self::new_unwrapped(provider, on_release))
    }

    /// Creates the attachment value without wrapping it in an `Arc`.
    ///
    /// This is an internal seam used by `server_services` so it can create the
    /// `Arc` with `Arc::new_cyclic` and capture a weak reference for the
    /// release callback.
    pub(crate) fn new_unwrapped(
        provider: Arc<RemoteServiceProvider>,
        on_release: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            endpoint: RemoteServiceEndpoint::new(provider.clone()),
            provider,
            on_release: Arc::new(Mutex::new(Some(Box::new(on_release)))),
            release_state: Arc::new(Mutex::new(ReleaseState::Idle)),
        }
    }

    async fn release_impl(&self, cx: Context) -> Result<(), HostError> {
        let notify = loop {
            let mut guard = lock(&self.release_state);
            match std::mem::replace(&mut *guard, ReleaseState::Idle) {
                ReleaseState::Done(result) => {
                    *guard = ReleaseState::Done(result.clone());
                    return result
                        .as_ref()
                        .map(|_| ())
                        .map_err(|error| duplicate_host_error(error.as_ref()));
                }
                ReleaseState::Releasing(notify) => {
                    *guard = ReleaseState::Releasing(notify.clone());
                    break notify;
                }
                ReleaseState::Idle => {
                    let notify = Arc::new(Notify::new());
                    *guard = ReleaseState::Releasing(notify.clone());

                    let endpoint = Arc::clone(&self.endpoint);
                    let provider = Arc::clone(&self.provider);
                    let on_release = {
                        let mut guard = lock(&self.on_release);
                        guard.take()
                    };
                    let state = Arc::clone(&self.release_state);
                    let notify_done = Arc::clone(&notify);

                    tokio::spawn(async move {
                        let mut guard = ReleaseGuard::new(state, notify_done);

                        let mut first_error: Option<HostError> = None;
                        if let Err(error) = endpoint.dispose(cx).await {
                            first_error.get_or_insert(HostError::from(error));
                        }
                        provider.dispose();
                        if let Some(on_release) = on_release {
                            on_release();
                        }

                        let result = match first_error {
                            Some(error) => Err(Arc::new(error)),
                            None => Ok(()),
                        };
                        guard.complete(result);
                    });

                    break notify;
                }
            }
        };

        notify.notified().await;

        let guard = lock(&self.release_state);
        if let ReleaseState::Done(result) = &*guard {
            return result
                .as_ref()
                .map(|_| ())
                .map_err(|error| duplicate_host_error(error.as_ref()));
        }
        unreachable!("release state must be Done after notification")
    }
}

impl RoutedServerServiceAttachment for ProviderAttachment {
    fn invoke_service(
        &self,
        call: ServiceCall,
        publish: PublishUpdate,
        cx: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, HostError>> {
        {
            let guard = lock(&self.release_state);
            if !matches!(&*guard, ReleaseState::Idle) {
                return Box::pin(std::future::ready(Err(HostError::Service(
                    ServiceError::disposed("Server service attachment is released"),
                ))));
            }
        }

        let endpoint = Arc::clone(&self.endpoint);
        let publisher = adapt_publisher(publish);

        Box::pin(async move {
            endpoint
                .invoke(call, publisher, cx)
                .await
                .map_err(map_endpoint_error)
        })
    }

    fn release(&self, cx: Context) -> BoxFuture<'_, Result<(), HostError>> {
        Box::pin(self.release_impl(cx))
    }
}

/// Adapts the host `PublishUpdate` (String id, infallible delivery) to the
/// endpoint's `ServiceUpdatePublisher` (JsString id, fallible delivery).
///
/// Subscription ids are converted from canonical UTF-16 to a Rust `String` at
/// this boundary.  An invalid id is reported as a local `ServiceError`; the
/// endpoint publishes updates through one ordered worker per subscription and
/// terminates a subscription whose delivery fails.
fn adapt_publisher(publish: PublishUpdate) -> ServiceUpdatePublisher {
    Arc::new(move |subscription_id: JsString, update, context| {
        let id = match subscription_id.try_to_utf8() {
            Ok(id) => id,
            Err(error) => {
                return Box::pin(std::future::ready(Err(ServiceError::local(format!(
                    "Invalid subscription id: {error}"
                )))));
            }
        };
        let publish = Arc::clone(&publish);
        Box::pin(async move {
            publish(id, update, context).await;
            Ok(())
        })
    })
}

/// Maps endpoint errors back to `HostError`.
///
/// Service methods may preserve a host error by wrapping it in
/// `ServiceError::Handler` with a `HostError` source.  When that happens we
/// restore the original `HostError` so the routed server can use the correct
/// protocol code; otherwise the `ServiceError` is wrapped as `HostError::Service`.
fn map_endpoint_error(error: ServiceError) -> HostError {
    match error {
        ServiceError::Handler { source } => {
            if let Some(host) = source.downcast_ref::<HostError>() {
                return duplicate_host_error(host);
            }
            HostError::Service(ServiceError::Handler { source })
        }
        other => HostError::Service(other),
    }
}

/// Ensures the release state is marked `Done` even if the release task panics.
struct ReleaseGuard {
    state: Arc<Mutex<ReleaseState>>,
    notify: Arc<Notify>,
    completed: bool,
}

impl ReleaseGuard {
    fn new(state: Arc<Mutex<ReleaseState>>, notify: Arc<Notify>) -> Self {
        Self {
            state,
            notify,
            completed: false,
        }
    }

    fn complete(&mut self, result: Result<(), Arc<HostError>>) {
        {
            let mut guard = lock(&self.state);
            *guard = ReleaseState::Done(result);
        }
        self.completed = true;
        self.notify.notify_waiters();
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        {
            let mut guard = lock(&self.state);
            *guard = ReleaseState::Done(Err(Arc::new(HostError::Other(Box::new(
                PanicError("release task panicked"),
            )))));
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug)]
struct PanicError(&'static str);

impl fmt::Display for PanicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for PanicError {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
