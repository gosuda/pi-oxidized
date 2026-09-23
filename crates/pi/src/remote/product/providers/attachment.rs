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
use pi_agent::service::endpoint::{RemoteServiceEndpoint, ServiceUpdatePublisher};
use pi_agent::service::error::ServiceError;
use pi_agent::service::provider::RemoteServiceProvider;
use pi_agent::service::value::{JsString, JsonValue};
use pi_agent::service::wire::ServiceCall;
use tokio::sync::Notify;

use crate::remote::server::errors::{HostError, duplicate_host_error};
use crate::remote::server::host::{PublishUpdate, RoutedServerServiceAttachment};

type ReleaseCallback = Box<dyn FnOnce() + Send + 'static>;

/// Releases the endpoint and provider once, then invokes the owner-supplied
/// `on_release` callback.
pub struct ProviderAttachment {
    endpoint: Arc<RemoteServiceEndpoint>,
    provider: Arc<RemoteServiceProvider>,
    on_release: Arc<Mutex<Option<ReleaseCallback>>>,
    release_state: Arc<Mutex<ReleaseState>>,
}

enum ReleaseState {
    Idle {
        in_flight: usize,
        idle: Arc<Notify>,
    },
    Releasing {
        notify: Arc<Notify>,
        in_flight: usize,
        idle: Arc<Notify>,
    },
    Done(Result<(), Arc<HostError>>),
}

/// Keeps a service invocation admitted until its returned future is dropped.
struct InvocationGuard {
    state: Arc<Mutex<ReleaseState>>,
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        let idle = {
            let mut state = lock(&self.state);
            let (in_flight, notify) = match &mut *state {
                ReleaseState::Idle { in_flight, idle }
                | ReleaseState::Releasing {
                    in_flight, idle, ..
                } => (in_flight, idle),
                ReleaseState::Done(_) => return,
            };
            debug_assert!(*in_flight > 0);
            *in_flight = in_flight.saturating_sub(1);
            (*in_flight == 0).then(|| Arc::clone(notify))
        };
        if let Some(idle) = idle {
            idle.notify_waiters();
        }
    }
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
    /// `release` callback.
    pub(crate) fn new_unwrapped(
        provider: Arc<RemoteServiceProvider>,
        on_release: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            endpoint: RemoteServiceEndpoint::new(provider.clone()),
            provider,
            on_release: Arc::new(Mutex::new(Some(Box::new(on_release)))),
            release_state: Arc::new(Mutex::new(ReleaseState::Idle {
                in_flight: 0,
                idle: Arc::new(Notify::new()),
            })),
        }
    }

    async fn release_impl(&self, cx: Context) -> Result<(), HostError> {
        let mut start_release = None;
        let notified = {
            let mut state = lock(&self.release_state);
            match &mut *state {
                ReleaseState::Done(result) => {
                    return result
                        .clone()
                        .map_err(|error| duplicate_host_error(error.as_ref()));
                }
                ReleaseState::Releasing { notify, .. } => notify.clone().notified_owned(),
                ReleaseState::Idle { in_flight, idle } => {
                    let notify = Arc::new(Notify::new());
                    let notified = notify.clone().notified_owned();
                    let endpoint = Arc::clone(&self.endpoint);
                    let provider = Arc::clone(&self.provider);
                    let on_release = {
                        let mut callback = lock(&self.on_release);
                        callback.take()
                    };
                    let release_state = Arc::clone(&self.release_state);
                    let in_flight_notify = Arc::clone(idle);
                    let active_calls = *in_flight;
                    *state = ReleaseState::Releasing {
                        notify: Arc::clone(&notify),
                        in_flight: active_calls,
                        idle: in_flight_notify.clone(),
                    };
                    start_release = Some((
                        endpoint,
                        provider,
                        on_release,
                        release_state,
                        notify,
                        in_flight_notify,
                    ));
                    notified
                }
            }
        };

        if let Some((endpoint, provider, on_release, release_state, notify, in_flight_notify)) =
            start_release
        {
            tokio::spawn(async move {
                loop {
                    let wait = {
                        let state = lock(&release_state);
                        match &*state {
                            ReleaseState::Releasing { in_flight, .. } if *in_flight > 0 => {
                                Some(in_flight_notify.clone().notified_owned())
                            }
                            _ => None,
                        }
                    };
                    let Some(wait) = wait else {
                        break;
                    };
                    wait.await;
                }

                let mut guard = ReleaseGuard::new(release_state, notify);
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
        }

        notified.await;

        let state = lock(&self.release_state);
        if let ReleaseState::Done(result) = &*state {
            return result
                .as_ref()
                .copied()
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
        let admission = {
            let mut state = lock(&self.release_state);
            let ReleaseState::Idle { in_flight, .. } = &mut *state else {
                return Box::pin(std::future::ready(Err(HostError::Service(
                    ServiceError::disposed("Server service attachment is released"),
                ))));
            };
            *in_flight += 1;
            InvocationGuard {
                state: Arc::clone(&self.release_state),
            }
        };
        let endpoint = Arc::clone(&self.endpoint);
        let publisher = adapt_publisher(publish);

        Box::pin(async move {
            let _admission = admission;
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

/// Adapts the host `PublishUpdate` (`String` id, infallible delivery) to the
/// endpoint's `ServiceUpdatePublisher` (`JsString` id, fallible delivery).
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
            *guard = ReleaseState::Done(Err(Arc::new(HostError::Other(Box::new(PanicError(
                "release task panicked",
            ))))));
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use super::*;
    use pi_agent::service::provider::{
        RemoteServiceProvider, ServiceDefinition, ServiceImplementation, ServiceMember,
    };
    use pi_agent::service::wire::ServiceMode;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn release_waits_for_an_admitted_invocation() {
        let service_id = JsString::from_utf8("test.service");
        let member = JsString::from_utf8("wait");
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![ServiceDefinition {
                id: service_id.clone(),
                local: false,
                mode: ServiceMode::Singleton,
            }])
            .expect("provider"),
        );

        let (started_sender, started_receiver) = oneshot::channel();
        let (finish_sender, finish_receiver) = oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started_sender)));
        let finish = Arc::new(Mutex::new(Some(finish_receiver)));
        let mut implementation = ServiceImplementation::new();
        implementation.insert(
            member.clone(),
            ServiceMember::Method(Arc::new(move |_, _| {
                let started = lock(&started).take().expect("single invocation");
                let finish = lock(&finish).take().expect("single invocation");
                Box::pin(async move {
                    let _ = started.send(());
                    let _ = finish.await;
                    Ok(Some(JsonValue::Null))
                })
            })),
        );
        provider
            .provide(&service_id, implementation)
            .expect("provide method");

        let attachment = ProviderAttachment::new(Arc::clone(&provider), || {});
        let publish: PublishUpdate = Arc::new(|_, _, _| Box::pin(async {}));
        let call = ServiceCall {
            service_id,
            instance: None,
            member,
            args: Vec::new(),
        };
        let invoke_attachment = Arc::clone(&attachment);
        let invoke = tokio::spawn(async move {
            invoke_attachment
                .invoke_service(call, publish, Context::background())
                .await
        });
        started_receiver.await.expect("invocation started");

        let release_attachment = Arc::clone(&attachment);
        let release =
            tokio::spawn(async move { release_attachment.release(Context::background()).await });
        tokio::task::yield_now().await;
        assert!(
            !release.is_finished(),
            "release passed endpoint disposal while invocation was admitted"
        );

        finish_sender.send(()).expect("finish invocation");
        invoke
            .await
            .expect("invoke task")
            .expect("invocation result");
        release
            .await
            .expect("release task")
            .expect("release result");
    }
}
