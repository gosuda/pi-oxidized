//! Stable remote service bindings and lifetime management.

mod facade;
mod lifecycle;

use std::collections::{BTreeMap, BTreeSet};
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
    revision: AtomicU64,
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
    revision: AtomicU64,
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

struct ObserverTask {
    address: ServiceInstanceAddress,
    cancel: CancellationToken,
    join: JoinHandle<()>,
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
        if binding.observer_count() == 0
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                let _ = binding.stop_if_empty(Context::background()).await;
            });
        }
    }

    /// Cancels this observer and waits until every callback task has settled.
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
        cancel_and_drain(tasks).await;
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
        if let Some(binding) = lock(&self.inner.singletons).get(service_id).cloned() {
            return Ok(Arc::clone(&binding.facade));
        }
        let facade = RemoteServiceFacade::new(
            service_id.clone(),
            None,
            Arc::clone(&self.inner.transport),
            Arc::clone(&self.inner.lifecycle),
        );
        let binding = Arc::new(SingletonBinding {
            service_id: service_id.clone(),
            facade: Arc::new(facade),
            active: AtomicBool::new(true),
            revision: AtomicU64::new(0),
            subscription: Mutex::new(None),
            starting: Mutex::new(None),
            start_cancel: Mutex::new(None),
        });
        let mut singletons = lock(&self.inner.singletons);
        if let Some(existing) = singletons.get(service_id).cloned() {
            return Ok(Arc::clone(&existing.facade));
        }
        singletons.insert(service_id.clone(), Arc::clone(&binding));
        drop(singletons);
        self.inner.readiness_revision.fetch_add(1, Ordering::AcqRel);
        if self.inner.lifecycle.is_bound() {
            self.spawn_singleton_start(&binding)?;
        }
        Ok(Arc::clone(&binding.facade))
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
                binding.wait_start().await?;
            }
            for binding in keyed_starts {
                binding.wait_start().await?;
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
                binding.wait_start().await?;
            }
            for binding in keyed {
                if binding.observer_count() > 0 {
                    binding.spawn_start()?;
                    binding.wait_start().await?;
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
        if binding.starting_is_set() {
            return Ok(());
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service binding requires a Tokio runtime"))?;
        let revision = binding.revision.load(Ordering::Acquire);
        let token = CancellationToken::new();
        *lock(&binding.start_cancel) = Some(token.clone());
        let transport = Arc::clone(&self.inner.transport);
        let lifecycle = Arc::clone(&self.inner.lifecycle);
        let task_binding = Arc::clone(binding);
        let task = handle.spawn(async move {
            start_singleton(task_binding, transport, lifecycle, revision, token).await
        });
        *lock(&binding.starting) = Some(task);
        Ok(())
    }
}

impl SingletonBinding {
    fn starting_is_set(&self) -> bool {
        lock(&self.starting).is_some()
    }

    async fn wait_start(&self) -> Result<(), ServiceError> {
        let task = lock(&self.starting).take();
        let Some(task) = task else {
            return Ok(());
        };
        match task.await {
            Ok(result) => result,
            Err(error) => Err(ServiceError::internal_with_source(
                "Remote service start task failed",
                error,
            )),
        }
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
                    self.facade.install(snapshot, context)
                }
            }
            ServiceProviderUpdate::State {
                instance: None,
                member,
                sequence,
                ops,
            } => self.facade.update(member, *sequence, ops, context),
            ServiceProviderUpdate::State {
                instance: Some(_), ..
            }
            | ServiceProviderUpdate::Spawned { .. }
            | ServiceProviderUpdate::Closed { .. } => Err(ServiceError::local(
                "Singleton received a keyed lifecycle update",
            )),
        };
        if let Err(error) = result {
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
    binding
        .facade
        .install(&snapshot.instances[0], &Context::background())?;
    *lock(&binding.subscription) = Some(Arc::clone(&subscription));
    subscription.activate();
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
            revision: AtomicU64::new(0),
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

    async fn stop_if_empty(&self, context: Context) -> Result<(), ServiceError> {
        if self.observer_count() != 0 {
            return Ok(());
        }
        self.reset(context, false).await
    }

    fn spawn_start(self: &Arc<Self>) -> Result<(), ServiceError> {
        if self.closed.load(Ordering::Acquire)
            || !self.lifecycle.is_bound()
            || self.starting_is_set()
        {
            return Ok(());
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ServiceError::local("Remote service binding requires a Tokio runtime"))?;
        let revision = self.revision.load(Ordering::Acquire);
        let token = CancellationToken::new();
        *lock(&self.start_cancel) = Some(token.clone());
        let binding = Arc::clone(self);
        let task = handle.spawn(async move { binding.start(revision, token).await });
        *lock(&self.starting) = Some(task);
        Ok(())
    }

    fn starting_is_set(&self) -> bool {
        lock(&self.starting).is_some()
    }

    async fn wait_start(&self) -> Result<(), ServiceError> {
        let task = lock(&self.starting).take();
        let Some(task) = task else {
            return Ok(());
        };
        match task.await {
            Ok(result) => result,
            Err(error) => Err(ServiceError::internal_with_source(
                "Remote keyed service start task failed",
                error,
            )),
        }
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
            self.spawn_instance(instance, &Context::background())?;
        }
        *lock(&self.subscription) = Some(Arc::clone(&subscription));
        subscription.activate();
        self.ready.store(true, Ordering::Release);
        let instances: Vec<Arc<KeyedInstance>> = lock(&self.instances).values().cloned().collect();
        for instance in instances {
            self.start_observer_tasks(&instance, &Context::background());
        }
        Ok(())
    }

    async fn reset(&self, context: Context, permanent: bool) -> Result<(), ServiceError> {
        self.ready.store(false, Ordering::Release);
        if permanent {
            self.closed.store(true, Ordering::Release);
        }
        let instance_tasks = self.deactivate_instances();
        cancel_and_drain(instance_tasks).await;
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
            cancel_and_drain(tasks).await;
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
        ));
        facade.install(snapshot, context)?;
        let instance = Arc::new(KeyedInstance {
            address: address.clone(),
            facade,
        });
        let previous = {
            let mut instances = lock(&self.instances);
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
            self.start_observer_tasks(&instance, context);
        }
        Ok(())
    }

    fn start_observer_tasks(&self, instance: &Arc<KeyedInstance>, context: &Context) {
        let observers: Vec<Arc<Observer>> = lock(&self.observers).values().cloned().collect();
        for observer in observers {
            if !observer.active.load(Ordering::Acquire) {
                continue;
            }
            let (task_context, cancel) = context.with_cancel();
            let handler = Arc::clone(&observer.handler);
            let facade = Arc::clone(&instance.facade);
            let lifecycle = Arc::clone(&self.lifecycle);
            let task_cancel = cancel.clone();
            let address = instance.address.clone();
            let join = tokio::spawn(async move {
                if let Err(error) = handler(facade, task_context).await
                    && !task_cancel.is_cancelled()
                    && !lifecycle.is_disposed()
                {
                    lifecycle.report(error);
                }
            });
            lock(&observer.tasks).push(ObserverTask {
                address,
                cancel,
                join,
            });
        }
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
            ServiceProviderUpdate::Spawned { instance } => self.spawn_instance(instance, context),
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
                        instance.facade.update(member, *sequence, ops, context)
                    }
                    _ => Ok(()),
                }
            }
            ServiceProviderUpdate::State { instance: None, .. } => Err(ServiceError::local(
                "Keyed state update has no instance address",
            )),
        };
        if let Err(error) = result {
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

async fn cancel_and_drain(tasks: Vec<ObserverTask>) {
    for task in tasks {
        task.cancel.cancel();
        let _ = task.join.await;
    }
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

    use super::*;
    use crate::service::provider::{
        RemoteServiceProvider, ServiceDefinition, ServiceImplementation, ServiceMember,
        ServiceMethod,
    };
    use crate::service::replicated::ReplicatedStateDeliveryKind;
    use crate::service::value::{JsObject, JsonValue};

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
}
