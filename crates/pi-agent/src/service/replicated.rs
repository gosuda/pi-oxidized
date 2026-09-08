//! Provider and consumer replicated-state primitives.
//!
//! Mutable state derives publications from the canonical [`DeltaTracker`].
//! Consumer state keeps immutable `Arc` revisions and never calls user code
//! while holding its internal locks.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;

use super::delta::{DeltaOp, DeltaTracker, apply_immutable, is_base};
use super::error::{RemoteServiceErrorCode, ServiceError};
use super::state_codec::ServiceStateError;
use super::value::{JsInteger, JsonValue, is_json_value};
use crate::context::Context;

/// Whether a state delivery is a complete hydration or an incremental update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplicatedStateDeliveryKind {
    /// A complete base snapshot was installed.
    Hydrate,
    /// A contiguous incremental publication was installed.
    Update,
}

/// Metadata accompanying one immutable state revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedStateDelivery {
    /// Delivery kind.
    pub kind: ReplicatedStateDeliveryKind,
    /// Sequence assigned to the delivered revision.
    pub sequence: JsInteger,
}

/// Listener for consumer-side immutable state revisions.
pub type ReplicatedStateListener =
    Arc<dyn Fn(Arc<JsonValue>, Context, ReplicatedStateDelivery) + Send + Sync>;

/// Listener for provider-side decoded delta publications.
pub type ReplicatedSourceListener = Arc<dyn Fn(&[DeltaOp], JsInteger, Context) + Send + Sync>;

struct ReplicaInner {
    value: Option<Arc<JsonValue>>,
    sequence: Option<JsInteger>,
}

/// A cold consumer replica that validates hydration and contiguous updates.
#[derive(Clone)]
pub struct ReplicatedState {
    inner: Arc<Mutex<ReplicaInner>>,
    listeners: Arc<Mutex<BTreeMap<u64, ReplicatedStateListener>>>,
    next_listener: Arc<AtomicU64>,
    report_error: Arc<dyn Fn(ServiceError) + Send + Sync>,
    transition_gate: Arc<Mutex<()>>,
}

impl std::fmt::Debug for ReplicatedState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplicatedState")
            .field("value", &self.value().is_some())
            .field("sequence", &self.sequence())
            .finish_non_exhaustive()
    }
}

impl Default for ReplicatedState {
    fn default() -> Self {
        Self::new(Arc::new(|_error| {}))
    }
}

impl ReplicatedState {
    /// Creates a cold replica.  Listener failures are reported through
    /// `report_error` rather than escaping a provider update callback.
    #[must_use]
    pub fn new(report_error: Arc<dyn Fn(ServiceError) + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ReplicaInner {
                value: None,
                sequence: None,
            })),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener: Arc::new(AtomicU64::new(0)),
            report_error,
            transition_gate: Arc::new(Mutex::new(())),
        }
    }

    /// Returns the current immutable revision, or `None` before hydration.
    #[must_use]
    pub fn value(&self) -> Option<Arc<JsonValue>> {
        lock(&self.inner).value.clone()
    }

    /// Returns the current sequence, or `None` before hydration.
    #[must_use]
    pub fn sequence(&self) -> Option<JsInteger> {
        lock(&self.inner).sequence
    }

    /// Installs a complete base batch and delivers a hydrate revision.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::State`] if `ops` is not a base batch, if it
    /// does not produce a value, or if the produced value is not strict JSON.
    pub fn hydrate(
        &self,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: &Context,
    ) -> Result<(), ServiceError> {
        let transition = lock(&self.transition_gate);
        if !is_base(ops) {
            return Err(ServiceError::State(ServiceStateError::NotBaseBatch));
        }
        let value = apply_immutable(None, ops)?.ok_or_else(|| {
            ServiceError::internal("replicated state base batch did not produce a value")
        })?;
        if !is_json_value(&value) {
            return Err(ServiceError::remote(
                super::error::RemoteServiceErrorCode::ServiceInvalidValue,
                "replicated state hydration must be strict JSON",
            ));
        }
        let value = Arc::new(value);
        {
            let mut inner = lock(&self.inner);
            inner.value = Some(Arc::clone(&value));
            inner.sequence = Some(sequence);
        }
        drop(transition);
        self.deliver_all(
            &value,
            context,
            ReplicatedStateDelivery {
                kind: ReplicatedStateDeliveryKind::Hydrate,
                sequence,
            },
        );
        Ok(())
    }
    /// Installs a contiguous incremental batch and delivers an update revision.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::State`] if there is no prior hydration, if the
    /// `sequence` is not the next expected value, or if the result is not
    /// strict JSON.
    pub fn update(
        &self,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: &Context,
    ) -> Result<(), ServiceError> {
        let transition = lock(&self.transition_gate);
        let (previous_sequence, previous_value) = {
            let inner = lock(&self.inner);
            let (Some(sequence), Some(value)) = (inner.sequence, inner.value.as_ref()) else {
                return Err(ServiceError::State(
                    ServiceStateError::UpdateBeforeHydration,
                ));
            };
            (sequence, Arc::clone(value))
        };
        if previous_sequence.next() != sequence {
            self.clear();
            return Err(ServiceError::State(ServiceStateError::SequenceGap));
        }
        let value = apply_immutable(Some(previous_value.as_ref()), ops)?.ok_or_else(|| {
            ServiceError::internal("replicated state update unexpectedly cleared its value")
        })?;
        if !is_json_value(&value) {
            return Err(ServiceError::remote(
                super::error::RemoteServiceErrorCode::ServiceInvalidValue,
                "replicated state update must be strict JSON",
            ));
        }
        let value = Arc::new(value);
        {
            let mut inner = lock(&self.inner);
            inner.value = Some(Arc::clone(&value));
            inner.sequence = Some(sequence);
        }
        drop(transition);
        self.deliver_all(
            &value,
            context,
            ReplicatedStateDelivery {
                kind: ReplicatedStateDeliveryKind::Update,
                sequence,
            },
        );
        Ok(())
    }
    /// Discards the hydrated revision and sequence without notifying listeners.
    pub fn clear(&self) {
        let mut inner = lock(&self.inner);
        inner.value = None;
        inner.sequence = None;
    }

    /// Subscribes to immutable revisions and reports registration failures.
    ///
    /// # Errors
    ///
    /// This method currently returns `Ok`; the result type is retained for
    /// consistency with transport registration APIs.
    pub fn subscribe(
        &self,
        listener: ReplicatedStateListener,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, ServiceError> {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        let (current, listener_for_call) = {
            let inner = lock(&self.inner);
            let mut listeners = lock(&self.listeners);
            let current = inner
                .value
                .as_ref()
                .zip(inner.sequence)
                .map(|(value, sequence)| (Arc::clone(value), sequence));
            let listener_for_call = listener.clone();
            listeners.insert(id, listener);
            (current, listener_for_call)
        };
        if let Some((value, sequence)) = current {
            self.deliver_one(
                &listener_for_call,
                value,
                Context::background(),
                ReplicatedStateDelivery {
                    kind: ReplicatedStateDeliveryKind::Hydrate,
                    sequence,
                },
            );
        }
        Ok(remove_listener(&self.listeners, id))
    }

    fn deliver_all(
        &self,
        value: &Arc<JsonValue>,
        context: &Context,
        delivery: ReplicatedStateDelivery,
    ) {
        let listeners: Vec<ReplicatedStateListener> =
            lock(&self.listeners).values().cloned().collect();
        for listener in listeners {
            self.deliver_one(&listener, Arc::clone(value), context.clone(), delivery);
        }
    }

    fn deliver_one(
        &self,
        listener: &ReplicatedStateListener,
        value: Arc<JsonValue>,
        context: Context,
        delivery: ReplicatedStateDelivery,
    ) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(value, context, delivery);
        }));
        if result.is_err() {
            (self.report_error)(ServiceError::internal("replicated state listener panicked"));
        }
    }
}

struct PendingPublication {
    ops: Vec<DeltaOp>,
    sequence: JsInteger,
    value: Arc<JsonValue>,
    context: Context,
}

#[derive(Clone, Copy)]
enum PublicationMode {
    Inline,
    Queued,
}

enum PublicationRoute {
    Start(PendingPublication),
    Inline(PendingPublication),
    Queued,
}

struct MutableInner {
    tracker: DeltaTracker,
    published: Arc<JsonValue>,
    sequence: JsInteger,
    pending: VecDeque<PendingPublication>,
    draining: bool,
    draining_thread: Option<thread::ThreadId>,
}

/// A provider-owned mutable state with explicit delta publication.
pub struct MutableReplicatedState {
    inner: Arc<Mutex<MutableInner>>,
    listeners: Arc<Mutex<BTreeMap<u64, ReplicatedStateListener>>>,
    source_listeners: Arc<Mutex<BTreeMap<u64, ReplicatedSourceListener>>>,
    next_listener: Arc<AtomicU64>,
    next_source_listener: Arc<AtomicU64>,
}

impl std::fmt::Debug for MutableReplicatedState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MutableReplicatedState")
            .field("sequence", &self.sequence())
            .field("dirty", &lock(&self.inner).tracker.dirty())
            .finish_non_exhaustive()
    }
}

impl MutableReplicatedState {
    /// Creates a mutable state rooted at `initial`.
    #[must_use]
    pub fn new(initial: JsonValue) -> Arc<Self> {
        let mut tracker = DeltaTracker::new(initial.clone());
        // The source tracker consumes its initial base during construction.  A
        // discarded baseline gives the same publication state without needing
        // to manufacture and immediately apply a duplicate replacement batch.
        tracker.discard();
        Arc::new(Self {
            inner: Arc::new(Mutex::new(MutableInner {
                tracker,
                published: Arc::new(initial),
                sequence: JsInteger::zero(),
                pending: VecDeque::new(),
                draining: false,
                draining_thread: None,
            })),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            source_listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener: Arc::new(AtomicU64::new(0)),
            next_source_listener: Arc::new(AtomicU64::new(0)),
        })
    }
    /// Returns the last published immutable revision.
    #[must_use]
    pub fn value(&self) -> Arc<JsonValue> {
        lock(&self.inner).published.clone()
    }

    /// Returns a detached copy of the currently tracked mutable value.
    #[must_use]
    pub fn state(&self) -> Arc<JsonValue> {
        Arc::new(lock(&self.inner).tracker.state().clone())
    }

    /// Runs a synchronous mutation against a detached tracked-state revision.
    ///
    /// The user callback never runs while the state mutex is held.  Its
    /// resulting value is adopted as the next tracked root afterward.
    pub fn with_state_mut<R>(&self, mutate: impl FnOnce(&mut JsonValue) -> R) -> R {
        let mut next = self.state().as_ref().clone();
        let result = mutate(&mut next);
        let mut inner = lock(&self.inner);
        *inner.tracker.state_mut() = next;
        result
    }

    /// Returns the latest publication sequence, starting at zero.
    #[must_use]
    pub fn sequence(&self) -> JsInteger {
        lock(&self.inner).sequence
    }

    /// Publishes pending mutations as one canonical delta batch.
    ///
    /// # Errors
    ///
    /// Returns a [`ServiceError`] when the tracked value or resulting
    /// publication is invalid, or when delta generation or application fails.
    pub fn publish(&self, context: Context) -> Result<(), ServiceError> {
        let current_thread = thread::current().id();
        let route = {
            let mut inner = lock(&self.inner);
            if !is_json_value(inner.tracker.state()) {
                return Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceInvalidValue,
                    "replicated state publication must be strict JSON",
                ));
            }
            let ops = inner.tracker.flush()?;
            if ops.is_empty() {
                return Ok(());
            }
            let sequence = inner.sequence.next();
            inner.sequence = sequence;
            let value =
                apply_immutable(Some(inner.published.as_ref()), &ops)?.ok_or_else(|| {
                    ServiceError::internal("replicated publication unexpectedly cleared its value")
                })?;
            if !is_json_value(&value) {
                return Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceInvalidValue,
                    "replicated state publication must be strict JSON",
                ));
            }
            let publication = PendingPublication {
                ops,
                sequence,
                value: Arc::new(value),
                context,
            };
            inner.published = Arc::clone(&publication.value);
            if !inner.draining {
                inner.draining = true;
                inner.draining_thread = Some(current_thread);
                PublicationRoute::Start(publication)
            } else if inner.draining_thread == Some(current_thread) {
                PublicationRoute::Inline(publication)
            } else {
                inner.pending.push_back(publication);
                PublicationRoute::Queued
            }
        };
        match route {
            PublicationRoute::Start(publication) => {
                let mut panic_payload =
                    dispatch_publication(self, &publication, PublicationMode::Inline);
                if let Some(payload) = drain_publications(self) {
                    panic_payload.get_or_insert(payload);
                }
                if let Some(payload) = panic_payload {
                    std::panic::resume_unwind(payload);
                }
            }
            PublicationRoute::Inline(publication) => {
                if let Some(payload) =
                    dispatch_publication(self, &publication, PublicationMode::Inline)
                {
                    std::panic::resume_unwind(payload);
                }
            }
            PublicationRoute::Queued => {}
        }
        Ok(())
    }

    /// Subscribes to value revisions.  Pending mutations are published first,
    /// then the listener receives a hydrate delivery of the current value.
    ///
    /// # Errors
    ///
    /// Returns any error from the initial call to [`Self::publish`].
    pub fn subscribe(
        &self,
        listener: ReplicatedStateListener,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, ServiceError> {
        self.publish(Context::background())?;
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        let (value, listener_for_call) = {
            let inner = lock(&self.inner);
            let mut listeners = lock(&self.listeners);
            let value = inner.published.clone();
            let listener_for_call = listener.clone();
            listeners.insert(id, listener);
            (value, listener_for_call)
        };
        listener_for_call(
            value,
            Context::background(),
            ReplicatedStateDelivery {
                kind: ReplicatedStateDeliveryKind::Hydrate,
                sequence: self.sequence(),
            },
        );
        Ok(remove_listener(&self.listeners, id))
    }

    /// Subscribes to source-side decoded operation batches.
    pub fn subscribe_source(
        &self,
        listener: ReplicatedSourceListener,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let id = self.next_source_listener.fetch_add(1, Ordering::Relaxed);
        lock(&self.source_listeners).insert(id, listener);
        remove_listener(&self.source_listeners, id)
    }

    /// Returns whether the currently tracked value satisfies strict JSON
    /// admission.  This is useful before exposing it at a remote seam.
    #[must_use]
    pub fn is_strict_json(&self) -> bool {
        is_json_value(self.state().as_ref())
    }
}

fn drain_publications(state: &MutableReplicatedState) -> Option<Box<dyn std::any::Any + Send>> {
    let mut panic_payload = None;
    loop {
        let Some(publication) = ({
            let mut inner = lock(&state.inner);
            let publication = inner.pending.pop_front();
            if publication.is_none() {
                inner.draining = false;
                inner.draining_thread = None;
            }
            publication
        }) else {
            return panic_payload;
        };
        if let Some(payload) = dispatch_publication(state, &publication, PublicationMode::Queued) {
            panic_payload.get_or_insert(payload);
        }
    }
}

fn dispatch_publication(
    state: &MutableReplicatedState,
    publication: &PendingPublication,
    mode: PublicationMode,
) -> Option<Box<dyn std::any::Any + Send>> {
    let mut panic_payload = None;
    let source_listeners = lock(&state.source_listeners)
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for listener in source_listeners {
        let sequence = match mode {
            PublicationMode::Inline => lock(&state.inner).sequence,
            PublicationMode::Queued => publication.sequence,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(&publication.ops, sequence, publication.context.clone());
        }));
        if let Err(payload) = result {
            panic_payload.get_or_insert(payload);
        }
    }

    let listeners = lock(&state.listeners).values().cloned().collect::<Vec<_>>();
    let sequence = match mode {
        PublicationMode::Inline => lock(&state.inner).sequence,
        PublicationMode::Queued => publication.sequence,
    };
    let delivery = ReplicatedStateDelivery {
        kind: ReplicatedStateDeliveryKind::Update,
        sequence,
    };
    for listener in listeners {
        let value = match mode {
            PublicationMode::Inline => lock(&state.inner).published.clone(),
            PublicationMode::Queued => Arc::clone(&publication.value),
        };
        let context = publication.context.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(value, context, delivery);
        }));
        if let Err(payload) = result {
            panic_payload.get_or_insert(payload);
        }
    }
    panic_payload
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn remove_listener<T: ?Sized + Send + Sync + 'static>(
    listeners: &Arc<Mutex<BTreeMap<u64, Arc<T>>>>,
    id: u64,
) -> Arc<dyn Fn() + Send + Sync> {
    let weak: Weak<Mutex<BTreeMap<u64, Arc<T>>>> = Arc::downgrade(listeners);
    let closed = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(listeners) = weak.upgrade() {
            lock(&listeners).remove(&id);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MutableReplicatedState, ReplicatedStateDelivery, ReplicatedStateDeliveryKind,
        ReplicatedStateListener, lock,
    };
    use crate::context::Context;
    use crate::service::value::{JsObject, JsString, JsonValue};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn object(number: f64) -> JsonValue {
        JsonValue::Object(JsObject::from([(
            JsString::from_utf8("n"),
            JsonValue::Number(number),
        )]))
    }

    #[expect(clippy::panic, reason = "the test fixture must receive an object")]
    fn set_number(value: &mut JsonValue, number: f64) {
        let JsonValue::Object(map) = value else {
            panic!("expected an object");
        };
        map.insert(JsString::from_utf8("n"), JsonValue::Number(number));
    }

    fn number(value: &JsonValue) -> f64 {
        let number = value
            .as_object()
            .and_then(|map| map.get(&JsString::from_utf8("n")))
            .and_then(JsonValue::as_f64);
        assert!(number.is_some(), "expected numeric n");
        number.unwrap_or(f64::NAN)
    }

    #[test]
    fn mutable_replicated_state_preserves_same_thread_reentrant_delivery_order()
    -> Result<(), String> {
        let state = MutableReplicatedState::new(object(0.0));
        let events = Arc::new(Mutex::new(Vec::<(char, f64, f64)>::new()));

        let state_for_a = Arc::clone(&state);
        let events_for_a = Arc::clone(&events);
        let _remove_a = state
            .subscribe(Arc::new(move |value, context, delivery| {
                if delivery.kind != ReplicatedStateDeliveryKind::Update {
                    return;
                }
                let value = number(value.as_ref());
                lock(&events_for_a).push(('A', value, delivery.sequence.as_f64()));
                if value.to_bits() == 1.0_f64.to_bits() {
                    state_for_a.with_state_mut(|state| set_number(state, 2.0));
                    assert!(
                        state_for_a.publish(context).is_ok(),
                        "reentrant publication should succeed"
                    );
                }
            }))
            .map_err(|error| format!("listener A: {error}"))?;

        let events_for_b = Arc::clone(&events);
        let _remove_b = state
            .subscribe(Arc::new(move |value, _context, delivery| {
                if delivery.kind == ReplicatedStateDeliveryKind::Update {
                    lock(&events_for_b).push((
                        'B',
                        number(value.as_ref()),
                        delivery.sequence.as_f64(),
                    ));
                }
            }))
            .map_err(|error| format!("listener B: {error}"))?;

        state.with_state_mut(|state| set_number(state, 1.0));
        state
            .publish(Context::background())
            .map_err(|error| format!("initial publication should succeed: {error}"))?;

        assert_eq!(
            *lock(&events),
            vec![
                ('A', 1.0, 1.0),
                ('A', 2.0, 2.0),
                ('B', 2.0, 2.0),
                ('B', 2.0, 1.0),
            ]
        );
        Ok(())
    }

    enum CrossStateEvent {
        ListenerReady(u8),
        HelperReady(u8),
        HelperDone(u8, bool),
    }

    struct CrossStateListenerArgs {
        other: Arc<MutableReplicatedState>,
        events: mpsc::Sender<CrossStateEvent>,
        starts: Arc<Mutex<Vec<mpsc::Sender<()>>>>,
        helper_handles: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
        updates: Arc<Mutex<Vec<f64>>>,
        release: mpsc::Receiver<()>,
        id: u8,
    }

    #[expect(
        clippy::expect_used,
        reason = "the cross-state listener fixture must receive its release gate"
    )]
    fn cross_state_listener(args: CrossStateListenerArgs) -> ReplicatedStateListener {
        let CrossStateListenerArgs {
            other,
            events,
            starts,
            helper_handles,
            updates,
            release,
            id,
        } = args;
        let first = Arc::new(AtomicBool::new(true));
        let release = Arc::new(Mutex::new(Some(release)));
        Arc::new(
            move |_value: Arc<JsonValue>, _context: Context, delivery: ReplicatedStateDelivery| {
                if delivery.kind != ReplicatedStateDeliveryKind::Update {
                    return;
                }
                lock(&updates).push(delivery.sequence.as_f64());
                if !first.swap(false, Ordering::AcqRel) {
                    return;
                }
                let (start_tx, start_rx) = mpsc::channel();
                lock(&starts).push(start_tx);
                let helper_state = Arc::clone(&other);
                let helper_events = events.clone();
                let helper = thread::spawn(move || {
                    let _ = helper_events.send(CrossStateEvent::HelperReady(id));
                    let _ = start_rx.recv();
                    helper_state.with_state_mut(|state| set_number(state, 2.0));
                    let succeeded = helper_state.publish(Context::background()).is_ok();
                    let _ = helper_events.send(CrossStateEvent::HelperDone(id, succeeded));
                });
                lock(&helper_handles).push(helper);
                let _ = events.send(CrossStateEvent::ListenerReady(id));
                let receiver = lock(&release).take().expect("listener release receiver");
                let _ = receiver.recv();
            },
        )
    }

    fn coordinate_cross_state_helpers(
        events_rx: &mpsc::Receiver<CrossStateEvent>,
        starts: &Arc<Mutex<Vec<mpsc::Sender<()>>>>,
    ) -> (Option<&'static str>, [Option<bool>; 2]) {
        let mut failure = None;
        let mut listeners_ready = [false; 2];
        let mut helpers_ready = [false; 2];
        for _ in 0..4 {
            match events_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(CrossStateEvent::ListenerReady(id)) => listeners_ready[id as usize] = true,
                Ok(CrossStateEvent::HelperReady(id)) => helpers_ready[id as usize] = true,
                Ok(CrossStateEvent::HelperDone(_, _)) => {
                    failure = Some("helper completed before the start rendezvous");
                    break;
                }
                Err(error) => {
                    failure = Some(match error {
                        mpsc::RecvTimeoutError::Timeout => "listener/helper rendezvous timed out",
                        mpsc::RecvTimeoutError::Disconnected => {
                            "listener/helper event channel closed"
                        }
                    });
                    break;
                }
            }
        }
        if failure.is_none() && (listeners_ready != [true, true] || helpers_ready != [true, true]) {
            failure = Some("listener/helper rendezvous was incomplete");
        }

        let start_channels = {
            let mut channels = lock(starts);
            std::mem::take(&mut *channels)
        };
        for channel in &start_channels {
            let _ = channel.send(());
        }

        let mut helper_results = [None; 2];
        while failure.is_none() && helper_results.iter().any(Option::is_none) {
            match events_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(CrossStateEvent::HelperDone(id, succeeded)) => {
                    helper_results[id as usize] = Some(succeeded);
                    if !succeeded {
                        failure = Some("cross-state helper publication returned an error");
                    }
                }
                Ok(CrossStateEvent::ListenerReady(_) | CrossStateEvent::HelperReady(_)) => {
                    failure = Some("duplicate helper/listener rendezvous event");
                }
                Err(error) => {
                    failure = Some(match error {
                        mpsc::RecvTimeoutError::Timeout => "helper publication did not return",
                        mpsc::RecvTimeoutError::Disconnected => "helper event channel closed",
                    });
                }
            }
        }
        (failure, helper_results)
    }

    #[test]
    fn cross_state_helper_thread_publish_does_not_wait() -> Result<(), String> {
        let state_a = MutableReplicatedState::new(object(0.0));
        let state_b = MutableReplicatedState::new(object(0.0));
        let (events_tx, events_rx) = mpsc::channel::<CrossStateEvent>();
        let (release_a_tx, release_a_rx) = mpsc::channel::<()>();
        let (release_b_tx, release_b_rx) = mpsc::channel::<()>();
        let starts = Arc::new(Mutex::new(Vec::<mpsc::Sender<()>>::new()));
        let helper_handles = Arc::new(Mutex::new(Vec::<thread::JoinHandle<()>>::new()));
        let updates_a = Arc::new(Mutex::new(Vec::<f64>::new()));
        let updates_b = Arc::new(Mutex::new(Vec::<f64>::new()));

        let listener_a = cross_state_listener(CrossStateListenerArgs {
            other: Arc::clone(&state_b),
            events: events_tx.clone(),
            starts: Arc::clone(&starts),
            helper_handles: Arc::clone(&helper_handles),
            updates: Arc::clone(&updates_a),
            release: release_a_rx,
            id: 0,
        });
        let listener_b = cross_state_listener(CrossStateListenerArgs {
            other: Arc::clone(&state_a),
            events: events_tx.clone(),
            starts: Arc::clone(&starts),
            helper_handles: Arc::clone(&helper_handles),
            updates: Arc::clone(&updates_b),
            release: release_b_rx,
            id: 1,
        });

        let _remove_a = state_a
            .subscribe(listener_a)
            .map_err(|error| format!("listener A: {error}"))?;
        let _remove_b = state_b
            .subscribe(listener_b)
            .map_err(|error| format!("listener B: {error}"))?;

        let first_state = Arc::clone(&state_a);
        let first_owner = thread::spawn(move || {
            first_state.with_state_mut(|state| set_number(state, 1.0));
            first_state.publish(Context::background())
        });
        let second_state = Arc::clone(&state_b);
        let second_owner = thread::spawn(move || {
            second_state.with_state_mut(|state| set_number(state, 1.0));
            second_state.publish(Context::background())
        });

        let (mut failure, helper_results) = coordinate_cross_state_helpers(&events_rx, &starts);
        let _ = release_a_tx.send(());
        let _ = release_b_tx.send(());

        let first_owner_result = first_owner.join();
        let second_owner_result = second_owner.join();
        if first_owner_result.is_err() || second_owner_result.is_err() {
            failure.get_or_insert("state owner thread panicked");
        }

        let helpers = {
            let mut handles = lock(&helper_handles);
            std::mem::take(&mut *handles)
        };
        for helper in helpers {
            if helper.join().is_err() {
                failure.get_or_insert("cross-state helper thread panicked");
            }
        }

        if let Some(message) = failure {
            return Err(message.to_owned());
        }
        assert_eq!(helper_results, [Some(true), Some(true)]);
        assert_eq!(*lock(&updates_a), vec![1.0, 2.0]);
        assert_eq!(*lock(&updates_b), vec![1.0, 2.0]);
        Ok(())
    }
}
