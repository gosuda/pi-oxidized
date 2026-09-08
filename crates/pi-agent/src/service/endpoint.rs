//! Native endpoint that adapts Chord service control calls to one provider.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::BoxFuture;
use tokio::sync::Notify;

use crate::context::Context;

use super::delta::DeltaOp;
use super::error::ServiceError;
use super::provider::RemoteServiceProvider;
use super::transport::{ServiceSubscription, ServiceUpdateListener};
use super::value::{JsString, JsonValue};
use super::wire::{
    ServiceCall, ServiceControlCall, ServiceMode, ServiceProviderUpdate, decode_service_control_call,
};

/// Delivers one provider update to the transport owner.
///
/// The callback owns all values crossing the endpoint boundary.  It returns a
/// future so the endpoint can schedule delivery without holding provider or
/// endpoint state while transport I/O runs.
pub type ServiceUpdatePublisher = Arc<
    dyn Fn(
            JsString,
            ServiceProviderUpdate<DeltaOp>,
            Context,
        ) -> BoxFuture<'static, Result<(), ServiceError>>
        + Send
        + Sync,
>;

struct EndpointState {
    subscriptions: BTreeMap<JsString, Arc<dyn ServiceSubscription>>,
    pending_ids: BTreeSet<JsString>,
    closing_ids: BTreeSet<JsString>,
    pending_operations: usize,
    disposed: bool,
}

struct EndpointInner {
    state: Mutex<EndpointState>,
    idle: Notify,
}

impl EndpointInner {
    fn new() -> Self {
        Self {
            state: Mutex::new(EndpointState {
                subscriptions: BTreeMap::new(),
                pending_ids: BTreeSet::new(),
                closing_ids: BTreeSet::new(),
                pending_operations: 0,
                disposed: false,
            }),
            idle: Notify::new(),
        }
    }

    fn complete(&self, reservation: Option<Reservation>) {
        let notify = {
            let mut state = lock(&self.state);
            match reservation {
                Some(Reservation::Pending(id)) => {
                    state.pending_ids.remove(&id);
                }
                Some(Reservation::Closing(id)) => {
                    state.closing_ids.remove(&id);
                }
                None => {}
            }
            debug_assert!(state.pending_operations > 0);
            state.pending_operations -= 1;
            state.pending_operations == 0
        };
        if notify {
            self.idle.notify_one();
        }
    }


    fn admit_unsubscribe(
        self: &Arc<Self>,
        id: &JsString,
    ) -> Result<(Arc<dyn ServiceSubscription>, PendingOperation), ServiceError> {
        let mut state = lock(&self.state);
        if state.disposed {
            return Err(ServiceError::disposed("Remote service endpoint is disposed"));
        }
        let subscription = state
            .subscriptions
            .remove(id)
            .ok_or_else(|| ServiceError::local("Service subscription was not found"))?;
        state.closing_ids.insert(id.clone());
        state.pending_operations += 1;
        Ok((
            subscription,
            PendingOperation::new(Arc::clone(self), Some(Reservation::Closing(id.clone()))),
        ))
    }

    fn register_subscription(
        &self,
        id: &JsString,
        subscription: Arc<dyn ServiceSubscription>,
    ) -> bool {
        let mut state = lock(&self.state);
        state.pending_ids.remove(id);
        if state.disposed {
            return false;
        }
        let previous = state.subscriptions.insert(id.clone(), subscription);
        debug_assert!(previous.is_none());
        true
    }

    fn is_disposed(&self) -> bool {
        lock(&self.state).disposed
    }

    fn begin_dispose(&self) -> bool {
        let mut state = lock(&self.state);
        if state.disposed {
            return false;
        }
        state.disposed = true;
        true
    }

    fn take_subscriptions(&self) -> Vec<Arc<dyn ServiceSubscription>> {
        let mut state = lock(&self.state);
        std::mem::take(&mut state.subscriptions)
            .into_values()
            .collect()
    }
}


enum Reservation {
    Pending(JsString),
    Closing(JsString),
}

struct PendingOperation {
    inner: Arc<EndpointInner>,
    reservation: Option<Reservation>,
    finished: bool,
}

impl PendingOperation {
    fn new(inner: Arc<EndpointInner>, reservation: Option<Reservation>) -> Self {
        Self {
            inner,
            reservation,
            finished: false,
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.inner.complete(self.reservation.take());
    }
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Hosts one provider and owns the provider subscriptions for one consumer.
pub struct RemoteServiceEndpoint {
    provider: Arc<RemoteServiceProvider>,
    inner: Arc<EndpointInner>,
}

impl std::fmt::Debug for RemoteServiceEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteServiceEndpoint")
            .field("provider", &self.provider)
            .field("disposed", &self.inner.is_disposed())
            .finish_non_exhaustive()
    }
}

impl RemoteServiceEndpoint {
    /// Creates an endpoint over one provider.
    #[must_use]
    pub fn new(provider: Arc<RemoteServiceProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            inner: Arc::new(EndpointInner::new()),
        })
    }

    /// Decodes a control call or delegates an ordinary service invocation.
    pub async fn invoke(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> Result<Option<JsonValue>, ServiceError> {
        if self.inner.is_disposed() {
            return Err(ServiceError::disposed("Remote service endpoint is disposed"));
        }
        match decode_service_control_call(&call) {
            Some(ServiceControlCall::Catalogue) => {
                let catalogue = self
                    .provider
                    .catalogue()
                    .iter()
                    .cloned()
                    .map(super::wire::ServiceCatalogueEntry::into_json)
                    .collect();
                Ok(Some(JsonValue::Array(catalogue)))
            }
            Some(ServiceControlCall::Subscribe {
                subscription_id,
                service_id,
                mode,
            }) => self.subscribe(subscription_id, service_id, mode, publish, context).await,
            Some(ServiceControlCall::Unsubscribe { subscription_id }) => {
                self.unsubscribe(&subscription_id, context).await
            }
            None => self.provider.invoke(call, context).await,
        }
    }

    /// Closes all admitted subscriptions and waits for each close future.
    pub async fn dispose(&self, context: Context) -> Result<(), ServiceError> {
        if !self.inner.begin_dispose() {
            return Ok(());
        }
        wait_for_idle(Arc::clone(&self.inner)).await;
        let subscriptions = self.inner.take_subscriptions();
        let mut first_error = None;
        for subscription in subscriptions {
            if let Err(error) = subscription.close(context.clone()).await {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
    async fn subscribe(
        &self,
        subscription_id: JsString,
        service_id: JsString,
        mode: ServiceMode,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> Result<Option<JsonValue>, ServiceError> {
        let mut pending = self.admit_subscribe(subscription_id.clone())?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service endpoint requires a Tokio runtime"))?;
        let listener = update_listener(subscription_id.clone(), publish, runtime);
        let subscription = match self
            .provider
            .subscribe(service_id, mode, listener, context.clone())
            .await
        {
            Ok(subscription) => subscription,
            Err(error) => return Err(error),
        };
        if !self
            .inner
            .register_subscription(&subscription_id, Arc::clone(&subscription))
        {
            let close_result = subscription.close(context).await;
            pending.finish();
            return match close_result {
                Ok(()) => Err(ServiceError::disposed("Remote service endpoint is disposed")),
                Err(error) => Err(error),
            };
        }
        subscription.activate();
        let snapshot = subscription.snapshot().clone().into_json();
        pending.finish();
        Ok(Some(snapshot))
    }

    async fn unsubscribe(
        &self,
        subscription_id: &JsString,
        context: Context,
    ) -> Result<Option<JsonValue>, ServiceError> {
        let (subscription, mut pending) = self.inner.admit_unsubscribe(subscription_id)?;
        let result = subscription.close(context).await;
        pending.finish();
        result.map(|()| None)
    }

    fn admit_subscribe(&self, id: JsString) -> Result<PendingOperation, ServiceError> {
        let mut state = lock(&self.inner.state);
        if state.disposed {
            return Err(ServiceError::disposed("Remote service endpoint is disposed"));
        }
        if state.subscriptions.contains_key(&id)
            || state.pending_ids.contains(&id)
            || state.closing_ids.contains(&id)
        {
            return Err(ServiceError::local("Service subscription ID is already active"));
        }
        state.pending_ids.insert(id.clone());
        state.pending_operations += 1;
        Ok(PendingOperation::new(
            Arc::clone(&self.inner),
            Some(Reservation::Pending(id)),
        ))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn wait_for_idle(inner: Arc<EndpointInner>) {
    loop {
        let notified = inner.idle.notified();
        if lock(&inner.state).pending_operations == 0 {
            return;
        }
        notified.await;
    }
}

fn update_listener(
    subscription_id: JsString,
    publish: ServiceUpdatePublisher,
    runtime: tokio::runtime::Handle,
) -> ServiceUpdateListener {
    Arc::new(move |update, context| {
        let future = publish(subscription_id.clone(), update.clone(), context.clone());
        runtime.spawn(async move {
            let _ = future.await;
        });
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "endpoint tests use contextual fixture failures")]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use tokio::time::{timeout, Duration};
    use super::super::provider::{ServiceDefinition, ServiceImplementation, ServiceMember};
    use super::super::replicated::MutableReplicatedState;
    use super::super::wire::{
        ServiceMode, ServiceProviderUpdate, create_service_subscribe_call,
        create_service_unsubscribe_call,
    };
    use super::*;

    fn object(count: f64) -> JsonValue {
        JsonValue::Object(BTreeMap::from([(
            JsString::from_utf8("count"),
            JsonValue::Number(count),
        )]))
    }

    fn endpoint_with_state() -> (Arc<RemoteServiceEndpoint>, Arc<MutableReplicatedState>) {
        let state = MutableReplicatedState::new(object(0.0));
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![ServiceDefinition {
                id: JsString::from_utf8("svc"),
                local: false,
                mode: ServiceMode::Singleton,
            }])
            .expect("service definition"),
        );
        let mut implementation = ServiceImplementation::new();
        implementation.insert(JsString::from_utf8("state"), ServiceMember::State(Arc::clone(&state)));
        provider
            .provide(&JsString::from_utf8("svc"), implementation)
            .expect("service implementation");
        (RemoteServiceEndpoint::new(provider), state)
    }

    fn publisher(
        updates: Arc<Mutex<Vec<(JsString, ServiceProviderUpdate<DeltaOp>)>>>,
    ) -> ServiceUpdatePublisher {
        Arc::new(move |subscription_id, update, _context| {
            let updates = Arc::clone(&updates);
            Box::pin(async move {
                lock(&updates).push((subscription_id, update));
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn rejects_duplicate_and_missing_subscriptions() {
        let (endpoint, _state) = endpoint_with_state();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let publish = publisher(Arc::clone(&updates));
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                Arc::clone(&publish),
                Context::background(),
            )
            .await
            .expect("subscribe");
        let duplicate = endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect_err("duplicate subscription");
        assert_eq!(duplicate.to_string(), "Service subscription ID is already active");
        let missing = endpoint
            .invoke(
                create_service_unsubscribe_call("missing"),
                publisher(Arc::clone(&updates)),
                Context::background(),
            )
            .await
            .expect_err("missing subscription");
        assert_eq!(missing.to_string(), "Service subscription was not found");
        endpoint.dispose(Context::background()).await.expect("dispose");
    }

    #[tokio::test]
    async fn activates_before_returning_snapshot_and_publishes_updates() {
        let (endpoint, state) = endpoint_with_state();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let publish = publisher(Arc::clone(&updates));
        let snapshot = endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe")
            .expect("snapshot");
        let mode = snapshot
            .as_object()
            .and_then(|object| object.get(&JsString::from_utf8("mode")))
            .and_then(JsonValue::as_str);
        assert_eq!(mode, Some(&JsString::from_utf8("singleton")));
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish state");
        timeout(Duration::from_secs(1), async {
            while lock(&updates).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publisher delivery");
        let delivered = lock(&updates);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, JsString::from_utf8("subscription"));
        endpoint.dispose(Context::background()).await.expect("dispose");
    }

    #[tokio::test]
    async fn disposal_race_closes_a_subscription_admitted_concurrently() {
        let (endpoint, state) = endpoint_with_state();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let subscribe_endpoint = Arc::clone(&endpoint);
        let subscribe_updates = publisher(Arc::clone(&updates));
        let subscribe = async move {
            subscribe_endpoint
                .invoke(
                    create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                    subscribe_updates,
                    Context::background(),
                )
                .await
        };
        let dispose_endpoint = Arc::clone(&endpoint);
        let dispose = async move { dispose_endpoint.dispose(Context::background()).await };
        let (_subscribe_result, dispose_result) = tokio::join!(subscribe, dispose);
        dispose_result.expect("dispose");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish state");
        tokio::task::yield_now().await;
        assert!(lock(&updates).is_empty(), "disposed endpoint cannot publish updates");
    }
}
