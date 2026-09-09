//! Native endpoint that adapts Chord service control calls to one provider.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::context::Context;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::{Notify, mpsc};

use super::delta::DeltaOp;
use super::error::ServiceError;
use super::provider::RemoteServiceProvider;
use super::transport::{ServiceSubscription, ServiceUpdateListener};
use super::value::{JsString, JsonValue};
use super::wire::{
    ServiceCall, ServiceControlCall, ServiceMode, ServiceProviderUpdate,
    decode_service_control_call,
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

struct EndpointSubscription {
    subscription: Arc<dyn ServiceSubscription>,
    token: Arc<()>,
}

struct EndpointState {
    subscriptions: BTreeMap<JsString, EndpointSubscription>,
    delivery_errors: BTreeMap<JsString, ServiceError>,
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
                delivery_errors: BTreeMap::new(),
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
            return Err(ServiceError::disposed(
                "Remote service endpoint is disposed",
            ));
        }
        let subscription = state
            .subscriptions
            .remove(id)
            .ok_or_else(|| ServiceError::local("Service subscription was not found"))?;
        state.closing_ids.insert(id.clone());
        state.pending_operations += 1;
        Ok((
            subscription.subscription,
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
        let previous = state.subscriptions.insert(
            id.clone(),
            EndpointSubscription {
                subscription,
                token: Arc::new(()),
            },
        );
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
            .map(|entry| entry.subscription)
            .collect()
    }

    /// Records a delivery failure and removes the subscription so no further
    /// updates are accepted.  The caller closes the removed subscription.
    fn fail_subscription(
        &self,
        id: &JsString,
        token: Option<&Arc<()>>,
        error: ServiceError,
    ) -> Option<Arc<dyn ServiceSubscription>> {
        let mut state = lock(&self.state);
        if !state
            .subscriptions
            .get(id)
            .is_some_and(|entry| token.is_some_and(|token| Arc::ptr_eq(&entry.token, token)))
        {
            return None;
        }
        state.delivery_errors.insert(id.clone(), error);
        state
            .subscriptions
            .remove(id)
            .map(|entry| entry.subscription)
    }

    fn admit_delivery(self: &Arc<Self>) -> Option<PendingOperation> {
        let mut state = lock(&self.state);
        if state.disposed {
            return None;
        }
        state.pending_operations += 1;
        Some(PendingOperation::new(Arc::clone(self), None))
    }

    /// Takes the recorded delivery failure for one subscription, if any.
    fn take_delivery_error(&self, id: &JsString) -> Option<ServiceError> {
        lock(&self.state).delivery_errors.remove(id)
    }

    /// Drains every recorded delivery failure.
    fn take_delivery_errors(&self) -> BTreeMap<JsString, ServiceError> {
        std::mem::take(&mut lock(&self.state).delivery_errors)
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
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the endpoint is disposed, when a control
    /// payload fails to decode, when the provider rejects the invocation, or
    /// when subscribing/unsubscribing reports a delivery failure.
    pub async fn invoke(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: Context,
    ) -> Result<Option<JsonValue>, ServiceError> {
        if self.inner.is_disposed() {
            return Err(ServiceError::disposed(
                "Remote service endpoint is disposed",
            ));
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
            }) => {
                self.subscribe(subscription_id, service_id, mode, publish, context)
                    .await
            }
            Some(ServiceControlCall::Unsubscribe { subscription_id }) => {
                self.unsubscribe(&subscription_id, context).await
            }
            None => self.provider.invoke(call, context).await,
        }
    }

    /// Closes all admitted subscriptions and waits for each close future.
    ///
    /// # Errors
    ///
    /// Returns the first [`ServiceError`] recorded while closing subscriptions,
    /// including delivery failures the ordered worker reported before dispose.
    pub async fn dispose(&self, context: Context) -> Result<(), ServiceError> {
        if !self.inner.begin_dispose() {
            return Ok(());
        }
        wait_for_idle(Arc::clone(&self.inner)).await;
        let subscriptions = self.inner.take_subscriptions();
        let mut first_error = self.inner.take_delivery_errors().into_values().next();
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
        tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service endpoint requires a Tokio runtime"))?;
        let (sender, receiver) =
            mpsc::unbounded_channel::<(ServiceProviderUpdate<DeltaOp>, Context)>();
        let listener = update_listener(sender);
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
                Ok(()) => Err(ServiceError::disposed(
                    "Remote service endpoint is disposed",
                )),
                Err(error) => Err(error),
            };
        }
        subscription.activate();
        let snapshot = subscription.snapshot().clone().into_json();
        spawn_update_worker(subscription_id, receiver, publish, Arc::clone(&self.inner));
        pending.finish();
        Ok(Some(snapshot))
    }

    async fn unsubscribe(
        &self,
        subscription_id: &JsString,
        context: Context,
    ) -> Result<Option<JsonValue>, ServiceError> {
        if let Some(error) = self.inner.take_delivery_error(subscription_id) {
            return Err(error);
        }
        let (subscription, mut pending) = self.inner.admit_unsubscribe(subscription_id)?;
        let result = subscription.close(context).await;
        pending.finish();
        result.map(|()| None)
    }

    fn admit_subscribe(&self, id: JsString) -> Result<PendingOperation, ServiceError> {
        let mut state = lock(&self.inner.state);
        if state.disposed {
            return Err(ServiceError::disposed(
                "Remote service endpoint is disposed",
            ));
        }
        if state.subscriptions.contains_key(&id)
            || state.pending_ids.contains(&id)
            || state.closing_ids.contains(&id)
        {
            return Err(ServiceError::local(
                "Service subscription ID is already active",
            ));
        }
        state.delivery_errors.remove(&id);
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

/// Queues provider updates for one subscription's ordered delivery worker.
///
/// The provider invokes the listener in publication order, so the listener
/// only enqueues each update and delivery order survives publish futures that
/// would complete out of order.  Updates enqueued before the delivery worker
/// starts — including entries drained by activation — cannot reach the
/// consumer before the subscription snapshot is established, because the
/// endpoint starts the worker only after capturing it.
fn update_listener(
    sender: mpsc::UnboundedSender<(ServiceProviderUpdate<DeltaOp>, Context)>,
) -> ServiceUpdateListener {
    Arc::new(
        move |update: &ServiceProviderUpdate<DeltaOp>, context: &Context| {
            let _ = sender.send((update.clone(), context.clone()));
        },
    )
}

/// Drains one subscription's queued updates in publication order and
/// publishes each sequentially.  A failed or panicking publish terminates the
/// subscription and records the delivery error for the next control call
/// rather than leaving the subscription active and silently missing updates.
fn spawn_update_worker(
    subscription_id: JsString,
    mut receiver: mpsc::UnboundedReceiver<(ServiceProviderUpdate<DeltaOp>, Context)>,
    publish: ServiceUpdatePublisher,
    inner: Arc<EndpointInner>,
) {
    let token = lock(&inner.state)
        .subscriptions
        .get(&subscription_id)
        .map(|entry| Arc::clone(&entry.token));
    tokio::spawn(async move {
        while let Some((update, context)) = receiver.recv().await {
            let Some(_pending) = inner.admit_delivery() else {
                return;
            };
            let result = std::panic::AssertUnwindSafe(async {
                publish(subscription_id.clone(), update, context.clone()).await
            })
            .catch_unwind()
            .await;
            let result = result.unwrap_or_else(|_| {
                Err(ServiceError::internal("service update delivery panicked"))
            });
            if let Err(error) = result {
                if let Some(subscription) =
                    inner.fail_subscription(&subscription_id, token.as_ref(), error)
                {
                    let _ = subscription.close(context).await;
                }
                return;
            }
        }
    });
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "endpoint tests use contextual fixture failures"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::super::provider::{ServiceDefinition, ServiceImplementation, ServiceMember};
    use super::super::replicated::MutableReplicatedState;
    use super::super::wire::{
        ServiceMode, ServiceProviderUpdate, create_service_subscribe_call,
        create_service_unsubscribe_call,
    };
    use super::*;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

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
        implementation.insert(
            JsString::from_utf8("state"),
            ServiceMember::State(Arc::clone(&state)),
        );
        provider
            .provide(&JsString::from_utf8("svc"), implementation)
            .expect("service implementation");
        (RemoteServiceEndpoint::new(provider), state)
    }

    /// Ordered deliveries one subscription worker published, oldest first.
    type UpdateLog = Vec<(JsString, ServiceProviderUpdate<DeltaOp>)>;

    fn publisher(updates: Arc<Mutex<UpdateLog>>) -> ServiceUpdatePublisher {
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
        assert_eq!(
            duplicate.to_string(),
            "Service subscription ID is already active"
        );
        let missing = endpoint
            .invoke(
                create_service_unsubscribe_call("missing"),
                publisher(Arc::clone(&updates)),
                Context::background(),
            )
            .await
            .expect_err("missing subscription");
        assert_eq!(missing.to_string(), "Service subscription was not found");
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
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
        {
            let delivered = lock(&updates);
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].0, JsString::from_utf8("subscription"));
        }
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
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
        assert!(
            lock(&updates).is_empty(),
            "disposed endpoint cannot publish updates"
        );
    }

    fn failing_publisher(calls: Arc<Mutex<usize>>) -> ServiceUpdatePublisher {
        Arc::new(move |_subscription_id, _update, _context| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                *lock(&calls) += 1;
                Err(ServiceError::local("delivery failed"))
            })
        })
    }

    #[tokio::test]
    async fn buffered_updates_publish_only_after_the_worker_starts() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let publish = publisher(Arc::clone(&updates));
        let inner = Arc::new(EndpointInner::new());
        let (sender, receiver) = mpsc::unbounded_channel();
        let listener = update_listener(sender);
        let update = ServiceProviderUpdate::Unavailable;
        listener(&update, &Context::background());
        listener(&update, &Context::background());
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            lock(&updates).is_empty(),
            "updates must not publish before the delivery worker starts"
        );
        spawn_update_worker(
            JsString::from_utf8("subscription"),
            receiver,
            publish,
            inner,
        );
        timeout(Duration::from_secs(1), async {
            while lock(&updates).len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("buffered delivery after the worker starts");
        assert_eq!(lock(&updates).len(), 2);
    }

    #[tokio::test]
    async fn delivers_updates_in_order_when_publishes_complete_out_of_order() {
        let (endpoint, state) = endpoint_with_state();
        let updates = Arc::new(Mutex::new(Vec::new()));
        let (release, released) = oneshot::channel();
        let released = Arc::new(Mutex::new(Some(released)));
        let publish_updates = Arc::clone(&updates);
        let publish: ServiceUpdatePublisher = Arc::new(move |subscription_id, update, _context| {
            let updates = Arc::clone(&publish_updates);
            let released = Arc::clone(&released);
            Box::pin(async move {
                let released = lock(&released).take();
                if let Some(released) = released {
                    let _ = released.await;
                }
                lock(&updates).push((subscription_id, update));
                Ok(())
            })
        });
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("first publish");
        state.with_state_mut(|value| *value = object(2.0));
        state
            .publish(Context::background())
            .expect("second publish");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            lock(&updates).is_empty(),
            "the second update cannot overtake the blocked first delivery"
        );
        release.send(()).expect("release first delivery");
        timeout(Duration::from_secs(1), async {
            while lock(&updates).len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ordered delivery");
        {
            let delivered = lock(&updates);
            let sequences: Vec<f64> = delivered
                .iter()
                .filter_map(|(_, update)| match update {
                    ServiceProviderUpdate::State { sequence, .. } => Some(sequence.as_f64()),
                    _ => None,
                })
                .collect();
            assert_eq!(sequences, [1.0, 2.0]);
        }
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn failed_delivery_terminates_the_subscription_and_reports_the_error() {
        let (endpoint, state) = endpoint_with_state();
        let calls = Arc::new(Mutex::new(0usize));
        let publish = failing_publisher(Arc::clone(&calls));
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish");
        timeout(Duration::from_secs(1), async {
            while *lock(&calls) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery attempted");
        let error = timeout(Duration::from_secs(1), async {
            loop {
                let result = endpoint
                    .invoke(
                        create_service_unsubscribe_call("subscription"),
                        publisher(Arc::new(Mutex::new(Vec::new()))),
                        Context::background(),
                    )
                    .await;
                match result {
                    Err(error) if error.to_string() == "delivery failed" => break error,
                    _ => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("delivery error reported");
        assert_eq!(error.to_string(), "delivery failed");
        state.with_state_mut(|value| *value = object(2.0));
        state
            .publish(Context::background())
            .expect("publish after failure");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *lock(&calls),
            1,
            "terminated subscription receives no further updates"
        );
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn dispose_reports_a_recorded_delivery_error() {
        let (endpoint, state) = endpoint_with_state();
        let calls = Arc::new(Mutex::new(0usize));
        let publish = failing_publisher(Arc::clone(&calls));
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish");
        let subscription_id = JsString::from_utf8("subscription");
        timeout(Duration::from_secs(1), async {
            while !lock(&endpoint.inner.state)
                .delivery_errors
                .contains_key(&subscription_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delivery error recorded");
        let error = endpoint
            .dispose(Context::background())
            .await
            .expect_err("dispose reports the delivery failure");
        assert_eq!(error.to_string(), "delivery failed");
    }

    #[tokio::test]
    async fn stale_delivery_failure_cannot_remove_a_reused_subscription_id() {
        let (endpoint, state) = endpoint_with_state();
        let (started, started_rx) = oneshot::channel::<()>();
        let (release, released) = oneshot::channel::<()>();
        let (failing, failed_rx) = oneshot::channel::<()>();
        let started = Arc::new(Mutex::new(Some(started)));
        let released = Arc::new(Mutex::new(Some(released)));
        let failing = Arc::new(Mutex::new(Some(failing)));
        let stale_publish: ServiceUpdatePublisher = Arc::new(move |_id, _update, _context| {
            let started = Arc::clone(&started);
            let released = Arc::clone(&released);
            let failing = Arc::clone(&failing);
            Box::pin(async move {
                if let Some(started) = lock(&started).take() {
                    let _ = started.send(());
                }
                let released = lock(&released).take();
                if let Some(released) = released {
                    let _ = released.await;
                }
                if let Some(failing) = lock(&failing).take() {
                    let _ = failing.send(());
                }
                Err(ServiceError::local("stale delivery failed"))
            })
        });
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                stale_publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish");
        started_rx.await.expect("stale delivery started");
        endpoint
            .invoke(
                create_service_unsubscribe_call("subscription"),
                publisher(Arc::new(Mutex::new(Vec::new()))),
                Context::background(),
            )
            .await
            .expect("unsubscribe first generation");
        let updates = Arc::new(Mutex::new(Vec::new()));
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publisher(Arc::clone(&updates)),
                Context::background(),
            )
            .await
            .expect("re-subscribe reuses the id");
        release.send(()).expect("release stale delivery");
        failed_rx.await.expect("stale delivery failed");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        state.with_state_mut(|value| *value = object(2.0));
        state
            .publish(Context::background())
            .expect("publish after reuse");
        timeout(Duration::from_secs(1), async {
            while lock(&updates).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reused subscription still delivers");
        endpoint
            .invoke(
                create_service_unsubscribe_call("subscription"),
                publisher(Arc::new(Mutex::new(Vec::new()))),
                Context::background(),
            )
            .await
            .expect("stale failure must not poison the reused id");
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    #[expect(
        clippy::panic,
        reason = "the publisher must panic during invocation to cover the catch_unwind boundary"
    )]
    async fn publish_invocation_panic_is_caught_and_terminates_the_subscription() {
        let (endpoint, state) = endpoint_with_state();
        let calls = Arc::new(Mutex::new(0usize));
        let panic_calls = Arc::clone(&calls);
        let publish: ServiceUpdatePublisher = Arc::new(
            move |_id, _update, _context| -> BoxFuture<'static, Result<(), ServiceError>> {
                *lock(&panic_calls) += 1;
                panic!("publish invocation panicked")
            },
        );
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish");
        let subscription_id = JsString::from_utf8("subscription");
        timeout(Duration::from_secs(1), async {
            while !lock(&endpoint.inner.state)
                .delivery_errors
                .contains_key(&subscription_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panic recorded as a delivery error");
        let error = endpoint
            .invoke(
                create_service_unsubscribe_call("subscription"),
                publisher(Arc::new(Mutex::new(Vec::new()))),
                Context::background(),
            )
            .await
            .expect_err("unsubscribe reports the panic");
        assert_eq!(error.to_string(), "service update delivery panicked");
        state.with_state_mut(|value| *value = object(2.0));
        state
            .publish(Context::background())
            .expect("publish after panic");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *lock(&calls),
            1,
            "panicked subscription receives no further updates"
        );
        endpoint
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn dispose_waits_for_an_in_flight_delivery_and_reports_its_error() {
        let (endpoint, state) = endpoint_with_state();
        let (started, started_rx) = oneshot::channel::<()>();
        let (release, released) = oneshot::channel::<()>();
        let started = Arc::new(Mutex::new(Some(started)));
        let released = Arc::new(Mutex::new(Some(released)));
        let publish: ServiceUpdatePublisher = Arc::new(move |_id, _update, _context| {
            let started = Arc::clone(&started);
            let released = Arc::clone(&released);
            Box::pin(async move {
                if let Some(started) = lock(&started).take() {
                    let _ = started.send(());
                }
                let released = lock(&released).take();
                if let Some(released) = released {
                    let _ = released.await;
                }
                Err(ServiceError::local("in-flight delivery failed"))
            })
        });
        endpoint
            .invoke(
                create_service_subscribe_call("subscription", "svc", ServiceMode::Singleton),
                publish,
                Context::background(),
            )
            .await
            .expect("subscribe");
        state.with_state_mut(|value| *value = object(1.0));
        state.publish(Context::background()).expect("publish");
        started_rx.await.expect("delivery started");
        let dispose_endpoint = Arc::clone(&endpoint);
        let dispose =
            tokio::spawn(async move { dispose_endpoint.dispose(Context::background()).await });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            !dispose.is_finished(),
            "dispose must wait for the in-flight delivery"
        );
        release.send(()).expect("release delivery");
        let error = timeout(Duration::from_secs(1), dispose)
            .await
            .expect("dispose completes")
            .expect("dispose task")
            .expect_err("dispose reports the in-flight delivery error");
        assert_eq!(error.to_string(), "in-flight delivery failed");
    }
}
