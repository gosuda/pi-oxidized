//! Ordered harness event delivery and epoch-gated snapshot watchers.
//!
//! The bus binds recipients synchronously.  Admission therefore has no async
//! gap: an event is either in the owned delivery queue before [`emit`] returns,
//! worker drains that queue in order while each watcher owns an independent
//! serialized worker for its listener.  A listener that emits and awaits a
//! nested event is dispatched inline on the worker's poll chain instead of
//! queueing behind itself, so nested events overtake earlier batches that are
//! admitted but not yet in delivery.  Outside any Tokio runtime, admission
//! drives the drain synchronously so a dropped future cannot strand an
//! admitted event.  Watchers follow the same rule: without a runtime — or
//! inside a transient admission runtime whose spawned tasks die with it — the
//! watcher worker is driven inline, and a boundary barrier enqueued from the
//! drain's poll chain is delivered by the awaiter's own chain rather than
//! queued behind the suspended worker.

use futures::future::{BoxFuture, FutureExt, ready};
use std::any::Any;
use std::cell::{Cell, RefCell};
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
    /// Identity of the task currently running [`drain`], used to detect a
    /// listener reentering the bus with an awaited emit.  `None` while no
    /// drain is in flight.
    drain_task: Mutex<Option<tokio::task::Id>>,
}

struct BusState {
    listeners: BTreeMap<HarnessEventType, Vec<ListenerRegistration>>,
    watchers: Vec<WatcherRegistration>,
    queue: VecDeque<DeliveryItem>,
    running: bool,
    worker_scheduled: bool,
    closed: Option<Arc<HarnessFault>>,
    next_id: u64,
    /// Count of reentrant (nested) batches occupying the front of `queue`.
    ///
    /// Nested admissions are inserted at this index so they stay ahead of
    /// not-yet-delivered outer batches while preserving FIFO among themselves;
    /// `push_front` would reverse the order of multiple nested admissions.
    reentrant_front: usize,
}

struct ListenerRegistration {
    id: u64,
    listener: EventListener,
}

trait WatcherRecipient: Send + Sync {
    fn push(&self, event: HarnessEvent, context: Context);

    /// Drives this watcher's serialized worker inline until its queue
    /// empties.  No-op when an ancestor worker for the same watcher is
    /// already running on this thread; that worker picks the event up when
    /// its current callback returns.
    fn drive_inline(&self) -> BoxFuture<'static, ()>;
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
    Barrier {
        action: Option<Box<dyn FnOnce() + Send>>,
        done: Option<oneshot::Sender<()>>,
    },
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
                    reentrant_front: 0,
                }),
                drain_task: Mutex::new(None),
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
    /// the batch.  Outside a Tokio runtime the batch is delivered
    /// synchronously before this returns.
    ///
    /// # Panics
    ///
    /// Panics when no Tokio runtime is available and a one-shot current-thread
    /// runtime cannot be built to drive the drain (resource exhaustion while
    /// creating the executor).  Silently stranding the admitted batch would
    /// violate the delivery guarantee, so this fails loudly instead.
    #[must_use]
    #[allow(
        clippy::panic,
        reason = "fail loudly on executor collapse rather than strand admitted events"
    )]
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
        // A listener may reenter the bus while the drain worker is suspended
        // inside it.  Queue such nested batches ahead of not-yet-delivered
        // outer batches; the returned future delivers them inline (see
        // `deliver_reentrant_batch`) instead of waiting for a worker that
        // cannot run until the emitting listener returns.  Insert at the
        // `reentrant_front` boundary so multiple nested admissions from one
        // listener keep FIFO order — `push_front` would reverse them.
        if reentrant_from_drain(&self.core) {
            let boundary = state.reentrant_front;
            state.queue.insert(
                boundary,
                DeliveryItem::Batch {
                    events: bound,
                    done: Some(done),
                },
            );
            state.reentrant_front = boundary + 1;
            drop(state);
            return deliver_reentrant_batch(Arc::clone(&self.core), observation).boxed();
        }
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
            // No Tokio runtime is available, so a dropped future would strand
            // the admitted batch forever.  Drive the drain to completion on
            // this thread: admission keeps guaranteeing delivery.
            INLINE_DRAIN.with(|active| active.set(true));
            let _inline_guard = InlineDrainGuard;
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(drain(Arc::clone(&self.core))),
                // Without an executor there is no way to honor the delivery
                // guarantee; fail loudly rather than strand the event.
                Err(error) => panic!("transient bus drain runtime failed to build: {error}"),
            }
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

    /// Enqueues a stream-boundary barrier at the bus tail and returns how its
    /// completion is observed.
    ///
    /// The barrier's queue position is the resnapshot boundary: everything
    /// already queued is processed through the watcher phase machine before
    /// it fires, and later admissions land behind it.  When the caller is a
    /// delivered listener suspended on the drain's poll chain
    /// (`reentrant_from_drain`), that suspended worker owns the queue — no
    /// second worker may be scheduled — so the caller observes completion by
    /// driving the queue inline from its own chain (`Some`): the awaiting
    /// resnapshot delivers pre-mark items first, preserving the tail
    /// boundary.  `None` means the barrier resolves through the caller's
    /// ordinary boundary receiver once a spawned worker drains to it.
    fn enqueue_barrier(&self, barrier: Box<dyn FnOnce() + Send>) -> Option<oneshot::Receiver<()>> {
        let (done, observation) = oneshot::channel();
        let mut state = lock_unpoisoned(&self.core.state);
        if reentrant_from_drain(&self.core) {
            state.queue.push_back(DeliveryItem::Barrier {
                action: Some(barrier),
                done: Some(done),
            });
            drop(state);
            return Some(observation);
        }
        state.queue.push_back(DeliveryItem::Barrier {
            action: Some(barrier),
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
        None
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
    // Record the draining task so reentrant emits from delivered listeners
    // are recognized; the no-runtime inline drain is recognized through
    // `INLINE_DRAIN` because `block_on` has no task id.
    *lock_unpoisoned(&core.drain_task) = tokio::task::try_id();
    loop {
        let item = {
            let mut state = lock_unpoisoned(&core.state);
            let Some(item) = state.queue.pop_front() else {
                // Keep the empty check and worker hand-off under one lock.
                // Otherwise an admission between `pop_front` and clearing
                // `worker_scheduled` could strand a queued observation.
                state.running = false;
                state.worker_scheduled = false;
                *lock_unpoisoned(&core.drain_task) = None;
                if state.closed.is_some() {
                    state.listeners.clear();
                    state.watchers.clear();
                }
                return;
            };
            if state.reentrant_front > 0 {
                state.reentrant_front -= 1;
            }
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
            DeliveryItem::Barrier { mut action, done } => {
                if let Some(barrier) = action.take() {
                    let _ = AssertUnwindSafe(async move { barrier() })
                        .catch_unwind()
                        .await;
                }
                if let Some(done) = done {
                    let _ = done.send(());
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
                deliver_to_watcher(watcher, bound.event.clone(), bound.context.clone()).await;
            }
        }
    }
}

/// Hands one event to a watcher and, inside a transient inline delivery
/// window, drives the watcher's serialized worker on this poll chain.
///
/// A worker spawned inside `emit_batch`'s one-shot runtime would be dropped
/// when that runtime shuts down at the end of the admission, stranding the
/// watcher queue; awaiting the drain inline is what preserves the
/// no-runtime delivery guarantee.
async fn deliver_to_watcher(
    watcher: Arc<dyn WatcherRecipient>,
    event: HarnessEvent,
    context: Context,
) {
    watcher.push(event, context);
    if watcher_inline_window() {
        watcher.drive_inline().await;
    }
}

thread_local! {
    /// Whether this thread is inside a transient inline delivery window:
    /// driving a no-runtime inline [`drain`] via `emit_batch`, or an inline
    /// watcher worker.  Inside such a window the thread only ever executes
    /// the owner's poll chain, so the flag identifies reentrant admissions
    /// exactly and marks every runtime alive on the thread as transient.
    static INLINE_DRAIN: Cell<bool> = const { Cell::new(false) };
}

/// Restores [`INLINE_DRAIN`] on scope exit, including through unwinding.
struct InlineDrainGuard;

impl Drop for InlineDrainGuard {
    fn drop(&mut self) {
        INLINE_DRAIN.with(|active| active.set(false));
    }
}

thread_local! {
    /// Watcher addresses whose inline workers are currently driving on this
    /// thread.  A watcher on this stack is drained by an ancestor worker on
    /// the same poll chain, so a push reaching it from inside a callback
    /// must not schedule a second concurrent worker.
    static INLINE_WATCHER_WORKERS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Whether the current thread is inside a transient inline delivery window
/// ([`INLINE_DRAIN`]), where delivery is guaranteed only by the caller's own
/// poll chain and spawned tasks die with the transient runtime.
fn watcher_inline_window() -> bool {
    INLINE_DRAIN.with(Cell::get)
}

/// Whether a watcher's inline worker is already active on this thread.
fn watcher_worker_inline(key: usize) -> bool {
    INLINE_WATCHER_WORKERS.with(|workers| workers.borrow().contains(&key))
}

/// Registers and unregisters one inline watcher worker on this thread,
/// including through unwinding.
struct WatcherInlineGuard(usize);

impl WatcherInlineGuard {
    fn enter(key: usize) -> Self {
        INLINE_WATCHER_WORKERS.with(|workers| workers.borrow_mut().push(key));
        Self(key)
    }
}

impl Drop for WatcherInlineGuard {
    fn drop(&mut self) {
        INLINE_WATCHER_WORKERS.with(|workers| {
            let mut workers = workers.borrow_mut();
            if let Some(position) = workers.iter().rposition(|&worker| worker == self.0) {
                workers.remove(position);
            }
        });
    }
}

/// Returns whether the caller is a delivered listener running inside the
/// drain's poll chain.  The spawned worker is recognized by its Tokio task
/// id; the no-runtime inline drain by the [`INLINE_DRAIN`] thread flag.
fn reentrant_from_drain(core: &BusCore) -> bool {
    if INLINE_DRAIN.with(Cell::get) {
        return true;
    }
    let Some(task_id) = tokio::task::try_id() else {
        return false;
    };
    lock_unpoisoned(&core.drain_task).is_some_and(|drain_id| drain_id == task_id)
}

/// Delivers queued items inline until the calling emit's own batch completes.
///
/// Runs on the drain's poll chain: a listener emitted an event and awaits its
/// delivery while the worker is suspended inside that listener.  Items are
/// consumed from the front so the nested batch keeps queue order relative to
/// anything an earlier abandoned nested emit left queued.  Nested admissions
/// are inserted at the `reentrant_front` boundary, so multiple nested batches
/// from one listener are delivered in admission (FIFO) order.  Dropping the
/// future stays safe: the batch remains admitted and the suspended worker
/// delivers it once the emitting listener returns.
async fn deliver_reentrant_batch(core: Arc<BusCore>, observation: oneshot::Receiver<()>) {
    let mut observation = observation;
    loop {
        match observation.try_recv() {
            // Delivered by an enclosing inline dispatch, or the sender went
            // away; completion is observed either way.
            Err(oneshot::error::TryRecvError::Empty) => {}
            _ => return,
        }
        let item = {
            let mut state = lock_unpoisoned(&core.state);
            let item = state.queue.pop_front();
            if state.reentrant_front > 0 {
                state.reentrant_front -= 1;
            }
            item
        };
        let Some(item) = item else {
            return;
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
            DeliveryItem::Barrier { mut action, done } => {
                if let Some(barrier) = action.take() {
                    let _ = AssertUnwindSafe(async move { barrier() })
                        .catch_unwind()
                        .await;
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
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
                deliver_to_watcher(watcher, handler_error.clone(), context.clone()).await;
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
        if should_schedule {
            self.lease.inner.dispatch_worker();
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
        // Held across both awaits: cancellation drops the guard, whose
        // `Drop` restores acceptance exactly like a failed commit would.
        let cancel_guard = ResnapshotCancelGuard {
            inner: Arc::clone(&self.lease.inner),
            mark: Arc::clone(&boundary.state),
            armed: Cell::new(true),
        };
        let capture_result =
            AssertUnwindSafe(async move { (capture)(cx.clone(), mark_boundary).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(closed_from_panic(panic_message(&panic))));
        boundary.wait_if_marked().await;
        let result = self.commit_resnapshot(capture_result, &boundary);
        cancel_guard.disarm();
        result
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
        let reentrant = Arc::new(Mutex::new(None));
        let reentrant_for_mark = Arc::clone(&reentrant);
        let weak_inner = Arc::downgrade(&self.lease.inner);
        let weak_core = self.lease.inner.core.clone();
        let on_mark = Box::new(move || {
            let reached_sender_for_barrier = Arc::clone(&reached_sender);
            let weak_inner = weak_inner.clone();
            let reentrant = Arc::clone(&reentrant_for_mark);
            let barrier = Box::new(move || {
                if let Some(inner) = weak_inner.upgrade() {
                    inner.mark_holding();
                }
                if let Some(sender) = lock_unpoisoned(&reached_sender_for_barrier).take() {
                    let _ = sender.send(());
                }
            });
            if let Some(core) = weak_core.upgrade() {
                if let Some(observation) = (HarnessEventBus { core }).enqueue_barrier(barrier) {
                    *lock_unpoisoned(&reentrant) = Some(observation);
                }
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
        (
            mark_boundary,
            ResnapshotMark {
                state,
                reached,
                reentrant,
                core: self.lease.inner.core.clone(),
            },
        )
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

/// Restores watcher acceptance when a resnapshot is cancelled before commit.
///
/// Held across the capture await and the boundary wait inside
/// [`WatchHandle::resnapshot`].  Dropping the future at either await runs
/// this `Drop`, which undoes `begin_resnapshot` exactly like the failure
/// arms of `commit_resnapshot`: acceptance is restored, and a marked
/// boundary keeps the phase at [`WatchPhase::Holding`] until
/// `finish_resnapshot` can atomically place the held events ahead of
/// anything newly admitted.
struct ResnapshotCancelGuard<T: Clone + Send + Sync + 'static> {
    inner: Arc<WatcherInner<T>>,
    mark: Arc<MarkState>,
    armed: Cell<bool>,
}

impl<T: Clone + Send + Sync + 'static> ResnapshotCancelGuard<T> {
    fn disarm(self) {
        self.armed.set(false);
    }
}

impl<T: Clone + Send + Sync + 'static> Drop for ResnapshotCancelGuard<T> {
    fn drop(&mut self) {
        if !self.armed.get() {
            return;
        }
        let marked = self.mark.marked();
        let mut held = Vec::new();
        let release_held = {
            let mut state = lock_unpoisoned(&self.inner.state);
            if state.unsubscribed() {
                state.resnapshot_active = false;
                state.phase = WatchPhase::Accepting;
                state.held.clear();
                false
            } else if marked {
                held.append(&mut state.held);
                true
            } else {
                state.resnapshot_active = false;
                state.phase = WatchPhase::Accepting;
                state.held.clear();
                false
            }
        };
        if release_held {
            self.inner.finish_resnapshot(held);
        }
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

    fn drive_inline(&self) -> BoxFuture<'static, ()> {
        WatcherInner::drive_inline(self)
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
        if should_schedule {
            self.dispatch_worker();
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
        if should_schedule {
            self.dispatch_worker();
        }
    }

    /// Spawns the serialized worker on the ambient runtime.  A `false`
    /// return must be routed through [`Self::dispatch_worker`]'s inline
    /// fallbacks rather than silently dropping the schedule.
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

    /// Address key matching this watcher's inline workers on one thread.
    fn inline_key(&self) -> usize {
        std::ptr::from_ref::<WatcherInner<T>>(self) as usize
    }

    fn drive_inline(&self) -> BoxFuture<'static, ()> {
        let key = self.inline_key();
        if watcher_worker_inline(key) {
            return ready(()).boxed();
        }
        let Some(inner) = self.self_ref.get().and_then(Weak::upgrade) else {
            return ready(()).boxed();
        };
        async move {
            let _worker_guard = WatcherInlineGuard::enter(key);
            drain_watcher(inner).await;
        }
        .boxed()
    }

    /// Starts this watcher's serialized worker for newly queued events.
    ///
    /// Inside a transient inline delivery window the awaiting deliverer owns
    /// the queue via [`Self::drive_inline`]: a task spawned here would be
    /// dropped when the transient runtime shuts down at the end of the
    /// admission.  Without any runtime the worker is driven inline on a
    /// one-shot current-thread runtime, mirroring `emit_batch`'s delivery
    /// guarantee.
    fn dispatch_worker(&self) {
        if watcher_inline_window() {
            return;
        }
        if self.spawn_worker() {
            return;
        }
        self.run_worker_inline();
    }

    /// Drives this watcher's worker inline when no runtime exists to host a
    /// spawned task, so queued events cannot strand.
    #[allow(
        clippy::panic,
        reason = "fail loudly on executor collapse rather than strand queued events"
    )]
    fn run_worker_inline(&self) {
        INLINE_DRAIN.with(|active| active.set(true));
        let _inline_guard = InlineDrainGuard;
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(self.drive_inline()),
            Err(error) => panic!("transient watcher worker runtime failed to build: {error}"),
        }
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
    /// Whether the capture marked its boundary.
    fn marked(&self) -> bool {
        self.marked.load(std::sync::atomic::Ordering::Acquire)
    }

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

/// One resnapshot's boundary outcome: the exactly-once mark flags, the
/// receiver that resolves once the bus drains to the marked barrier, and —
/// when the mark ran on the drain's poll chain — the inline observation that
/// lets the caller deliver the barrier on its own chain.
struct ResnapshotMark {
    state: Arc<MarkState>,
    reached: oneshot::Receiver<()>,
    reentrant: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    core: Weak<BusCore>,
}

impl ResnapshotMark {
    /// Whether the capture marked its boundary.
    fn marked(&self) -> bool {
        self.state.marked()
    }

    /// Waits for the boundary barrier to drain when the capture marked it.
    ///
    /// A barrier enqueued from the drain's poll chain carries its own inline
    /// observation; driving it here delivers the barrier on this chain
    /// instead of waiting for a worker that is suspended inside the caller.
    async fn wait_if_marked(&mut self) {
        if !self.marked() {
            return;
        }
        let observation = lock_unpoisoned(&self.reentrant).take();
        if let Some(core) = self.core.upgrade()
            && let Some(observation) = observation
        {
            deliver_reentrant_batch(core, observation).await;
            return;
        }
        let _ = (&mut self.reached).await;
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
#[allow(
    clippy::expect_used,
    reason = "bus tests use contextual fixture failures"
)]
mod tests {
    use super::{Arc, EventFilter, HarnessError, HarnessEventBus, ResnapshotCapture};
    use crate::context::Context;
    use crate::harness::event::{
        EventListener, HandlerErrorKind, HarnessEvent, HarnessEventPayload, HarnessEventType,
        MarkBoundary,
    };
    use futures::future::{FutureExt, pending};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    fn fault_event(code: &str) -> HarnessEvent {
        HarnessEvent::global(HarnessEventPayload::Fault {
            code: code.to_owned(),
            message: "test fault".to_owned(),
        })
    }

    fn handler_error_event() -> HarnessEvent {
        HarnessEvent::global(HarnessEventPayload::HandlerError {
            kind: HandlerErrorKind::Event {
                event: "fault".to_owned(),
            },
            error: "nested emit".to_owned(),
            stack: None,
        })
    }

    fn flag_listener(flag: &Arc<AtomicBool>) -> EventListener {
        let flag = Arc::clone(flag);
        Arc::new(move |_event: HarnessEvent, _context: Context| {
            let flag = Arc::clone(&flag);
            async move {
                flag.store(true, Ordering::Release);
            }
            .boxed()
        })
    }

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

    #[tokio::test]
    async fn listener_emitting_and_awaiting_does_not_deadlock() {
        let bus = HarnessEventBus::new();
        let context = Context::background();
        let nested_delivered = Arc::new(AtomicBool::new(false));
        let outer_completed = Arc::new(AtomicBool::new(false));

        let nested_flag = Arc::clone(&nested_delivered);
        let _nested_guard = bus
            .on(HarnessEventType::HandlerError, flag_listener(&nested_flag))
            .expect("nested registration");

        let outer_bus = bus.clone();
        let outer_flag = Arc::clone(&outer_completed);
        let _outer_guard = bus
            .on(
                HarnessEventType::Fault,
                Arc::new(move |_event: HarnessEvent, context: Context| {
                    let bus = outer_bus.clone();
                    let outer_flag = Arc::clone(&outer_flag);
                    let nested = handler_error_event();
                    async move {
                        bus.emit(nested, &context).await;
                        outer_flag.store(true, Ordering::Release);
                    }
                    .boxed()
                }),
            )
            .expect("outer registration");

        tokio::time::timeout(
            Duration::from_secs(10),
            bus.emit(fault_event("outer"), &context),
        )
        .await
        .expect("reentrant emit must not deadlock");

        assert!(nested_delivered.load(Ordering::Acquire));
        assert!(outer_completed.load(Ordering::Acquire));
    }

    #[test]
    fn emit_outside_runtime_delivers_without_awaiting() {
        let bus = HarnessEventBus::new();
        let delivered = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&delivered);
        let _listener_guard = bus
            .on(HarnessEventType::Fault, flag_listener(&flag))
            .expect("registration");

        drop(bus.emit(fault_event("no-runtime"), &Context::background()));
        assert!(
            delivered.load(Ordering::Acquire),
            "dropping the future must not strand the admitted event"
        );
    }

    fn counting_listener(counter: &Arc<AtomicUsize>) -> EventListener {
        let counter = Arc::clone(counter);
        Arc::new(move |_event: HarnessEvent, _context: Context| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::AcqRel);
            }
            .boxed()
        })
    }

    #[test]
    fn emit_outside_runtime_delivers_to_started_watcher() {
        let bus = HarnessEventBus::new();
        let delivered = Arc::new(AtomicUsize::new(0));
        let filter: EventFilter = Arc::new(|_| true);
        let handle = bus.watch((), filter, None).expect("watch registration");
        handle
            .start(counting_listener(&delivered))
            .expect("watch start");

        drop(bus.emit(fault_event("watched"), &Context::background()));
        assert_eq!(
            delivered.load(Ordering::Acquire),
            1,
            "the transient admission runtime must not strand the watcher queue"
        );
    }

    #[test]
    fn watcher_start_outside_runtime_drains_buffered_events() {
        let bus = HarnessEventBus::new();
        let delivered = Arc::new(AtomicUsize::new(0));
        let filter: EventFilter = Arc::new(|_| true);
        let handle = bus.watch((), filter, None).expect("watch registration");
        // Buffer one event while the watcher has no listener yet.
        drop(bus.emit(fault_event("buffered"), &Context::background()));

        handle
            .start(counting_listener(&delivered))
            .expect("watch start");
        assert_eq!(
            delivered.load(Ordering::Acquire),
            1,
            "starting outside a runtime must drain the flushed buffer inline"
        );
    }

    #[tokio::test]
    async fn cancelling_resnapshot_restores_acceptance() {
        let bus = HarnessEventBus::new();
        let context = Context::background();
        let filter: EventFilter = Arc::new(|_| true);
        let attempt = Arc::new(AtomicUsize::new(0));
        let capture: ResnapshotCapture<u32> = {
            let attempt = Arc::clone(&attempt);
            Arc::new(move |_context: Context, boundary: MarkBoundary| {
                let attempt = Arc::clone(&attempt);
                async move {
                    if attempt.fetch_add(1, Ordering::AcqRel) == 0 {
                        pending::<Result<u32, HarnessError>>().await
                    } else {
                        boundary.mark()?;
                        Ok(2)
                    }
                }
                .boxed()
            })
        };
        let handle = bus
            .watch(1, filter, Some(capture))
            .expect("watch registration");

        let cancelled =
            tokio::time::timeout(Duration::from_millis(50), handle.resnapshot(&context)).await;
        assert!(cancelled.is_err(), "the first capture must be cancellable");

        let snapshot = tokio::time::timeout(Duration::from_secs(10), handle.resnapshot(&context))
            .await
            .expect("a cancelled resnapshot must not block the next one")
            .expect("second resnapshot succeeds");
        assert_eq!(*snapshot, 2);
        assert_eq!(attempt.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn cancelling_after_boundary_mark_restores_acceptance() {
        let bus = HarnessEventBus::new();
        let context = Context::background();
        let filter: EventFilter = Arc::new(|_| true);
        let marked = Arc::new(AtomicBool::new(false));
        let capture: ResnapshotCapture<u32> = {
            let marked = Arc::clone(&marked);
            Arc::new(move |_context: Context, boundary: MarkBoundary| {
                let marked = Arc::clone(&marked);
                async move {
                    boundary.mark()?;
                    marked.store(true, Ordering::Release);
                    pending::<Result<u32, HarnessError>>().await
                }
                .boxed()
            })
        };
        let handle = bus
            .watch(1, filter, Some(capture))
            .expect("watch registration");
        let delivered = Arc::new(AtomicUsize::new(0));
        handle
            .start(counting_listener(&delivered))
            .expect("watch start");

        let cancelled =
            tokio::time::timeout(Duration::from_millis(50), handle.resnapshot(&context)).await;
        assert!(
            cancelled.is_err(),
            "capture must stay cancellable after marking"
        );
        assert!(
            marked.load(Ordering::Acquire),
            "the boundary must have been marked before cancellation"
        );

        // Let the marked barrier drain, then prove the watcher accepts and
        // delivers again instead of staying in Dropping/Holding.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            bus.emit(fault_event("after-cancel"), &context),
        )
        .await
        .expect("post-cancel emit must not be held behind the cancelled resnapshot");
        assert_eq!(
            delivered.load(Ordering::Acquire),
            1,
            "a cancelled resnapshot must restore Accepting"
        );

        // A second resnapshot must start rather than fail with
        // "already in progress": its capture pends, so only a timeout means
        // the guard cleared `resnapshot_active`.
        let second =
            tokio::time::timeout(Duration::from_millis(50), handle.resnapshot(&context)).await;
        assert!(
            second.is_err(),
            "the guard must clear resnapshot_active; got {second:?}"
        );
    }

    #[tokio::test]
    async fn listener_resnapshot_does_not_deadlock_on_its_own_barrier() {
        let bus = HarnessEventBus::new();
        let context = Context::background();
        let filter: EventFilter =
            Arc::new(|event: &HarnessEvent| event.event_type() == HarnessEventType::Fault);
        let capture: ResnapshotCapture<u32> =
            Arc::new(|_context: Context, boundary: MarkBoundary| {
                async move {
                    boundary.mark()?;
                    Ok(2)
                }
                .boxed()
            });
        let handle = std::sync::Arc::new(
            bus.watch(1, filter, Some(capture))
                .expect("watch registration"),
        );
        let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let watcher_log = Arc::clone(&log);
        handle
            .start(Arc::new(move |event: HarnessEvent, _context: Context| {
                let watcher_log = Arc::clone(&watcher_log);
                async move {
                    let code = match event.payload {
                        HarnessEventPayload::Fault { code, .. } => code,
                        _ => String::from("other"),
                    };
                    let entry: &'static str = match code.as_str() {
                        "outer" => "watcher-outer",
                        "post-boundary" => "watcher-post",
                        _ => "watcher-other",
                    };
                    watcher_log.lock().expect("log").push(entry);
                }
                .boxed()
            }))
            .expect("watch start");

        let listener_bus = bus.clone();
        let listener_handle = std::sync::Arc::clone(&handle);
        let listener_log = Arc::clone(&log);
        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_listener = Arc::clone(&fired);
        let _listener_guard = bus
            .on(
                HarnessEventType::Fault,
                Arc::new(move |_event: HarnessEvent, context: Context| {
                    let bus = listener_bus.clone();
                    let handle = std::sync::Arc::clone(&listener_handle);
                    let log = Arc::clone(&listener_log);
                    let fired = Arc::clone(&fired_for_listener);
                    async move {
                        // The post-boundary emit also carries Fault; only the
                        // first listener run drives the resnapshot.
                        if fired.swap(true, Ordering::AcqRel) {
                            return;
                        }
                        // Admitted on the drain chain before the mark; the
                        // barrier must not overtake it out of existence.
                        bus.emit(handler_error_event(), &context).await;
                        let snapshot = handle.resnapshot(&context).await.expect("resnapshot");
                        assert_eq!(*snapshot, 2);
                        log.lock().expect("log").push("resnapshot-done");
                    }
                    .boxed()
                }),
            )
            .expect("listener registration");

        tokio::time::timeout(
            Duration::from_secs(10),
            bus.emit(fault_event("outer"), &context),
        )
        .await
        .expect("a listener resnapshot must not deadlock on its own barrier");
        assert!(
            fired.load(Ordering::Acquire),
            "the first Fault must drive the listener: {log:?}"
        );

        // Delivered after the resnapshot completed: the watcher holds it
        // behind the boundary and replays it once acceptance is restored.
        tokio::time::timeout(
            Duration::from_secs(10),
            bus.emit(fault_event("post-boundary"), &context),
        )
        .await
        .expect("post-boundary delivery must not strand");

        // Emission resolution proves only the push; wait for the watcher's
        // serialized callbacks to run both deliveries.
        for _ in 0..1000 {
            if log.lock().expect("log").len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let log = log.lock().expect("log").clone();
        assert_eq!(
            log,
            vec!["resnapshot-done", "watcher-outer", "watcher-post"],
            "exact delivery order proves the boundary held and nothing leaked"
        );
        assert_eq!(*handle.snapshot(), 2);
    }
}
