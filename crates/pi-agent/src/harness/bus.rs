//! Ordered harness event delivery and epoch-gated snapshot watchers.
//!
//! The bus binds recipients synchronously.  Admission therefore has no async
//! gap: an event is either in the owned delivery queue before [`emit`] returns,
//! or it was rejected because the bus had already been closed.  A single bus
//! worker drains that queue in order while each watcher owns an independent
//! serialized worker for its listener.

use futures::future::{BoxFuture, FutureExt, ready};
use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::context::Context;

pub use super::event::{EventFilter, EventListener, MarkBoundary};
use super::event::{HandlerErrorKind, HarnessEvent, HarnessEventPayload, HarnessEventType};
use super::result::{HarnessError, HarnessFault};

/// Captures a complete watcher snapshot from an owned context.
pub type SnapshotCapture<T> =
    Arc<dyn Fn(Context) -> BoxFuture<'static, Result<T, HarnessError>> + Send + Sync>;

/// Captures a replacement snapshot and marks its event-stream boundary.
pub type ResnapshotCapture<T> =
    Arc<dyn Fn(Context, MarkBoundary) -> BoxFuture<'static, Result<T, HarnessError>> + Send + Sync>;

/// Shared slot holding a registration's one-shot removal action.
///
/// [`Unsubscribe`] and [`WatchHandle`] hand out the same removable
/// registration shape: the action runs at most once, on `unsubscribe` or on
/// drop.
type RemovalSlot = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// A removable ordinary event listener registration.
pub struct Unsubscribe(RemovalSlot);

impl Unsubscribe {
    fn new(action: Box<dyn FnOnce() + Send>) -> Self {
        Self(Arc::new(Mutex::new(Some(action))))
    }

    /// Removes the registration.  Calling this more than once is harmless.
    pub fn unsubscribe(&self) {
        let action = lock_unpoisoned(&self.0).take();
        if let Some(action) = action {
            action();
        }
    }
}

impl Drop for Unsubscribe {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Ordered event bus with ordinary listeners and snapshot watchers.
#[derive(Clone)]
pub struct HarnessEventBus {
    core: Arc<BusCore>,
}

struct BusCore {
    state: Mutex<BusState>,
}

struct BusState {
    listeners: BTreeMap<HarnessEventType, Vec<ListenerRegistration>>,
    watchers: Vec<WatcherRegistration>,
    queue: VecDeque<DeliveryItem>,
    running: bool,
    worker_scheduled: bool,
    closed: Option<Arc<HarnessFault>>,
    next_id: u64,
}

struct ListenerRegistration {
    id: u64,
    listener: EventListener,
}

trait WatcherRecipient: Send + Sync {
    fn push(&self, event: HarnessEvent, context: Context);
}

struct WatcherRegistration {
    id: u64,
    recipient: Arc<dyn WatcherRecipient>,
}

enum DeliveryItem {
    Batch {
        events: Vec<BoundEvent>,
        done: Option<oneshot::Sender<()>>,
    },
    Barrier(Option<Box<dyn FnOnce() + Send>>),
}

struct BoundEvent {
    event: HarnessEvent,
    context: Context,
    recipients: Vec<Recipient>,
}

enum Recipient {
    Listener(EventListener),
    Watcher(Arc<dyn WatcherRecipient>),
}

impl HarnessEventBus {
    /// Creates an open bus with no registrations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: Arc::new(BusCore {
                state: Mutex::new(BusState {
                    listeners: BTreeMap::new(),
                    watchers: Vec::new(),
                    queue: VecDeque::new(),
                    running: false,
                    worker_scheduled: false,
                    closed: None,
                    next_id: 0,
                }),
            }),
        }
    }

    /// Registers an ordinary listener for one event type.
    ///
    /// Registration is rejected after [`Self::close`].
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the bus has been closed.
    pub fn on(
        &self,
        event_type: HarnessEventType,
        listener: EventListener,
    ) -> Result<Unsubscribe, HarnessError> {
        let mut state = lock_unpoisoned(&self.core.state);
        if let Some(error) = state.closed.as_ref() {
            return Err(closed_error(error));
        }
        let id = next_id(&mut state);
        state
            .listeners
            .entry(event_type)
            .or_default()
            .push(ListenerRegistration { id, listener });
        let core = Arc::downgrade(&self.core);
        Ok(Unsubscribe::new(Box::new(move || {
            if let Some(core) = core.upgrade() {
                let mut state = lock_unpoisoned(&core.state);
                if let Some(listeners) = state.listeners.get_mut(&event_type) {
                    listeners.retain(|registration| registration.id != id);
                    if listeners.is_empty() {
                        state.listeners.remove(&event_type);
                    }
                }
            }
        })))
    }

    /// Binds one event's recipients and appends it to the serialized queue.
    ///
    /// The returned future observes completion only.  Dropping it does not
    /// retract an event that has already been admitted.
    #[must_use]
    pub fn emit(&self, event: HarnessEvent, cx: &Context) -> BoxFuture<'static, ()> {
        self.emit_batch(vec![event], cx)
    }

    /// Binds every event's recipients and appends one contiguous batch.
    ///
    /// Ordinary listeners are copied in registration order, followed by watch
    /// recipients, for each event.  No listener lookup occurs while delivering
    /// the batch.
    #[must_use]
    pub fn emit_batch(&self, events: Vec<HarnessEvent>, cx: &Context) -> BoxFuture<'static, ()> {
        if events.is_empty() {
            return ready(()).boxed();
        }

        let (done, observation) = oneshot::channel();
        let mut state = lock_unpoisoned(&self.core.state);
        if state.closed.is_some() {
            drop(done);
            return ready(()).boxed();
        }

        let bound =
            events
                .into_iter()
                .map(|event| {
                    let mut recipients = state
                        .listeners
                        .get(&event.event_type())
                        .into_iter()
                        .flat_map(|listeners| {
                            listeners.iter().map(|registration| {
                                Recipient::Listener(Arc::clone(&registration.listener))
                            })
                        })
                        .collect::<Vec<_>>();
                    recipients.extend(state.watchers.iter().map(|registration| {
                        Recipient::Watcher(Arc::clone(&registration.recipient))
                    }));
                    BoundEvent {
                        event,
                        context: cx.clone(),
                        recipients,
                    }
                })
                .collect::<Vec<_>>();
        state.queue.push_back(DeliveryItem::Batch {
            events: bound,
            done: Some(done),
        });
        state.running = true;
        let should_schedule = !state.worker_scheduled;
        if should_schedule {
            state.worker_scheduled = true;
        }
        drop(state);

        if should_schedule && !spawn_drain(Arc::clone(&self.core)) {
            let mut state = lock_unpoisoned(&self.core.state);
            state.worker_scheduled = false;
        }

        let core = Arc::clone(&self.core);
        async move {
            ensure_drain(core.clone()).await;
            let _ = observation.await;
        }
        .boxed()
    }

    /// Installs a watcher with an already captured snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the bus has been closed.
    pub fn watch<T: Clone + Send + Sync + 'static>(
        &self,
        snapshot: T,
        filter: EventFilter,
        resnapshot: Option<ResnapshotCapture<T>>,
    ) -> Result<WatchHandle<T>, HarnessError> {
        let pending = self.install_watcher(filter, resnapshot)?;
        Ok(pending.seal(Arc::new(snapshot)))
    }

    /// Captures a snapshot while the watcher is already registered.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the bus has been closed or when
    /// the capture fails or panics.
    pub async fn watch_from_snapshot<T: Clone + Send + Sync + 'static>(
        &self,
        capture: SnapshotCapture<T>,
        filter: EventFilter,
        cx: &Context,
    ) -> Result<WatchHandle<T>, HarnessError> {
        let resnapshot_capture: ResnapshotCapture<T> = {
            let capture = Arc::clone(&capture);
            Arc::new(move |capture_context, mark_boundary| {
                let capture = Arc::clone(&capture);
                async move {
                    let snapshot = capture(capture_context).await?;
                    mark_boundary.mark()?;
                    Ok(snapshot)
                }
                .boxed()
            })
        };
        let pending = self.install_watcher(filter, Some(resnapshot_capture))?;
        let captured = AssertUnwindSafe(async move { (capture)(cx.clone()).await })
            .catch_unwind()
            .await
            .map_err(|panic| closed_from_panic(panic_message(&panic)))??;
        Ok(pending.seal(Arc::new(captured)))
    }

    /// Seals the bus.  Already bound queue items still drain, then registries
    /// are cleared.  Future admissions and registrations are rejected.
    pub fn close(&self, error: Arc<HarnessFault>) {
        let mut state = lock_unpoisoned(&self.core.state);
        if state.closed.is_none() {
            state.closed = Some(error);
        }
        if !state.running && state.queue.is_empty() {
            state.listeners.clear();
            state.watchers.clear();
        }
    }

    fn install_watcher<T: Clone + Send + Sync + 'static>(
        &self,
        filter: EventFilter,
        resnapshot: Option<ResnapshotCapture<T>>,
    ) -> Result<WatchLease<T>, HarnessError> {
        let mut state = lock_unpoisoned(&self.core.state);
        if let Some(error) = state.closed.as_ref() {
            return Err(closed_error(error));
        }
        let id = next_id(&mut state);
        let weak_core = Arc::downgrade(&self.core);
        let on_error: Arc<
            dyn Fn(HarnessEvent, Context, String) -> BoxFuture<'static, ()> + Send + Sync,
        > = Arc::new(move |event, context, message| {
            if event.event_type() == HarnessEventType::HandlerError {
                return ready(()).boxed();
            }
            let Some(core) = weak_core.upgrade() else {
                return ready(()).boxed();
            };
            let payload = HarnessEventPayload::HandlerError {
                kind: HandlerErrorKind::Event {
                    event: event.event_type().as_str().to_owned(),
                },
                error: message,
                stack: None,
            };
            let Ok(handler_error) = HarnessEvent::new(event.lane.clone(), false, payload) else {
                return ready(()).boxed();
            };
            HarnessEventBus { core }.emit(handler_error, &context)
        });
        let inner = Arc::new(WatcherInner {
            filter,
            state: Mutex::new(WatcherState {
                epoch: 0,
                phase: WatchPhase::Accepting,
                lifecycle: WatcherLifecycle::Buffering,
                listener: None,
                buffer: Vec::new(),
                queue: VecDeque::new(),
                held: Vec::new(),
                resnapshot_active: false,
                worker: WorkerStatus {
                    scheduled: false,
                    callback_in_flight: false,
                },
            }),
            resnapshot,
            on_error,
            core: Arc::downgrade(&self.core),
            self_ref: OnceLock::new(),
        });
        if inner.self_ref.set(Arc::downgrade(&inner)).is_err() {
            return Err(watcher_error(
                "watcher self-reference was initialized twice",
            ));
        }
        let recipient: Arc<dyn WatcherRecipient> = Arc::clone(&inner) as Arc<dyn WatcherRecipient>;
        state.watchers.push(WatcherRegistration { id, recipient });
        drop(state);

        let unregister_core = Arc::downgrade(&self.core);
        let unregister = Box::new(move || {
            if let Some(core) = unregister_core.upgrade() {
                let mut state = lock_unpoisoned(&core.state);
                state.watchers.retain(|registration| registration.id != id);
            }
        });
        Ok(WatchLease {
            inner,
            unregister: Arc::new(Mutex::new(Some(unregister))),
        })
    }

    fn enqueue_barrier(&self, barrier: Box<dyn FnOnce() + Send>) {
        let mut state = lock_unpoisoned(&self.core.state);
        state.queue.push_back(DeliveryItem::Barrier(Some(barrier)));
        state.running = true;
        let should_schedule = !state.worker_scheduled;
        if should_schedule {
            state.worker_scheduled = true;
        }
        drop(state);
        if should_schedule && !spawn_drain(Arc::clone(&self.core)) {
            let mut state = lock_unpoisoned(&self.core.state);
            state.worker_scheduled = false;
        }
    }
}

impl Default for HarnessEventBus {
    fn default() -> Self {
        Self::new()
    }
}

async fn ensure_drain(core: Arc<BusCore>) {
    let should_run = {
        let mut state = lock_unpoisoned(&core.state);
        if state.running && !state.worker_scheduled {
            state.worker_scheduled = true;
            true
        } else {
            false
        }
    };
    if should_run {
        drain(core).await;
    }
}

fn spawn_drain(core: Arc<BusCore>) -> bool {
    let Ok(handle) = Handle::try_current() else {
        return false;
    };
    drop(handle.spawn(drain(core)));
    true
}

async fn drain(core: Arc<BusCore>) {
    loop {
        let item = {
            let mut state = lock_unpoisoned(&core.state);
            let Some(item) = state.queue.pop_front() else {
                // Keep the empty check and worker hand-off under one lock.
                // Otherwise an admission between `pop_front` and clearing
                // `worker_scheduled` could strand a queued observation.
                state.running = false;
                state.worker_scheduled = false;
                if state.closed.is_some() {
                    state.listeners.clear();
                    state.watchers.clear();
                }
                return;
            };
            item
        };
        match item {
            DeliveryItem::Batch { events, done } => {
                for bound in events {
                    deliver_bound(&core, bound).await;
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            DeliveryItem::Barrier(mut barrier) => {
                if let Some(barrier) = barrier.take() {
                    let _ = AssertUnwindSafe(async move { barrier() })
                        .catch_unwind()
                        .await;
                }
            }
        }
    }
}

async fn deliver_bound(core: &Arc<BusCore>, bound: BoundEvent) {
    for recipient in bound.recipients {
        match recipient {
            Recipient::Listener(listener) => {
                let event = bound.event.clone();
                let context = bound.context.clone();
                let result = AssertUnwindSafe(async move { (listener)(event, context).await })
                    .catch_unwind()
                    .await;
                if let Err(panic) = result {
                    report_handler_error(core, &bound.event, &bound.context, panic_message(&panic))
                        .await;
                }
            }
            Recipient::Watcher(watcher) => {
                watcher.push(bound.event.clone(), bound.context.clone());
            }
        }
    }
}

async fn report_handler_error(
    core: &Arc<BusCore>,
    source: &HarnessEvent,
    context: &Context,
    message: String,
) {
    if source.event_type() == HarnessEventType::HandlerError {
        return;
    }
    let payload = HarnessEventPayload::HandlerError {
        kind: HandlerErrorKind::Event {
            event: source.event_type().as_str().to_owned(),
        },
        error: message,
        stack: None,
    };
    let Ok(handler_error) = HarnessEvent::new(source.lane.clone(), false, payload) else {
        return;
    };
    let recipients = {
        let state = lock_unpoisoned(&core.state);
        let mut recipients = state
            .listeners
            .get(&HarnessEventType::HandlerError)
            .into_iter()
            .flat_map(|listeners| {
                listeners
                    .iter()
                    .map(|registration| Recipient::Listener(Arc::clone(&registration.listener)))
            })
            .collect::<Vec<_>>();
        recipients.extend(
            state
                .watchers
                .iter()
                .map(|registration| Recipient::Watcher(Arc::clone(&registration.recipient))),
        );
        recipients
    };

    // Handler-error delivery is intentionally bounded.  A handler that
    // panics here is swallowed rather than recursively producing another
    // handler_error event.
    for recipient in recipients {
        match recipient {
            Recipient::Listener(listener) => {
                let event = handler_error.clone();
                let context = context.clone();
                let _ = AssertUnwindSafe(async move { (listener)(event, context).await })
                    .catch_unwind()
                    .await;
            }
            Recipient::Watcher(watcher) => {
                watcher.push(handler_error.clone(), context.clone());
            }
        }
    }
}

/// The registered-watcher ownership guard for a snapshot watcher.
///
/// The lease is created by `install_watcher`, so the watcher is already bound
/// on the bus and buffers matching events.  While the initial snapshot capture
/// is pending, the lease alone owns the registration: dropping it here (a
/// failed or cancelled capture) runs the teardown.
///
/// [`WatchLease::seal`] moves the lease whole into the handle's `lease` field,
/// which makes the same teardown fire when the live handle is dropped, after
/// `unsubscribe` or at the final drop.  Keeping the pending state a separate
/// type is what makes [`WatchHandle::snapshot`] total: a handle cannot be
/// observed while its snapshot is still missing.
struct WatchLease<T: Clone + Send + Sync + 'static> {
    inner: Arc<WatcherInner<T>>,
    unregister: RemovalSlot,
}

impl<T: Clone + Send + Sync + 'static> WatchLease<T> {
    /// Installs the captured snapshot and returns the live handle, taking over
    /// the registration without running the teardown.
    fn seal(self, snapshot: Arc<T>) -> WatchHandle<T> {
        WatchHandle {
            lease: self,
            snapshot: Mutex::new(snapshot),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Drop for WatchLease<T> {
    fn drop(&mut self) {
        self.inner.teardown(&self.unregister);
    }
}

/// A snapshot watcher with a stable immutable snapshot handle.
pub struct WatchHandle<T: Clone + Send + Sync + 'static> {
    /// Owns the registration; its `Drop` performs the teardown when the handle
    /// is dropped.
    lease: WatchLease<T>,
    snapshot: Mutex<Arc<T>>,
}

impl<T: Clone + Send + Sync + 'static> WatchHandle<T> {
    /// Returns a shared immutable view of the current snapshot.
    ///
    /// A handle is only created with its initial snapshot installed, and
    /// [`Self::resnapshot`] replaces it, so a current snapshot always exists.
    #[must_use]
    pub fn snapshot(&self) -> Arc<T> {
        Arc::clone(&lock_unpoisoned(&self.snapshot))
    }

    /// Starts ordered callback delivery and flushes all pre-start events.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the handle is unsubscribed or has
    /// already started.
    pub fn start(&self, listener: EventListener) -> Result<(), HarnessError> {
        let should_schedule = {
            let mut state = lock_unpoisoned(&self.lease.inner.state);
            if state.unsubscribed() {
                return Err(watcher_error("watch handle is unsubscribed"));
            }
            if state.started() {
                return Err(watcher_error("watch handle has already started"));
            }
            state.lifecycle = WatcherLifecycle::Started;
            state.listener = Some(listener);
            while let Some(event) = state.buffer.pop() {
                state.queue.push_front(event);
            }
            // `buffer` was popped from newest to oldest, so push_front restores
            // the original oldest-to-newest order.
            let should_schedule = !state.queue.is_empty() && !state.worker.scheduled;
            if should_schedule {
                state.worker.scheduled = true;
            }
            should_schedule
        };
        if should_schedule && !self.lease.inner.spawn_worker() {
            let mut state = lock_unpoisoned(&self.lease.inner.state);
            state.worker.scheduled = false;
        }
        Ok(())
    }

    /// Captures a replacement snapshot using the bus-tail boundary protocol.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the handle is unsubscribed, does
    /// not support resnapshot, or already has a resnapshot in progress; when
    /// the capture fails or panics; or when the capture misuses its
    /// exactly-once boundary mark.
    pub async fn resnapshot(&self, cx: &Context) -> Result<Arc<T>, HarnessError> {
        let capture = self.begin_resnapshot()?;
        let (mark_boundary, mut boundary) = self.resnapshot_boundary();
        let capture_result =
            AssertUnwindSafe(async move { (capture)(cx.clone(), mark_boundary).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(closed_from_panic(panic_message(&panic))));
        boundary.wait_if_marked().await;
        self.commit_resnapshot(capture_result, &boundary)
    }

    /// Opens a resnapshot: validates the handle, bumps the epoch so queued
    /// and buffered events are dropped, and returns the capture to run.
    fn begin_resnapshot(&self) -> Result<ResnapshotCapture<T>, HarnessError> {
        let mut state = lock_unpoisoned(&self.lease.inner.state);
        if state.unsubscribed() {
            return Err(watcher_error("watch handle is unsubscribed"));
        }
        let Some(capture) = self.lease.inner.resnapshot.as_ref() else {
            return Err(watcher_error("watch handle does not support resnapshot"));
        };
        if state.resnapshot_active {
            return Err(watcher_error(
                "watch handle resnapshot is already in progress",
            ));
        }
        state.epoch = state.epoch.wrapping_add(1);
        state.phase = WatchPhase::Dropping;
        state.resnapshot_active = true;
        state.buffer.clear();
        state.queue.clear();
        Ok(Arc::clone(capture))
    }

    /// Wires the exactly-once boundary marker for one resnapshot.
    ///
    /// Marking enqueues a bus-tail barrier that transitions this watcher to
    /// `Holding` and resolves the returned receiver once the barrier drains.
    fn resnapshot_boundary(&self) -> (MarkBoundary, ResnapshotMark) {
        let (reached_sender, reached) = oneshot::channel();
        let reached_sender = Arc::new(Mutex::new(Some(reached_sender)));
        let weak_inner = Arc::downgrade(&self.lease.inner);
        let weak_core = self.lease.inner.core.clone();
        let on_mark = Box::new(move || {
            let reached_sender_for_barrier = Arc::clone(&reached_sender);
            let weak_inner = weak_inner.clone();
            let barrier = Box::new(move || {
                if let Some(inner) = weak_inner.upgrade() {
                    inner.mark_holding();
                }
                if let Some(sender) = lock_unpoisoned(&reached_sender_for_barrier).take() {
                    let _ = sender.send(());
                }
            });
            if let Some(core) = weak_core.upgrade() {
                HarnessEventBus { core }.enqueue_barrier(barrier);
            } else if let Some(sender) = lock_unpoisoned(&reached_sender).take() {
                let _ = sender.send(());
            }
        });
        let state = Arc::new(MarkState {
            marked: std::sync::atomic::AtomicBool::new(false),
            duplicate: std::sync::atomic::AtomicBool::new(false),
            callback: Mutex::new(Some(on_mark)),
        });
        let callback_state = Arc::clone(&state);
        let mark_boundary = MarkBoundary::from_callback(Arc::new(move || callback_state.mark()));
        (mark_boundary, ResnapshotMark { state, reached })
    }

    /// Commits a finished capture: installs the snapshot or the failure and
    /// releases events held behind the boundary in their original order.
    fn commit_resnapshot(
        &self,
        capture_result: Result<T, HarnessError>,
        boundary: &ResnapshotMark,
    ) -> Result<Arc<T>, HarnessError> {
        let marked = boundary.marked();
        let boundary_error = boundary.verdict();
        let mut held = Vec::new();
        let mut release_held = false;
        let result = {
            let mut state = lock_unpoisoned(&self.lease.inner.state);
            if state.unsubscribed() {
                state.resnapshot_active = false;
                state.phase = WatchPhase::Accepting;
                state.held.clear();
                Err(watcher_error("watch handle is unsubscribed"))
            } else if let Some(error) = boundary_error {
                if marked {
                    // Keep Holding until `finish_resnapshot` can atomically
                    // place all held events before accepting new ones.
                    release_held = true;
                    held.append(&mut state.held);
                } else {
                    state.resnapshot_active = false;
                    state.phase = WatchPhase::Accepting;
                    state.held.clear();
                }
                Err(error)
            } else {
                release_held = true;
                held.append(&mut state.held);
                match capture_result {
                    Ok(next) => {
                        let next = Arc::new(next);
                        *lock_unpoisoned(&self.snapshot) = Arc::clone(&next);
                        Ok(next)
                    }
                    Err(error) => Err(error),
                }
            }
        };
        if release_held {
            self.lease.inner.finish_resnapshot(held);
        }
        result
    }

    /// Stops future watcher deliveries and unregisters this handle.
    pub fn unsubscribe(&self) {
        self.lease.inner.teardown(&self.lease.unregister);
    }
}

struct WatcherInner<T: Clone + Send + Sync + 'static> {
    filter: EventFilter,
    state: Mutex<WatcherState>,
    resnapshot: Option<ResnapshotCapture<T>>,
    on_error: Arc<dyn Fn(HarnessEvent, Context, String) -> BoxFuture<'static, ()> + Send + Sync>,
    core: Weak<BusCore>,
    self_ref: OnceLock<Weak<WatcherInner<T>>>,
}

struct WatcherState {
    epoch: u64,
    phase: WatchPhase,
    lifecycle: WatcherLifecycle,
    listener: Option<EventListener>,
    buffer: Vec<QueuedEvent>,
    queue: VecDeque<QueuedEvent>,
    held: Vec<QueuedEventNoEpoch>,
    resnapshot_active: bool,
    worker: WorkerStatus,
}

impl WatcherState {
    /// Whether teardown ran; dominates every other state check.
    fn unsubscribed(&self) -> bool {
        matches!(self.lifecycle, WatcherLifecycle::Unsubscribed)
    }

    /// Whether `start` moved delivery from the buffer to the queue.
    fn started(&self) -> bool {
        matches!(self.lifecycle, WatcherLifecycle::Started)
    }
}

/// Registration lifecycle of one watcher.
///
/// `Buffering` and `Started` are mutually exclusive delivery modes and
/// `Unsubscribed` is terminal: teardown dominates every other state check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatcherLifecycle {
    /// `start` has not run; matching events accumulate in `buffer`.
    Buffering,
    /// `start` ran; matching events go to the serialized `queue`.
    Started,
    /// Torn down; buffering, delivery, and resnapshot all stop.
    Unsubscribed,
}

/// Serialized delivery worker status for one watcher.
struct WorkerStatus {
    /// A drain task is scheduled or running.
    scheduled: bool,
    /// The worker popped an event and is invoking the listener callback.
    callback_in_flight: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WatchPhase {
    Accepting,
    Dropping,
    Holding,
}

struct QueuedEvent {
    event: HarnessEvent,
    context: Context,
    epoch: u64,
}

struct QueuedEventNoEpoch {
    event: HarnessEvent,
    context: Context,
}

impl<T: Clone + Send + Sync + 'static> WatcherRecipient for WatcherInner<T> {
    fn push(&self, event: HarnessEvent, context: Context) {
        if lock_unpoisoned(&self.state).unsubscribed() {
            return;
        }
        if !self.filter_matches(&event, &context) {
            return;
        }
        self.push_accepted(QueuedEventNoEpoch { event, context });
    }
}

impl<T: Clone + Send + Sync + 'static> WatcherInner<T> {
    fn filter_matches(&self, event: &HarnessEvent, context: &Context) -> bool {
        match std::panic::catch_unwind(AssertUnwindSafe(|| (self.filter)(event))) {
            Ok(matches) => matches,
            Err(panic) => {
                self.report_error(event.clone(), context.clone(), panic_message(&panic));
                false
            }
        }
    }

    fn push_accepted(&self, event: QueuedEventNoEpoch) {
        let should_schedule = {
            let mut state = lock_unpoisoned(&self.state);
            if state.unsubscribed() {
                return;
            }
            match state.phase {
                WatchPhase::Dropping => return,
                WatchPhase::Holding => {
                    state.held.push(event);
                    return;
                }
                WatchPhase::Accepting => {}
            }
            if state.started() {
                let epoch = state.epoch;
                state.queue.push_back(QueuedEvent {
                    event: event.event,
                    context: event.context,
                    epoch,
                });
                let should_schedule = !state.worker.scheduled;
                if should_schedule {
                    state.worker.scheduled = true;
                }
                should_schedule
            } else {
                let epoch = state.epoch;
                state.buffer.push(QueuedEvent {
                    event: event.event,
                    context: event.context,
                    epoch,
                });
                false
            }
        };
        if should_schedule && !self.spawn_worker() {
            let mut state = lock_unpoisoned(&self.state);
            state.worker.scheduled = false;
        }
    }

    fn mark_holding(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if state.unsubscribed() || !state.resnapshot_active {
            return;
        }
        state.phase = WatchPhase::Holding;
    }

    fn finish_resnapshot(&self, mut held: Vec<QueuedEventNoEpoch>) {
        let should_schedule = {
            let mut state = lock_unpoisoned(&self.state);
            if state.unsubscribed() {
                state.resnapshot_active = false;
                state.phase = WatchPhase::Accepting;
                state.held.clear();
                false
            } else {
                // Events arriving after the capture's mark are still in
                // `state.held`; append them after the events captured before
                // this method acquired the lock.  This keeps the held stream
                // ahead of all events admitted after the swap.
                held.append(&mut state.held);
                let epoch = state.epoch;
                if state.started() {
                    for event in held {
                        state.queue.push_back(QueuedEvent {
                            event: event.event,
                            context: event.context,
                            epoch,
                        });
                    }
                } else {
                    for event in held {
                        state.buffer.push(QueuedEvent {
                            event: event.event,
                            context: event.context,
                            epoch,
                        });
                    }
                }
                state.resnapshot_active = false;
                state.phase = WatchPhase::Accepting;
                let should_schedule =
                    state.started() && !state.queue.is_empty() && !state.worker.scheduled;
                if should_schedule {
                    state.worker.scheduled = true;
                }
                should_schedule
            }
        };
        if should_schedule && !self.spawn_worker() {
            let mut state = lock_unpoisoned(&self.state);
            state.worker.scheduled = false;
        }
    }

    fn spawn_worker(&self) -> bool {
        let Ok(handle) = Handle::try_current() else {
            return false;
        };
        let Some(inner) = self.self_ref.get().and_then(Weak::upgrade) else {
            return false;
        };
        drop(handle.spawn(drain_watcher(inner)));
        true
    }

    fn unsubscribe(&self) {
        let mut state = lock_unpoisoned(&self.state);
        if state.unsubscribed() {
            return;
        }
        state.lifecycle = WatcherLifecycle::Unsubscribed;
        state.listener = None;
        state.buffer.clear();
        state.queue.clear();
        state.held.clear();
    }

    /// Stops delivery and runs the registration's removal action at most once.
    fn teardown(&self, unregister: &RemovalSlot) {
        self.unsubscribe();
        let action = lock_unpoisoned(unregister).take();
        if let Some(action) = action {
            action();
        }
    }

    fn report_error(&self, event: HarnessEvent, context: Context, message: String) {
        let future = (self.on_error)(event, context, message);
        if let Ok(handle) = Handle::try_current() {
            drop(handle.spawn(future));
        }
    }
}

async fn drain_watcher<T: Clone + Send + Sync + 'static>(inner: Arc<WatcherInner<T>>) {
    loop {
        let next = {
            let mut state = lock_unpoisoned(&inner.state);
            if state.unsubscribed() {
                state.worker.scheduled = false;
                return;
            }
            let Some(next) = state.queue.pop_front() else {
                state.worker.scheduled = false;
                return;
            };
            if !state.started() || state.phase != WatchPhase::Accepting || next.epoch != state.epoch
            {
                None
            } else if let Some(listener) = state.listener.as_ref() {
                let listener = Arc::clone(listener);
                state.worker.callback_in_flight = true;
                Some((listener, next.event, next.context))
            } else {
                None
            }
        };
        let Some((listener, event, context)) = next else {
            continue;
        };
        let callback_allowed = {
            let state = lock_unpoisoned(&inner.state);
            !state.unsubscribed() && state.worker.callback_in_flight
        };
        if !callback_allowed {
            lock_unpoisoned(&inner.state).worker.callback_in_flight = false;
            continue;
        }
        let callback_event = event.clone();
        let callback_context = context.clone();
        let callback_result =
            AssertUnwindSafe(async move { (listener)(callback_event, callback_context).await })
                .catch_unwind()
                .await;
        lock_unpoisoned(&inner.state).worker.callback_in_flight = false;
        if let Err(panic) = callback_result
            && event.event_type() != HarnessEventType::HandlerError
        {
            let future = (inner.on_error)(event, context, panic_message(&panic));
            let _ = AssertUnwindSafe(future).catch_unwind().await;
        }
    }
}
struct MarkState {
    marked: std::sync::atomic::AtomicBool,
    duplicate: std::sync::atomic::AtomicBool,
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl MarkState {
    fn mark(&self) -> Result<(), HarnessError> {
        use std::sync::atomic::Ordering;

        if self.marked.swap(true, Ordering::AcqRel) {
            self.duplicate.store(true, Ordering::Release);
            return Err(watcher_error(
                "resnapshot boundary was marked more than once",
            ));
        }
        let callback = lock_unpoisoned(&self.callback).take();
        if let Some(callback) = callback {
            callback();
            Ok(())
        } else {
            self.duplicate.store(true, Ordering::Release);
            Err(watcher_error(
                "resnapshot boundary callback was unavailable",
            ))
        }
    }
}

/// One resnapshot's boundary outcome: the exactly-once mark flags and the
/// receiver that resolves once the bus drains to the marked barrier.
struct ResnapshotMark {
    state: Arc<MarkState>,
    reached: oneshot::Receiver<()>,
}

impl ResnapshotMark {
    /// Whether the capture marked its boundary.
    fn marked(&self) -> bool {
        self.state.marked.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Waits for the boundary barrier to drain when the capture marked it.
    async fn wait_if_marked(&mut self) {
        if self.marked() {
            let _ = (&mut self.reached).await;
        }
    }

    /// Returns the boundary violation when the capture misused its mark.
    fn verdict(&self) -> Option<HarnessError> {
        if !self.marked() {
            Some(watcher_error(
                "resnapshot capture did not mark its boundary",
            ))
        } else if self
            .state
            .duplicate
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Some(watcher_error(
                "resnapshot boundary was marked more than once",
            ))
        } else {
            None
        }
    }
}

fn next_id(state: &mut BusState) -> u64 {
    let id = state.next_id;
    state.next_id = state.next_id.wrapping_add(1);
    id
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn watcher_error(message: &str) -> HarnessError {
    HarnessError::Closed {
        message: message.to_owned(),
    }
}

fn closed_error(fault: &HarnessFault) -> HarnessError {
    HarnessError::Closed {
        message: fault.message.clone(),
    }
}

fn closed_from_panic(message: String) -> HarnessError {
    HarnessError::Closed { message }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "handler panicked".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{Arc, EventFilter, HarnessError, HarnessEventBus};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn dropping_watch_handle_releases_the_registration() -> Result<(), HarnessError> {
        let bus = HarnessEventBus::new();
        let marker = Arc::new(AtomicBool::new(true));
        let weak = Arc::downgrade(&marker);
        let filter: EventFilter = {
            let marker = Arc::clone(&marker);
            Arc::new(move |_| marker.load(Ordering::Relaxed))
        };
        let handle = bus.watch((), filter, None)?;
        drop(handle);
        drop(marker);
        assert!(weak.upgrade().is_none());
        Ok(())
    }
}
