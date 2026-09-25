//! Stable remote service bindings and lifetime management.

mod facade;
mod lifecycle;

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use futures::future::BoxFuture;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
/// Error reporter invoked when a binding task encounters a fatal error.
pub type BindingErrorReporter = Arc<dyn Fn(ServiceError) + Send + Sync>;
/// Access checker invoked before every binding handle operation.
pub type BindingAccessChecker = Arc<dyn Fn() -> Result<(), ServiceError> + Send + Sync>;

pub use self::facade::{RemoteServiceFacade, RemoteServiceMember};
use self::lifecycle::{Lifecycle, await_with_lifetime};
use super::delta::DeltaOp;
use super::error::{RemoteServiceErrorCode, ServiceError};
use super::transport::{RemoteServiceTransport, ServiceSubscription};
use super::value::JsString;
use super::wire::{
    ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMode, ServiceProviderUpdate,
};

/// Asynchronous handler invoked for each live keyed instance.
pub type KeyedObserver = Arc<
    dyn Fn(Arc<RemoteServiceFacade>, Context) -> BoxFuture<'static, Result<(), ServiceError>>
        + Send
        + Sync,
>;

/// Options for one consumer binding.
pub struct BindingOptions {
    /// Service identifiers the binding is permitted to acquire.
    pub services: Vec<JsString>,
    /// Transport capability used for invocation and subscriptions.
    pub transport: Arc<dyn RemoteServiceTransport>,
    /// Whether subscriptions should be opened immediately.
    pub bound: bool,
    /// Receives errors from update and observer tasks.
    pub on_error: Option<BindingErrorReporter>,
    /// Checks the host access boundary before every handle operation.
    pub assert_access: Option<BindingAccessChecker>,
}
impl BindingOptions {
    /// Creates a bound binding with no optional callbacks.
    #[must_use]
    pub fn new(services: Vec<JsString>, transport: Arc<dyn RemoteServiceTransport>) -> Self {
        Self {
            services,
            transport,
            bound: true,
            on_error: None,
            assert_access: None,
        }
    }
}

/// Stable, allowlisted consumer binding over a [`RemoteServiceTransport`].
#[derive(Clone)]
pub struct RemoteServiceBinding {
    inner: Arc<BindingInner>,
}

struct BindingInner {
    transport: Arc<dyn RemoteServiceTransport>,
    allowlist: BTreeSet<JsString>,
    modes: Mutex<BTreeMap<JsString, ServiceMode>>,
    lifecycle: Arc<Lifecycle>,
    singletons: Mutex<BTreeMap<JsString, Arc<SingletonBinding>>>,
    keyed: Mutex<BTreeMap<JsString, Arc<KeyedBinding>>>,
    readiness_revision: AtomicU64,
    transition: tokio::sync::Mutex<()>,
}

struct SingletonBinding {
    service_id: JsString,
    facade: Arc<RemoteServiceFacade>,
    active: AtomicBool,
    /// Shared with the facade so commits re-validate under the members lock.
    revision: Arc<AtomicU64>,
    subscription: Mutex<Option<Arc<dyn ServiceSubscription>>>,
    starting: Mutex<Option<JoinHandle<Result<(), ServiceError>>>>,
    start_cancel: Mutex<Option<CancellationToken>>,
}

struct KeyedBinding {
    service_id: JsString,
    transport: Arc<dyn RemoteServiceTransport>,
    lifecycle: Arc<Lifecycle>,
    observers: Mutex<BTreeMap<u64, Arc<Observer>>>,
    next_observer: AtomicU64,
    instances: Mutex<BTreeMap<JsString, Arc<KeyedInstance>>>,
    subscription: Mutex<Option<Arc<dyn ServiceSubscription>>>,
    starting: Mutex<Option<JoinHandle<Result<(), ServiceError>>>>,
    start_cancel: Mutex<Option<CancellationToken>>,
    /// Shared with every instance facade so commits re-validate under the members lock.
    revision: Arc<AtomicU64>,
    /// Runtime captured at startup; update listeners may run off-runtime.
    runtime: Mutex<Option<tokio::runtime::Handle>>,
    ready: AtomicBool,
    closed: AtomicBool,
}

struct KeyedInstance {
    address: ServiceInstanceAddress,
    facade: Arc<RemoteServiceFacade>,
}

struct Observer {
    handler: KeyedObserver,
    active: AtomicBool,
    tasks: Mutex<Vec<ObserverTask>>,
}

/// Monotonic identity source for spawned observer callbacks.
static NEXT_OBSERVER_TASK: AtomicU64 = AtomicU64::new(1);

struct ObserverTask {
    /// Identity of the spawned handler, used to exclude a self-close.
    id: u64,
    address: ServiceInstanceAddress,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

tokio::task_local! {
    /// Identity of the running observer callback, visible to self-closes.
    static OBSERVER_TASK: u64;
}

/// Registration returned by [`RemoteServiceBinding::observe_keyed`].
///
/// Dropping or stopping it cancels the handler tasks. Use [`Self::close`] when
/// the caller also needs to await their drain.
pub struct ServiceObservation {
    binding: Weak<KeyedBinding>,
    observer_id: u64,
    stopped: AtomicBool,
}

impl Drop for ServiceObservation {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ServiceObservation {
    /// Cancels this observer without waiting for an already-running callback.
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(binding) = self.binding.upgrade() else {
            return;
        };
        let tasks = binding.remove_observer(self.observer_id);
        cancel_and_detach(tasks);
        // Spawn on the runtime captured at startup: the final observer may be
        // dropped from a thread without a Tokio context, where
        // `Handle::try_current` would silently skip the subscription teardown
        // and leak it until the binding is disposed.
        let runtime = lock(&binding.runtime).clone();
        if binding.observer_count() == 0
            && let Some(handle) = runtime
        {
            handle.spawn(async move {
                let _ = binding.stop_if_empty(Context::background()).await;
            });
        }
    }

    /// Cancels this observer and waits until every callback task has settled.
    ///
    /// A callback closing its own observation never awaits its own task.
    ///
    /// # Errors
    /// Returns an error if the binding is disposed, the context is cancelled,
    /// or awaiting the keyed binding stop fails.
    pub async fn close(&self, context: Context) -> Result<(), ServiceError> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(binding) = self.binding.upgrade() else {
            return Ok(());
        };
        let tasks = binding.remove_observer(self.observer_id);
        let self_task = OBSERVER_TASK.try_with(|id| *id).ok();
        cancel_and_drain(tasks, self_task).await;
        binding.stop_if_empty(context).await
    }
}

impl RemoteServiceBinding {
    /// Creates an allowlisted binding. Duplicate IDs are rejected before any transport call.
    ///
    /// # Errors
    /// Returns an error when a service ID is empty or duplicated.
    pub fn new(options: BindingOptions) -> Result<Self, ServiceError> {
        let mut allowlist = BTreeSet::new();
        for id in &options.services {
            if id.as_utf16().is_empty() {
                return Err(ServiceError::local(
                    "Remote service binding IDs must not be empty",
                ));
            }
            if !allowlist.insert(id.clone()) {
                return Err(ServiceError::local(
                    "Remote service binding has duplicate service IDs",
                ));
            }
        }
        let report_error = options.on_error.unwrap_or_else(|| Arc::new(|_| {}));
        let assert_access = options.assert_access.unwrap_or_else(|| Arc::new(|| Ok(())));
        let lifecycle = Lifecycle::new(options.bound, report_error, assert_access);
        Ok(Self {
            inner: Arc::new(BindingInner {
                transport: options.transport,
                allowlist,
                modes: Mutex::new(BTreeMap::new()),
                lifecycle,
                singletons: Mutex::new(BTreeMap::new()),
                keyed: Mutex::new(BTreeMap::new()),
                readiness_revision: AtomicU64::new(0),
                transition: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Acquires a stable singleton facade. Replacements reuse the same facade and member handles.
    ///
    /// # Errors
    /// Returns an error when the service is not allowlisted or is already used as a different mode.
    pub fn use_service(
        &self,
        service_id: &JsString,
    ) -> Result<Arc<RemoteServiceFacade>, ServiceError> {
        self.assert_available(service_id, ServiceMode::Singleton)?;
        let singletons = lock(&self.inner.singletons);
        if self.inner.lifecycle.is_disposed() {
            return Err(ServiceError::disposed("Remote service binding is disposed"));
        }
        if let Some(binding) = singletons.get(service_id).cloned() {
            return Ok(Arc::clone(&binding.facade));
        }
        drop(singletons);
        let revision = Arc::new(AtomicU64::new(0));
        let facade = Arc::new(RemoteServiceFacade::new(
            service_id.clone(),
            None,
            Arc::clone(&self.inner.transport),
            Arc::clone(&self.inner.lifecycle),
            Arc::clone(&revision),
        ));
        let binding = Arc::new(SingletonBinding {
            service_id: service_id.clone(),
            facade: Arc::clone(&facade),
            active: AtomicBool::new(true),
            revision,
            subscription: Mutex::new(None),
            starting: Mutex::new(None),
            start_cancel: Mutex::new(None),
        });
        let mut singletons = lock(&self.inner.singletons);
        if self.inner.lifecycle.is_disposed() {
            return Err(ServiceError::disposed("Remote service binding is disposed"));
        }
        if let Some(existing) = singletons.get(service_id).cloned() {
            return Ok(Arc::clone(&existing.facade));
        }
        singletons.insert(service_id.clone(), Arc::clone(&binding));
        drop(singletons);
        self.inner.readiness_revision.fetch_add(1, Ordering::AcqRel);
        if self.inner.lifecycle.is_bound() {
            self.spawn_singleton_start(&binding)?;
        }
        Ok(facade)
    }

    /// Observes every live generation of a keyed service.
    ///
    /// # Errors
    /// Returns an error when the service is not allowlisted, is already used as a different mode,
    /// or the keyed binding is closed.
    pub fn observe_keyed(
        &self,
        service_id: &JsString,
        handler: KeyedObserver,
    ) -> Result<ServiceObservation, ServiceError> {
        self.assert_available(service_id, ServiceMode::Keyed)?;
        let binding = {
            let mut keyed = lock(&self.inner.keyed);
            if self.inner.lifecycle.is_disposed() {
                return Err(ServiceError::disposed("Remote service binding is disposed"));
            }
            if let Some(binding) = keyed.get(service_id) {
                Arc::clone(binding)
            } else {
                let binding = Arc::new(KeyedBinding::new(
                    service_id.clone(),
                    Arc::clone(&self.inner.transport),
                    Arc::clone(&self.inner.lifecycle),
                ));
                keyed.insert(service_id.clone(), Arc::clone(&binding));
                self.inner.readiness_revision.fetch_add(1, Ordering::AcqRel);
                binding
            }
        };
        let (observer_id, was_empty) = binding.add_observer(handler)?;
        if was_empty && self.inner.lifecycle.is_bound() {
            binding.spawn_start()?;
        } else if !was_empty {
            // A late observer on a ready binding must still see the live
            // instances the first observer's subscription already stored.
            binding.start_tasks_for_observer(observer_id)?;
        }
        Ok(ServiceObservation {
            binding: Arc::downgrade(&binding),
            observer_id,
            stopped: AtomicBool::new(false),
        })
    }
    /// Waits for all currently acquired subscriptions to hydrate and activate.
    ///
    /// # Errors
    /// Returns an error if access is denied, a start task fails, or the context is cancelled.
    pub async fn ready(&self, context: &Context) -> Result<(), ServiceError> {
        self.inner.lifecycle.assert_access()?;
        loop {
            let revision = self.inner.readiness_revision.load(Ordering::Acquire);
            let singleton_starts: Vec<Arc<SingletonBinding>> =
                lock(&self.inner.singletons).values().cloned().collect();
            let keyed_starts: Vec<Arc<KeyedBinding>> =
                lock(&self.inner.keyed).values().cloned().collect();
            for binding in singleton_starts {
                binding.wait_start(context).await?;
            }
            for binding in keyed_starts {
                binding.wait_start(context).await?;
            }
            context.check()?;
            if revision == self.inner.readiness_revision.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }

    /// Closes existing subscriptions, then optionally starts fresh ones.
    ///
    /// # Errors
    /// Returns an error if access is denied, stopping or resetting a binding fails,
    /// restarting fails, or the context is cancelled.
    pub async fn rebind(&self, bound: bool, context: Context) -> Result<(), ServiceError> {
        self.inner.lifecycle.assert_access()?;
        let _transition = self.inner.transition.lock().await;
        self.inner.lifecycle.set_bound(bound);
        self.inner.readiness_revision.fetch_add(1, Ordering::AcqRel);
        let singletons: Vec<Arc<SingletonBinding>> =
            lock(&self.inner.singletons).values().cloned().collect();
        let keyed: Vec<Arc<KeyedBinding>> = lock(&self.inner.keyed).values().cloned().collect();
        for binding in &singletons {
            binding.revision.fetch_add(1, Ordering::AcqRel);
            binding.facade.clear();
            binding.stop(context.clone()).await?;
        }
        for binding in &keyed {
            binding.revision.fetch_add(1, Ordering::AcqRel);
            binding.reset(context.clone(), false).await?;
        }
        if bound {
            for binding in singletons {
                binding.active.store(true, Ordering::Release);
                self.spawn_singleton_start(&binding)?;
                binding.wait_start(&context).await?;
            }
            for binding in keyed {
                if binding.observer_count() > 0 {
                    binding.spawn_start()?;
                    binding.wait_start(&context).await?;
                }
            }
        }
        Ok(())
    }

    /// Disposes all subscriptions, observer tasks, and in-flight calls.
    ///
    /// # Errors
    /// Returns an error if stopping a singleton or closing a keyed binding fails.
    pub async fn dispose(&self, context: Context) -> Result<(), ServiceError> {
        if self.inner.lifecycle.is_disposed() {
            return Ok(());
        }
        self.inner.lifecycle.dispose();
        self.inner.readiness_revision.fetch_add(1, Ordering::AcqRel);
        let singletons: Vec<Arc<SingletonBinding>> = {
            let mut map = lock(&self.inner.singletons);
            std::mem::take(&mut *map).into_values().collect()
        };
        let keyed: Vec<Arc<KeyedBinding>> = {
            let mut map = lock(&self.inner.keyed);
            std::mem::take(&mut *map).into_values().collect()
        };
        let mut first_error = None;
        for binding in singletons {
            binding.active.store(false, Ordering::Release);
            binding.revision.fetch_add(1, Ordering::AcqRel);
            binding.facade.deactivate();
            if let Err(error) = binding.stop(context.clone()).await {
                first_error.get_or_insert(error);
            }
        }
        for binding in keyed {
            binding.revision.fetch_add(1, Ordering::AcqRel);
            if let Err(error) = binding.close(context.clone()).await {
                first_error.get_or_insert(error);
            }
        }
        self.inner.lifecycle.calls.drain().await;
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    fn assert_available(
        &self,
        service_id: &JsString,
        mode: ServiceMode,
    ) -> Result<(), ServiceError> {
        self.inner.lifecycle.assert_access()?;
        if !self.inner.allowlist.contains(service_id) {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceNotAllowed,
                format!(
                    "Remote service {} is not allowlisted",
                    display_js(service_id)
                ),
            ));
        }
        let mut modes = lock(&self.inner.modes);
        if let Some(existing) = modes.get(service_id).copied()
            && existing != mode
        {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceModeMismatch,
                format!(
                    "Remote service {} is already used as {}",
                    display_js(service_id),
                    existing.as_str()
                ),
            ));
        }
        modes.insert(service_id.clone(), mode);
        Ok(())
    }

    fn spawn_singleton_start(&self, binding: &Arc<SingletonBinding>) -> Result<(), ServiceError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service binding requires a Tokio runtime"))?;
        let mut starting = lock(&binding.starting);
        if starting.is_some() {
            return Ok(());
        }
        let revision = binding.revision.load(Ordering::Acquire);
        let token = CancellationToken::new();
        *lock(&binding.start_cancel) = Some(token.clone());
        let transport = Arc::clone(&self.inner.transport);
        let lifecycle = Arc::clone(&self.inner.lifecycle);
        let task_binding = Arc::clone(binding);
        let task = handle.spawn(async move {
            start_singleton(task_binding, transport, lifecycle, revision, token).await
        });
        *starting = Some(task);
        Ok(())
    }
}

impl SingletonBinding {
    async fn wait_start(&self, context: &Context) -> Result<(), ServiceError> {
        let task = lock(&self.starting).take();
        let Some(task) = task else {
            return Ok(());
        };
        let (restore, result) = join_start_or_cancel(
            task,
            context.token().cloned(),
            "Remote service start task failed",
        )
        .await;
        if let Some(task) = restore {
            *lock(&self.starting) = Some(task);
        }
        result
    }

    async fn stop(&self, context: Context) -> Result<(), ServiceError> {
        if let Some(token) = lock(&self.start_cancel).take() {
            token.cancel();
        }
        let start = lock(&self.starting).take();
        if let Some(start) = start {
            let _ = start.await;
        }
        let subscription = lock(&self.subscription).take();
        if let Some(subscription) = subscription {
            subscription.close(context).await?;
        }
        Ok(())
    }

    fn update(&self, update: &ServiceProviderUpdate<DeltaOp>, context: &Context, revision: u64) {
        if !self.active.load(Ordering::Acquire) || self.revision.load(Ordering::Acquire) != revision
        {
            return;
        }
        let result = match update {
            ServiceProviderUpdate::Unavailable => {
                self.facade.clear();
                Ok(())
            }
            ServiceProviderUpdate::Replaced { snapshot } => {
                if snapshot.instance.is_some() {
                    Err(ServiceError::local(
                        "Singleton replacement has an instance address",
                    ))
                } else {
                    self.facade.install(snapshot, context, revision)
                }
            }
            ServiceProviderUpdate::State {
                instance: None,
                member,
                sequence,
                ops,
            } => self
                .facade
                .update(member, *sequence, ops, context, revision),
            ServiceProviderUpdate::State {
                instance: Some(_), ..
            }
            | ServiceProviderUpdate::Spawned { .. }
            | ServiceProviderUpdate::Closed { .. } => Err(ServiceError::local(
                "Singleton received a keyed lifecycle update",
            )),
        };
        if let Err(error) = result
            && self.active.load(Ordering::Acquire)
            && self.revision.load(Ordering::Acquire) == revision
        {
            self.facade.inner.lifecycle.report(error);
        }
    }
}
async fn start_singleton(
    binding: Arc<SingletonBinding>,
    transport: Arc<dyn RemoteServiceTransport>,
    lifecycle: Arc<Lifecycle>,
    revision: u64,
    token: CancellationToken,
) -> Result<(), ServiceError> {
    let listener_binding = Arc::clone(&binding);
    let listener = Arc::new(
        move |update: &ServiceProviderUpdate<DeltaOp>, context: &Context| {
            listener_binding.update(update, context, revision);
        },
    );
    let subscription = await_with_lifetime(
        &Context::background(),
        token,
        transport.subscribe(
            binding.service_id.clone(),
            ServiceMode::Singleton,
            listener,
            Context::background(),
        ),
    )
    .await?;
    if !binding.active.load(Ordering::Acquire)
        || !lifecycle.is_bound()
        || lifecycle.is_disposed()
        || binding.revision.load(Ordering::Acquire) != revision
    {
        subscription.close(Context::background()).await?;
        return Ok(());
    }
    let snapshot = subscription.snapshot().clone();
    if snapshot.service_id != binding.service_id
        || snapshot.mode != ServiceMode::Singleton
        || snapshot.instances.len() != 1
        || snapshot.instances[0].instance.is_some()
    {
        subscription.close(Context::background()).await?;
        return Err(ServiceError::local(format!(
            "Remote service {} returned an invalid singleton snapshot",
            display_js(&binding.service_id)
        )));
    }
    if let Err(error) =
        binding
            .facade
            .install(&snapshot.instances[0], &Context::background(), revision)
    {
        if binding.revision.load(Ordering::Acquire) != revision {
            // Superseded by a lifecycle transition; nothing was recorded yet.
            subscription.close(Context::background()).await?;
            return Ok(());
        }
        binding.facade.deactivate();
        if let Err(cleanup_error) = subscription.close(Context::background()).await {
            *lock(&binding.subscription) = Some(Arc::clone(&subscription));
            lifecycle.report(cleanup_error);
        }
        return Err(error);
    }
    *lock(&binding.subscription) = Some(Arc::clone(&subscription));
    subscription.activate();
    // Revalidate after recording and activating: a concurrent rebind or
    // dispose must not leave this subscription tracked and live.
    if !binding.active.load(Ordering::Acquire)
        || !lifecycle.is_bound()
        || lifecycle.is_disposed()
        || binding.revision.load(Ordering::Acquire) != revision
    {
        lock(&binding.subscription).take();
        binding.facade.deactivate();
        subscription.close(Context::background()).await?;
        return Ok(());
    }
    Ok(())
}

impl KeyedBinding {
    fn new(
        service_id: JsString,
        transport: Arc<dyn RemoteServiceTransport>,
        lifecycle: Arc<Lifecycle>,
    ) -> Self {
        Self {
            service_id,
            transport,
            lifecycle,
            observers: Mutex::new(BTreeMap::new()),
            next_observer: AtomicU64::new(1),
            instances: Mutex::new(BTreeMap::new()),
            subscription: Mutex::new(None),
            starting: Mutex::new(None),
            start_cancel: Mutex::new(None),
            revision: Arc::new(AtomicU64::new(0)),
            runtime: Mutex::new(None),
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn observer_count(&self) -> usize {
        lock(&self.observers).len()
    }

    fn add_observer(&self, handler: KeyedObserver) -> Result<(u64, bool), ServiceError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ServiceError::disposed(
                "Remote keyed service binding is closed",
            ));
        }
        let id = self.next_observer.fetch_add(1, Ordering::AcqRel);
        let mut observers = lock(&self.observers);
        let was_empty = observers.is_empty();
        observers.insert(
            id,
            Arc::new(Observer {
                handler,
                active: AtomicBool::new(true),
                tasks: Mutex::new(Vec::new()),
            }),
        );
        Ok((id, was_empty))
    }

    fn remove_observer(&self, id: u64) -> Vec<ObserverTask> {
        let observer = lock(&self.observers).remove(&id);
        let Some(observer) = observer else {
            return Vec::new();
        };
        observer.active.store(false, Ordering::Release);
        std::mem::take(&mut *lock(&observer.tasks))
    }

    async fn stop_if_empty(self: &Arc<Self>, context: Context) -> Result<(), ServiceError> {
        if self.observer_count() != 0 {
            return Ok(());
        }
        self.reset(context, false).await?;
        // The reset awaited teardown, so an observer may have registered in
        // the meantime; restart startup so it is not left behind a shutdown.
        if !self.closed.load(Ordering::Acquire)
            && self.lifecycle.is_bound()
            && self.observer_count() > 0
        {
            self.spawn_start()?;
        }
        Ok(())
    }

    fn spawn_start(self: &Arc<Self>) -> Result<(), ServiceError> {
        if self.closed.load(Ordering::Acquire) || !self.lifecycle.is_bound() {
            return Ok(());
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service binding requires a Tokio runtime"))?;
        // Update listeners may run off-runtime; capture the handle for them.
        *lock(&self.runtime) = Some(handle.clone());
        let mut starting = lock(&self.starting);
        if starting.is_some() {
            return Ok(());
        }
        let revision = self.revision.load(Ordering::Acquire);
        let token = CancellationToken::new();
        *lock(&self.start_cancel) = Some(token.clone());
        let binding = Arc::clone(self);
        let task = handle.spawn(async move { binding.start(revision, token).await });
        *starting = Some(task);
        Ok(())
    }

    async fn wait_start(&self, context: &Context) -> Result<(), ServiceError> {
        let task = lock(&self.starting).take();
        let Some(task) = task else {
            return Ok(());
        };
        let (restore, result) = join_start_or_cancel(
            task,
            context.token().cloned(),
            "Remote keyed service start task failed",
        )
        .await;
        if let Some(task) = restore {
            *lock(&self.starting) = Some(task);
        }
        result
    }

    async fn start(
        self: Arc<Self>,
        revision: u64,
        token: CancellationToken,
    ) -> Result<(), ServiceError> {
        let listener_binding = Arc::clone(&self);
        let listener = Arc::new(
            move |update: &ServiceProviderUpdate<DeltaOp>, context: &Context| {
                listener_binding.update(update, context, revision);
            },
        );
        let subscription = await_with_lifetime(
            &Context::background(),
            token.clone(),
            self.transport.subscribe(
                self.service_id.clone(),
                ServiceMode::Keyed,
                listener,
                Context::background(),
            ),
        )
        .await?;
        if self.closed.load(Ordering::Acquire)
            || !self.lifecycle.is_bound()
            || self.revision.load(Ordering::Acquire) != revision
        {
            subscription.close(Context::background()).await?;
            return Ok(());
        }
        let snapshot = subscription.snapshot().clone();
        if snapshot.service_id != self.service_id || snapshot.mode != ServiceMode::Keyed {
            subscription.close(Context::background()).await?;
            return Err(ServiceError::local(format!(
                "Remote service {} returned the wrong keyed snapshot",
                display_js(&self.service_id)
            )));
        }
        for instance in &snapshot.instances {
            if let Err(error) = self.spawn_instance(instance, &Context::background(), revision) {
                let tasks = self.deactivate_instances();
                cancel_and_drain(tasks, None).await;
                subscription.close(Context::background()).await?;
                if self.revision.load(Ordering::Acquire) != revision {
                    // Superseded by a lifecycle transition; treat as graceful.
                    return Ok(());
                }
                return Err(error);
            }
        }
        *lock(&self.subscription) = Some(Arc::clone(&subscription));
        subscription.activate();
        // Revalidate after recording and activating: a concurrent rebind,
        // reset, or dispose must not leave this subscription tracked and live.
        if self.closed.load(Ordering::Acquire)
            || !self.lifecycle.is_bound()
            || self.lifecycle.is_disposed()
            || self.revision.load(Ordering::Acquire) != revision
        {
            lock(&self.subscription).take();
            self.ready.store(false, Ordering::Release);
            let tasks = self.deactivate_instances();
            subscription.close(Context::background()).await?;
            cancel_and_drain(tasks, None).await;
            return Ok(());
        }
        self.ready.store(true, Ordering::Release);
        let instances: Vec<Arc<KeyedInstance>> = lock(&self.instances).values().cloned().collect();
        for instance in &instances {
            self.start_observer_tasks(instance, &Context::background())?;
        }
        Ok(())
    }

    async fn reset(&self, context: Context, permanent: bool) -> Result<(), ServiceError> {
        // Invalidate callbacks from the subscription being closed before any
        // teardown: an in-flight Spawned update must not repopulate the map.
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.ready.store(false, Ordering::Release);
        if permanent {
            self.closed.store(true, Ordering::Release);
        }
        let instance_tasks = self.deactivate_instances();
        cancel_and_drain(instance_tasks, None).await;
        if let Some(token) = lock(&self.start_cancel).take() {
            token.cancel();
        }
        let start = lock(&self.starting).take();
        if let Some(start) = start {
            let _ = start.await;
        }
        let subscription = lock(&self.subscription).take();
        if let Some(subscription) = subscription {
            subscription.close(context).await?;
        }
        if permanent {
            let observers: Vec<Arc<Observer>> = {
                let mut map = lock(&self.observers);
                std::mem::take(&mut *map).into_values().collect()
            };
            let mut tasks = Vec::new();
            for observer in observers {
                observer.active.store(false, Ordering::Release);
                tasks.extend(std::mem::take(&mut *lock(&observer.tasks)));
            }
            cancel_and_drain(tasks, None).await;
        }
        Ok(())
    }

    async fn close(&self, context: Context) -> Result<(), ServiceError> {
        self.reset(context, true).await
    }

    fn deactivate_instances(&self) -> Vec<ObserverTask> {
        let instances: Vec<Arc<KeyedInstance>> = {
            let mut map = lock(&self.instances);
            std::mem::take(&mut *map).into_values().collect()
        };
        let mut tasks = Vec::new();
        for instance in instances {
            instance.facade.deactivate();
            tasks.extend(self.take_tasks_for(&instance.address));
        }
        tasks
    }

    fn take_tasks_for(&self, address: &ServiceInstanceAddress) -> Vec<ObserverTask> {
        let observers: Vec<Arc<Observer>> = lock(&self.observers).values().cloned().collect();
        let mut tasks = Vec::new();
        for observer in observers {
            let mut owned = lock(&observer.tasks);
            let mut retained = Vec::with_capacity(owned.len());
            for task in owned.drain(..) {
                if task.address == *address {
                    tasks.push(task);
                } else {
                    retained.push(task);
                }
            }
            *owned = retained;
        }
        tasks
    }
    fn cancel_tasks_for(&self, address: &ServiceInstanceAddress) {
        let observers: Vec<Arc<Observer>> = lock(&self.observers).values().cloned().collect();
        for observer in observers {
            for task in lock(&observer.tasks).iter() {
                if task.address == *address {
                    task.cancel.cancel();
                }
            }
        }
    }

    fn spawn_instance(
        &self,
        snapshot: &ServiceInstanceSnapshot<DeltaOp>,
        context: &Context,
        revision: u64,
    ) -> Result<(), ServiceError> {
        let address = snapshot
            .instance
            .clone()
            .ok_or_else(|| ServiceError::local("Keyed service instance snapshot has no address"))?;
        let facade = Arc::new(RemoteServiceFacade::new(
            self.service_id.clone(),
            Some(address.clone()),
            Arc::clone(&self.transport),
            Arc::clone(&self.lifecycle),
            Arc::clone(&self.revision),
        ));
        facade.install(snapshot, context, revision)?;
        let instance = Arc::new(KeyedInstance {
            address: address.clone(),
            facade,
        });
        let previous = {
            let mut instances = lock(&self.instances);
            // Serialize with reset: once a reset invalidates this revision the
            // instance must not re-enter the supposedly emptied map.
            if self.revision.load(Ordering::Acquire) != revision {
                return Err(ServiceError::local(
                    "Keyed instance install was superseded by a binding transition",
                ));
            }
            if let Some(existing) = instances.get(&address.key)
                && existing.address.generation == address.generation
            {
                return Err(ServiceError::local(
                    "Keyed service repeated a live generation",
                ));
            }
            instances.insert(address.key.clone(), Arc::clone(&instance))
        };
        if let Some(previous) = previous {
            previous.facade.deactivate();
            self.cancel_tasks_for(&previous.address);
        }
        if self.ready.load(Ordering::Acquire) {
            self.start_observer_tasks(&instance, context)?;
        }
        Ok(())
    }

    fn start_observer_tasks(
        &self,
        instance: &Arc<KeyedInstance>,
        context: &Context,
    ) -> Result<(), ServiceError> {
        let observers: Vec<Arc<Observer>> = lock(&self.observers).values().cloned().collect();
        for observer in observers {
            self.spawn_observer_task(&observer, instance, context)?;
        }
        Ok(())
    }

    /// Schedules a newly registered observer against every live instance of a
    /// ready binding, so late observers see the instances they missed.
    fn start_tasks_for_observer(&self, observer_id: u64) -> Result<(), ServiceError> {
        if !self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let observer = lock(&self.observers).get(&observer_id).cloned();
        let Some(observer) = observer else {
            return Ok(());
        };
        let instances: Vec<Arc<KeyedInstance>> = lock(&self.instances).values().cloned().collect();
        for instance in &instances {
            self.spawn_observer_task(&observer, instance, &Context::background())?;
        }
        Ok(())
    }

    fn spawn_observer_task(
        &self,
        observer: &Arc<Observer>,
        instance: &Arc<KeyedInstance>,
        context: &Context,
    ) -> Result<(), ServiceError> {
        // Update listeners may fire off-runtime; use the handle captured at
        // startup, falling back to the current runtime when available.
        let handle = match lock(&self.runtime).clone() {
            Some(handle) => handle,
            None => tokio::runtime::Handle::try_current().map_err(|_| {
                ServiceError::local("Remote service binding requires a Tokio runtime")
            })?,
        };
        let (task_context, cancel) = context.with_cancel();
        let handler = Arc::clone(&observer.handler);
        let facade = Arc::clone(&instance.facade);
        let lifecycle = Arc::clone(&self.lifecycle);
        let task_cancel = cancel.clone();
        let address = instance.address.clone();
        let task_id = NEXT_OBSERVER_TASK.fetch_add(1, Ordering::AcqRel);
        // Record the task under the observer's task lock, and skip spawning
        // once the observer is disabled: a stop either observes the recorded
        // task or disables the observer before the callback can start.
        let mut tasks = lock(&observer.tasks);
        if !observer.active.load(Ordering::Acquire) {
            return Ok(());
        }
        let join = handle.spawn(OBSERVER_TASK.scope(task_id, async move {
            if let Err(error) = handler(facade, task_context).await
                && !task_cancel.is_cancelled()
                && !lifecycle.is_disposed()
            {
                lifecycle.report(error);
            }
        }));
        tasks.push(ObserverTask {
            id: task_id,
            address,
            cancel,
            join,
        });
        Ok(())
    }

    fn update(&self, update: &ServiceProviderUpdate<DeltaOp>, context: &Context, revision: u64) {
        if self.closed.load(Ordering::Acquire) || self.revision.load(Ordering::Acquire) != revision
        {
            return;
        }
        let result = match update {
            ServiceProviderUpdate::Unavailable | ServiceProviderUpdate::Replaced { .. } => Err(
                ServiceError::local("Keyed service received a singleton lifecycle update"),
            ),
            ServiceProviderUpdate::Spawned { instance } => {
                self.spawn_instance(instance, context, revision)
            }
            ServiceProviderUpdate::Closed { instance } => {
                let current = lock(&self.instances).get(&instance.key).cloned();
                if current
                    .as_ref()
                    .is_some_and(|value| value.address.generation == instance.generation)
                {
                    let removed = lock(&self.instances).remove(&instance.key);
                    if let Some(removed) = removed {
                        removed.facade.deactivate();
                        self.cancel_tasks_for(&removed.address);
                    }
                }
                Ok(())
            }
            ServiceProviderUpdate::State {
                instance: Some(address),
                member,
                sequence,
                ops,
            } => {
                let current = lock(&self.instances).get(&address.key).cloned();
                match current {
                    Some(instance)
                        if instance.address.generation == address.generation
                            && instance.facade.inner.is_active() =>
                    {
                        instance
                            .facade
                            .update(member, *sequence, ops, context, revision)
                    }
                    _ => Ok(()),
                }
            }
            ServiceProviderUpdate::State { instance: None, .. } => Err(ServiceError::local(
                "Keyed state update has no instance address",
            )),
        };
        if let Err(error) = result
            && !self.closed.load(Ordering::Acquire)
            && self.revision.load(Ordering::Acquire) == revision
        {
            self.lifecycle.report(error);
        }
    }
}

fn cancel_and_detach(tasks: Vec<ObserverTask>) {
    for task in tasks {
        task.cancel.cancel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = task.join.await;
            });
        }
    }
}

async fn cancel_and_drain(tasks: Vec<ObserverTask>, exclude: Option<u64>) {
    for task in tasks {
        task.cancel.cancel();
        if exclude.is_some_and(|id| task.id == id) {
            // A handler closing its own observation: awaiting its join would
            // wait for the close itself, so cancel it and detach instead.
            continue;
        }
        let _ = task.join.await;
    }
}

/// Awaits a start task, racing it against the caller's cancellation. Returns
/// the still-pending handle on cancellation so the caller can restore it for
/// later lifecycle cleanup; never abandons a started task.
async fn join_start_or_cancel(
    task: JoinHandle<Result<(), ServiceError>>,
    caller_cancellation: Option<CancellationToken>,
    failure_message: &'static str,
) -> (
    Option<JoinHandle<Result<(), ServiceError>>>,
    Result<(), ServiceError>,
) {
    let mut join = Some(task);
    let cancelled = caller_cancellation;
    std::future::poll_fn(move |cx| {
        // Cancellation wins ties over a ready start result. A fresh waiter is
        // created and polled on every pass so the caller's waker stays
        // registered with the token between polls.
        if let Some(token) = cancelled.as_ref() {
            if token.is_cancelled() {
                return std::task::Poll::Ready((join.take(), Err(ServiceError::Cancelled)));
            }
            let mut waiter = std::pin::pin!(token.cancelled());
            if waiter.as_mut().poll(cx).is_ready() {
                return std::task::Poll::Ready((join.take(), Err(ServiceError::Cancelled)));
            }
        }
        let Some(task) = join.as_mut() else {
            return std::task::Poll::Ready((None, Ok(())));
        };
        match Pin::new(task).poll(cx) {
            std::task::Poll::Ready(joined) => {
                let result = match joined {
                    Ok(result) => result,
                    Err(error) => Err(ServiceError::internal_with_source(failure_message, error)),
                };
                join = None;
                std::task::Poll::Ready((None, result))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .await
}

fn display_js(value: &JsString) -> String {
    String::from_utf16_lossy(value.as_utf16())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test assertions use expect for concise failure"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::FutureExt;
    use futures::future::BoxFuture;

    use super::*;
    use crate::service::delta::{DeltaOp, PathSegment, StatePath};
    use crate::service::provider::{
        RemoteServiceProvider, ServiceDefinition, ServiceImplementation, ServiceMember,
        ServiceMethod,
    };
    use crate::service::replicated::ReplicatedStateDeliveryKind;
    use crate::service::transport::{
        RemoteServiceTransport, ServiceSubscription, ServiceUpdateListener,
    };
    use crate::service::value::{JsInteger, JsObject, JsonValue};
    use crate::service::wire::{
        ServiceCall, ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshot,
        ServiceSubscriptionSnapshot,
    };

    fn definition(id: &JsString, mode: ServiceMode) -> ServiceDefinition {
        ServiceDefinition {
            id: id.clone(),
            local: false,
            mode,
        }
    }

    fn method(result: Option<JsonValue>) -> ServiceMethod {
        let result = Arc::new(result);
        Arc::new(move |_args, _context| {
            let result = Arc::clone(&result);
            async move { Ok(result.as_ref().clone()) }.boxed()
        })
    }

    fn object(number: f64) -> JsonValue {
        JsonValue::Object(JsObject::from([(
            JsString::from_utf8("number"),
            JsonValue::Number(number),
        )]))
    }

    fn implementation(member: ServiceMember) -> ServiceImplementation {
        BTreeMap::from([(JsString::from_utf8("member"), member)])
    }

    struct TrackingSubscription {
        snapshot: Arc<ServiceSubscriptionSnapshot<DeltaOp>>,
        closes: Arc<AtomicUsize>,
    }

    impl ServiceSubscription for TrackingSubscription {
        fn snapshot(&self) -> &ServiceSubscriptionSnapshot<DeltaOp> {
            self.snapshot.as_ref()
        }

        fn activate(&self) {}

        fn close(&self, _context: Context) -> BoxFuture<'_, Result<(), ServiceError>> {
            self.closes.fetch_add(1, Ordering::AcqRel);
            async { Ok(()) }.boxed()
        }
    }

    struct TrackingTransport {
        snapshot: Arc<ServiceSubscriptionSnapshot<DeltaOp>>,
        closes: Arc<AtomicUsize>,
        invokes: Arc<AtomicUsize>,
    }

    impl RemoteServiceTransport for TrackingTransport {
        fn invoke(
            &self,
            _call: ServiceCall,
            _context: Context,
        ) -> BoxFuture<'_, Result<Option<JsonValue>, ServiceError>> {
            self.invokes.fetch_add(1, Ordering::AcqRel);
            async { Err(ServiceError::local("test transport does not invoke")) }.boxed()
        }

        fn subscribe(
            &self,
            _service_id: JsString,
            _mode: ServiceMode,
            _listener: ServiceUpdateListener,
            _context: Context,
        ) -> BoxFuture<'_, Result<Arc<dyn ServiceSubscription>, ServiceError>> {
            let subscription = TrackingSubscription {
                snapshot: Arc::clone(&self.snapshot),
                closes: Arc::clone(&self.closes),
            };
            async move { Ok(Arc::new(subscription) as Arc<dyn ServiceSubscription>) }.boxed()
        }
    }

    fn tracking_transport(
        snapshot: ServiceSubscriptionSnapshot<DeltaOp>,
    ) -> (Arc<dyn RemoteServiceTransport>, Arc<AtomicUsize>) {
        let (transport, closes, _invokes) = tracking_transport_with_invokes(snapshot);
        (transport, closes)
    }

    fn tracking_transport_with_invokes(
        snapshot: ServiceSubscriptionSnapshot<DeltaOp>,
    ) -> (
        Arc<dyn RemoteServiceTransport>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let closes = Arc::new(AtomicUsize::new(0));
        let invokes = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(TrackingTransport {
            snapshot: Arc::new(snapshot),
            closes: Arc::clone(&closes),
            invokes: Arc::clone(&invokes),
        });
        (transport, closes, invokes)
    }

    #[tokio::test]
    async fn singleton_install_failure_closes_subscription() {
        let service_id = JsString::from_utf8("singleton");
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: service_id.clone(),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: None,
                members: vec![ServiceMemberSnapshot::State {
                    name: JsString::from_utf8("member"),
                    sequence: JsInteger::zero(),
                    ops: vec![DeltaOp::Replace(object(1.0))],
                }],
            }],
        };
        let (transport, closes) = tracking_transport(snapshot);
        let mut options = BindingOptions::new(vec![service_id.clone()], transport);
        options.bound = false;
        let binding = RemoteServiceBinding::new(options).expect("binding");
        let facade = binding.use_service(&service_id).expect("facade");
        let _ = facade
            .member("member")
            .expect("member")
            .invoke(Vec::new(), Context::background())
            .await;

        binding
            .rebind(true, Context::background())
            .await
            .expect_err("singleton install should fail");
        assert_eq!(closes.load(Ordering::Acquire), 1);
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn keyed_start_failure_closes_subscription_and_rolls_back_instances() {
        let service_id = JsString::from_utf8("keyed");
        let first_address = ServiceInstanceAddress {
            key: JsString::from_utf8("first"),
            generation: JsInteger::one(),
        };
        let second_address = ServiceInstanceAddress {
            key: JsString::from_utf8("second"),
            generation: JsInteger::one(),
        };
        let invalid_path = StatePath::new([PathSegment::key("number")]).expect("state path");
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: service_id.clone(),
            mode: ServiceMode::Keyed,
            instances: vec![
                ServiceInstanceSnapshot {
                    instance: Some(first_address),
                    members: vec![ServiceMemberSnapshot::Method {
                        name: JsString::from_utf8("member"),
                    }],
                },
                ServiceInstanceSnapshot {
                    instance: Some(second_address),
                    members: vec![ServiceMemberSnapshot::State {
                        name: JsString::from_utf8("member"),
                        sequence: JsInteger::zero(),
                        ops: vec![DeltaOp::Delete(invalid_path)],
                    }],
                },
            ],
        };
        let (transport, closes) = tracking_transport(snapshot);
        let mut options = BindingOptions::new(vec![service_id.clone()], transport);
        options.bound = false;
        let binding = RemoteServiceBinding::new(options).expect("binding");
        let observation = binding
            .observe_keyed(
                &service_id,
                Arc::new(|_facade, _context| async { Ok(()) }.boxed()),
            )
            .expect("observation");
        let keyed = lock(&binding.inner.keyed)
            .get(&service_id)
            .cloned()
            .expect("keyed binding");

        binding
            .rebind(true, Context::background())
            .await
            .expect_err("keyed install should fail");
        assert_eq!(closes.load(Ordering::Acquire), 1);
        assert!(lock(&keyed.instances).is_empty());

        observation
            .close(Context::background())
            .await
            .expect("close observation");
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn singleton_facade_replacement_preserves_identity_and_omission() {
        let service_id = JsString::from_utf8("singleton");
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![definition(&service_id, ServiceMode::Singleton)])
                .expect("provider"),
        );
        provider
            .provide(
                &service_id,
                implementation(ServiceMember::Method(method(None))),
            )
            .expect("provide");
        let binding = RemoteServiceBinding::new(BindingOptions::new(
            vec![service_id.clone()],
            Arc::clone(&provider) as Arc<dyn RemoteServiceTransport>,
        ))
        .expect("binding");
        let facade = binding.use_service(&service_id).expect("facade");
        binding.ready(&Context::background()).await.expect("ready");
        let member = facade.member("member").expect("member");
        assert_eq!(
            member
                .invoke(Vec::new(), Context::background())
                .await
                .expect("invoke"),
            None
        );

        provider
            .replace(
                &service_id,
                implementation(ServiceMember::Method(method(Some(JsonValue::Null)))),
            )
            .expect("replace");
        let replacement = binding
            .use_service(&service_id)
            .expect("replacement facade");
        assert!(Arc::ptr_eq(&facade, &replacement));
        assert_eq!(
            member
                .invoke(Vec::new(), Context::background())
                .await
                .expect("invoke"),
            Some(JsonValue::Null)
        );
        binding
            .rebind(false, Context::background())
            .await
            .expect("unbind");
        let stale = member
            .invoke(Vec::new(), Context::background())
            .await
            .expect_err("unbound invoke");
        assert!(matches!(
            stale,
            ServiceError::Remote(remote)
                if remote.code == RemoteServiceErrorCode::ServiceStaleInstance
        ));
        binding
            .rebind(true, Context::background())
            .await
            .expect("rebind");
        assert_eq!(
            member
                .invoke(Vec::new(), Context::background())
                .await
                .expect("invoke"),
            Some(JsonValue::Null)
        );
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn state_hydrates_updates_and_clears_without_mutating_previous_revisions() {
        let service_id = JsString::from_utf8("stateful");
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![definition(&service_id, ServiceMode::Singleton)])
                .expect("provider"),
        );
        let mutable = crate::service::replicated::MutableReplicatedState::new(object(1.0));
        provider
            .provide(
                &service_id,
                implementation(ServiceMember::State(Arc::clone(&mutable))),
            )
            .expect("provide");
        let binding = RemoteServiceBinding::new(BindingOptions::new(
            vec![service_id.clone()],
            Arc::clone(&provider) as Arc<dyn RemoteServiceTransport>,
        ))
        .expect("binding");
        let facade = binding.use_service(&service_id).expect("facade");
        binding.ready(&Context::background()).await.expect("ready");
        let replica = facade.state("member").expect("state");
        let first = replica.value().expect("hydrated");
        assert_eq!(first.as_ref(), &object(1.0));

        let deliveries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&deliveries);
        let remove = replica
            .subscribe(Arc::new(move |value, _context, delivery| {
                let mut values = recorded.lock().expect("lock");
                values.push((delivery.kind, delivery.sequence, value));
            }))
            .expect("subscribe");
        mutable.with_state_mut(|value| *value = object(2.0));
        mutable.publish(Context::background()).expect("publish");
        tokio::task::yield_now().await;
        assert_eq!(first.as_ref(), &object(1.0));
        assert_eq!(replica.value().as_deref(), Some(&object(2.0)));
        assert!(
            deliveries
                .lock()
                .expect("lock")
                .iter()
                .any(|(kind, _, _)| *kind == ReplicatedStateDeliveryKind::Update)
        );

        provider.withdraw(&service_id).expect("withdraw");
        assert!(replica.value().is_none());
        remove();
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn keyed_replacement_rejects_stale_facades() {
        let service_id = JsString::from_utf8("keyed");
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![definition(&service_id, ServiceMode::Keyed)])
                .expect("provider"),
        );
        let binding = RemoteServiceBinding::new(BindingOptions::new(
            vec![service_id.clone()],
            Arc::clone(&provider) as Arc<dyn RemoteServiceTransport>,
        ))
        .expect("binding");
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&observed);
        let observation = binding
            .observe_keyed(
                &service_id,
                Arc::new(move |facade, _context| {
                    let record = Arc::clone(&record);
                    async move {
                        record.lock().expect("lock").push(facade);
                        Ok(())
                    }
                    .boxed()
                }),
            )
            .expect("observe");
        binding.ready(&Context::background()).await.expect("ready");
        let first_handle = provider
            .spawn(
                &service_id,
                JsString::from_utf8("room"),
                implementation(ServiceMember::Method(method(Some(JsonValue::Null)))),
            )
            .expect("spawn");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let stale = observed
            .lock()
            .expect("lock")
            .first()
            .cloned()
            .expect("observed facade");
        let stale_member = stale.member("member").expect("member");
        first_handle.close();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let error = stale_member
            .invoke(Vec::new(), Context::background())
            .await
            .expect_err("stale invoke");
        assert!(matches!(
            error,
            ServiceError::Remote(remote)
                if remote.code == RemoteServiceErrorCode::ServiceStaleInstance
        ));
        let second_handle = provider
            .spawn(
                &service_id,
                JsString::from_utf8("room"),
                implementation(ServiceMember::Method(method(Some(JsonValue::Null)))),
            )
            .expect("replacement spawn");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let current = observed
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .expect("replacement facade");
        assert_ne!(
            stale.address().expect("stale address").generation,
            current.address().expect("current address").generation
        );
        assert_eq!(
            current
                .invoke("member", Vec::new(), Context::background())
                .await
                .expect("current invoke"),
            Some(JsonValue::Null)
        );
        second_handle.close();
        observation
            .close(Context::background())
            .await
            .expect("close observation");
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn dispose_cancels_in_flight_calls() {
        let service_id = JsString::from_utf8("hanging");
        let provider = Arc::new(
            RemoteServiceProvider::new(vec![definition(&service_id, ServiceMode::Singleton)])
                .expect("provider"),
        );
        let started = Arc::new(AtomicUsize::new(0));
        let started_by_method = Arc::clone(&started);
        let hanging: ServiceMethod = Arc::new(move |_args, _context| {
            started_by_method.fetch_add(1, Ordering::Relaxed);
            async move { std::future::pending::<Result<Option<JsonValue>, ServiceError>>().await }
                .boxed()
        });
        provider
            .provide(&service_id, implementation(ServiceMember::Method(hanging)))
            .expect("provide");
        let binding = RemoteServiceBinding::new(BindingOptions::new(
            vec![service_id.clone()],
            Arc::clone(&provider) as Arc<dyn RemoteServiceTransport>,
        ))
        .expect("binding");
        let facade = binding.use_service(&service_id).expect("facade");
        binding.ready(&Context::background()).await.expect("ready");
        let call = facade.invoke("member", Vec::new(), Context::background());
        let task = tokio::spawn(call);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            if started.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
        assert!(matches!(
            task.await.expect("call task"),
            Err(ServiceError::Cancelled)
        ));
    }

    fn keyed_snapshot_with_method_instance(
        service_id: &JsString,
        key: &str,
    ) -> ServiceSubscriptionSnapshot<DeltaOp> {
        ServiceSubscriptionSnapshot {
            service_id: service_id.clone(),
            mode: ServiceMode::Keyed,
            instances: vec![ServiceInstanceSnapshot {
                instance: Some(ServiceInstanceAddress {
                    key: JsString::from_utf8(key),
                    generation: JsInteger::one(),
                }),
                members: vec![ServiceMemberSnapshot::Method {
                    name: JsString::from_utf8("member"),
                }],
            }],
        }
    }

    #[tokio::test]
    async fn late_observer_receives_existing_instances() {
        let service_id = JsString::from_utf8("keyed");
        let (transport, _closes) =
            tracking_transport(keyed_snapshot_with_method_instance(&service_id, "room"));
        let binding =
            RemoteServiceBinding::new(BindingOptions::new(vec![service_id.clone()], transport))
                .expect("binding");
        let first_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&first_seen);
        let first = binding
            .observe_keyed(
                &service_id,
                Arc::new(move |facade, _context| {
                    let record = Arc::clone(&record);
                    async move {
                        record.lock().expect("lock").push(facade);
                        Ok(())
                    }
                    .boxed()
                }),
            )
            .expect("observe");
        binding.ready(&Context::background()).await.expect("ready");
        assert_eq!(first_seen.lock().expect("lock").len(), 1);

        let second_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = Arc::clone(&second_seen);
        let second = binding
            .observe_keyed(
                &service_id,
                Arc::new(move |facade, _context| {
                    let record = Arc::clone(&record);
                    async move {
                        record.lock().expect("lock").push(facade);
                        Ok(())
                    }
                    .boxed()
                }),
            )
            .expect("late observe");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            second_seen.lock().expect("lock").len(),
            1,
            "late observer must be scheduled against the live instance"
        );

        second
            .close(Context::background())
            .await
            .expect("close second");
        first
            .close(Context::background())
            .await
            .expect("close first");
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn invoke_rechecks_facade_liveness_before_dispatch() {
        let service_id = JsString::from_utf8("singleton");
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: service_id.clone(),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: None,
                members: vec![ServiceMemberSnapshot::Method {
                    name: JsString::from_utf8("member"),
                }],
            }],
        };
        let (transport, _closes, invokes) = tracking_transport_with_invokes(snapshot);
        let binding =
            RemoteServiceBinding::new(BindingOptions::new(vec![service_id.clone()], transport))
                .expect("binding");
        let facade = binding.use_service(&service_id).expect("facade");
        binding.ready(&Context::background()).await.expect("ready");
        let member = facade.member("member").expect("member");

        // Construct the lazy invocation future, then invalidate the facade
        // before the future is ever polled.
        let pending = member.invoke(Vec::new(), Context::background());
        binding
            .rebind(false, Context::background())
            .await
            .expect("unbind");
        let error = pending.await.expect_err("stale invoke");
        assert!(matches!(
            error,
            ServiceError::Remote(remote)
                if remote.code == RemoteServiceErrorCode::ServiceStaleInstance
        ));
        assert_eq!(
            invokes.load(Ordering::Acquire),
            0,
            "a stale facade must not reach the transport"
        );
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn observer_close_does_not_await_own_callback() {
        let service_id = JsString::from_utf8("keyed");
        let (transport, _closes) =
            tracking_transport(keyed_snapshot_with_method_instance(&service_id, "room"));
        let binding =
            RemoteServiceBinding::new(BindingOptions::new(vec![service_id.clone()], transport))
                .expect("binding");
        let gate = Arc::new(tokio::sync::Notify::new());
        let observation_slot: Arc<std::sync::Mutex<Option<Arc<ServiceObservation>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let entered = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));

        let handler = {
            let gate = Arc::clone(&gate);
            let slot = Arc::clone(&observation_slot);
            let entered = Arc::clone(&entered);
            let finished = Arc::clone(&finished);
            Arc::new(move |_facade, _context| {
                let gate = Arc::clone(&gate);
                let slot = Arc::clone(&slot);
                let entered = Arc::clone(&entered);
                let finished = Arc::clone(&finished);
                async move {
                    entered.fetch_add(1, Ordering::AcqRel);
                    gate.notified().await;
                    let observation = slot.lock().expect("lock").clone().expect("observation");
                    observation
                        .close(Context::background())
                        .await
                        .expect("self close");
                    finished.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                }
                .boxed()
            })
        };
        let observation = Arc::new(
            binding
                .observe_keyed(&service_id, handler)
                .expect("observe"),
        );
        *observation_slot.lock().expect("lock") = Some(Arc::clone(&observation));
        binding.ready(&Context::background()).await.expect("ready");
        for _ in 0..8 {
            tokio::task::yield_now().await;
            if entered.load(Ordering::Acquire) != 0 {
                break;
            }
        }
        assert_eq!(entered.load(Ordering::Acquire), 1, "callback running");

        gate.notify_one();
        let wait = async {
            while finished.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("self-close must not await its own task");
        binding
            .dispose(Context::background())
            .await
            .expect("dispose");
    }

    #[tokio::test]
    async fn use_service_rejects_binding_disposed_during_acquisition() {
        let service_id = JsString::from_utf8("singleton");
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: service_id.clone(),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: None,
                members: vec![ServiceMemberSnapshot::Method {
                    name: JsString::from_utf8("member"),
                }],
            }],
        };
        let (transport, _closes) = tracking_transport(snapshot);
        let lifecycle_slot: Arc<std::sync::Mutex<Option<Arc<Lifecycle>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot = Arc::clone(&lifecycle_slot);
        let mut options = BindingOptions::new(vec![service_id.clone()], transport);
        options.bound = false;
        options.assert_access = Some(Arc::new(move || {
            if let Some(lifecycle) = slot.lock().expect("lock").as_ref() {
                lifecycle.dispose();
            }
            Ok(())
        }));
        let binding = RemoteServiceBinding::new(options).expect("binding");
        *lifecycle_slot.lock().expect("lock") = Some(Arc::clone(&binding.inner.lifecycle));

        // The access checker disposes the binding between the entry check and
        // the map insertion, so acquisition must still fail closed.
        let result = binding.use_service(&service_id);
        assert!(matches!(result, Err(ServiceError::Disposed(_))));
        assert!(
            lock(&binding.inner.singletons).is_empty(),
            "a disposed binding must not gain an untracked singleton entry"
        );
    }
}
