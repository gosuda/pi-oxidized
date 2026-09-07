//! Native remote-service provider and its loopback transport implementation.

use std::collections::BTreeMap;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;

use futures::future::BoxFuture;

use crate::context::Context;

use super::delta::DeltaOp;
use super::error::{RemoteServiceErrorCode, ServiceError};
use super::replicated::{MutableReplicatedState, ReplicatedSourceListener};
use super::transport::{RemoteServiceTransport, ServiceSubscription, ServiceUpdateListener};
use super::value::{is_json_value, JsInteger, JsString, JsonValue};
use super::wire::{
    ServiceCall, ServiceCatalogueEntry, ServiceInstanceAddress, ServiceInstanceSnapshot,
    ServiceMemberSnapshot, ServiceMode, ServiceProviderUpdate, ServiceSubscriptionSnapshot,
};

/// One published service definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDefinition {
    /// Stable service identifier.
    pub id: JsString,
    /// Process-local services cannot be published remotely.
    pub local: bool,
    /// Singleton or keyed lifecycle.
    pub mode: ServiceMode,
}

/// A callable native service member.
pub type ServiceMethod = Arc<
    dyn Fn(Vec<JsonValue>, Context) -> BoxFuture<'static, Result<Option<JsonValue>, ServiceError>>
        + Send
        + Sync,
>;

/// Members that may cross the remote service boundary.
#[derive(Clone)]
pub enum ServiceMember {
    /// A method receiving owned JSON arguments and an invocation context.
    Method(ServiceMethod),
    /// A mutable replicated state member.
    State(Arc<MutableReplicatedState>),
}

/// One complete implementation classified by member kind.
pub type ServiceImplementation = BTreeMap<JsString, ServiceMember>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceMemberKind {
    Method,
    State,
}

struct ProviderInstance {
    address: Option<ServiceInstanceAddress>,
    implementation: Arc<ServiceImplementation>,
    active: AtomicBool,
    remove_member_listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}
struct ProviderSubscriber {
    listener: ServiceUpdateListener,
    state: Mutex<SubscriberState>,
}

struct SubscriberState {
    buffer: Vec<(ServiceProviderUpdate<DeltaOp>, Context)>,
    active: bool,
    /// Thread that currently owns serial delivery, if any.  Identity only
    /// distinguishes same-thread reentrant publication; it never justifies a
    /// cross-thread wait.
    draining_thread: Option<thread::ThreadId>,
    terminated: bool,
    closed: bool,
}

struct ServiceRegistration {
    service_id: JsString,
    mode: ServiceMode,
    singleton: Option<Arc<ProviderInstance>>,
    singleton_shape: Option<BTreeMap<JsString, ServiceMemberKind>>,
    instances: BTreeMap<JsString, Arc<ProviderInstance>>,
    generations: BTreeMap<JsString, JsInteger>,
    subscribers: Vec<Arc<ProviderSubscriber>>,
}

struct ProviderInner {
    disposed: bool,
    registrations: BTreeMap<JsString, ServiceRegistration>,
}

/// A provider-owned close token for one keyed instance.
#[derive(Clone)]
pub struct ServiceInstanceHandle {
    provider: Weak<Mutex<ProviderInner>>,
    service_id: JsString,
    address: ServiceInstanceAddress,
    instance: Arc<ProviderInstance>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for ServiceInstanceHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceInstanceHandle")
            .field("address", &self.address)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ServiceInstanceHandle {
    /// Returns the immutable address captured at spawn time.
    #[must_use]
    pub const fn address(&self) -> &ServiceInstanceAddress {
        &self.address
    }

    /// Closes this generation once.  A stale token cannot close a replacement.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(provider) = self.provider.upgrade() else {
            return;
        };
        let instance = {
            let mut inner = lock(&provider);
            if inner.disposed {
                return;
            }
            let Some(registration) = inner.registrations.get_mut(&self.service_id) else {
                return;
            };
            let matches = registration
                .instances
                .get(&self.address.key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.instance));
            if !matches {
                return;
            }
            registration.instances.remove(&self.address.key)
        };
        if let Some(instance) = instance {
            deactivate_instance(&instance);
            emit_update_inner(
                &provider,
                &self.service_id,
                &ServiceProviderUpdate::Closed {
                    instance: self.address.clone(),
                },
                &Context::background(),
            );
        }
    }
}

/// Hosts allowlisted singleton and keyed service implementations.
pub struct RemoteServiceProvider {
    catalogue: Vec<ServiceCatalogueEntry>,
    inner: Arc<Mutex<ProviderInner>>,
}

impl std::fmt::Debug for RemoteServiceProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteServiceProvider")
            .field("catalogue", &self.catalogue)
            .field("disposed", &lock(&self.inner).disposed)
            .finish_non_exhaustive()
    }
}

impl RemoteServiceProvider {
    /// Creates a provider catalogue, preserving definition order.
    ///
    /// # Errors
    ///
    /// Returns an error when a definition is marked `local` or when the
    /// catalogue contains duplicate service identifiers.
    pub fn new(definitions: Vec<ServiceDefinition>) -> Result<Self, ServiceError> {
        let mut catalogue = Vec::with_capacity(definitions.len());
        let mut registrations = BTreeMap::new();
        for definition in definitions {
            if definition.local {
                return Err(ServiceError::local(format!(
                    "Local service {} cannot be published remotely",
                    display_js_string(&definition.id)
                )));
            }
            if registrations.contains_key(&definition.id) {
                return Err(ServiceError::local("Remote service catalogue contains duplicate IDs"));
            }
            catalogue.push(ServiceCatalogueEntry {
                service_id: definition.id.clone(),
                mode: definition.mode,
            });
            registrations.insert(
                definition.id.clone(),
                ServiceRegistration {
                    service_id: definition.id,
                    mode: definition.mode,
                    singleton: None,
                    singleton_shape: None,
                    instances: BTreeMap::new(),
                    generations: BTreeMap::new(),
                    subscribers: Vec::new(),
                },
            );
        }
        Ok(Self {
            catalogue,
            inner: Arc::new(Mutex::new(ProviderInner {
                disposed: false,
                registrations,
            })),
        })
    }

    /// Returns the immutable ordered catalogue.
    #[must_use]
    pub fn catalogue(&self) -> &[ServiceCatalogueEntry] {
        &self.catalogue
    }

    /// Publishes one singleton implementation.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is disposed, the service is not
    /// allowlisted or not singleton, a provider is already published, the
    /// implementation has no members or holds a non-JSON state value, or the
    /// member shape does not match the published facade.
    pub fn provide(&self, service_id: &JsString, implementation: ServiceImplementation) -> Result<(), ServiceError> {
        {
            let inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            if registration.singleton.is_some() {
                return Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceModeMismatch,
                    format!("Remote service {} already has a provider", display_js_string(service_id)),
                ));
            }
        }
        let classified = classify_implementation(service_id, implementation)?;
        let shape = member_shape(&classified);
        {
            let inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            if registration.singleton.is_some() {
                return Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceModeMismatch,
                    format!("Remote service {} already has a provider", display_js_string(service_id)),
                ));
            }
            assert_shape(registration, &shape)?;
        }
        let instance = self.create_instance(service_id, classified, None);
        let outcome: Result<(), ServiceError> = (|| {
            let mut inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration_mut(&mut inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            if registration.singleton.is_some() {
                Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceModeMismatch,
                    format!("Remote service {} already has a provider", display_js_string(service_id)),
                ))
            } else {
                assert_shape(registration, &shape).map(|()| {
                    registration.singleton = Some(Arc::clone(&instance));
                    registration.singleton_shape = Some(shape);
                })
            }
        })();
        if outcome.is_err() {
            deactivate_instance(&instance);
        }
        outcome
    }

    /// Withdraws one singleton while preserving subscriptions and shape.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is disposed or the service is not
    /// allowlisted or not singleton.
    pub fn withdraw(&self, service_id: &JsString) -> Result<(), ServiceError> {
        let previous = {
            let mut inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration_mut(&mut inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            registration.singleton.take()
        };
        if let Some(previous) = previous {
            deactivate_instance(&previous);
            emit_update_inner(
                &self.inner,
                service_id,
                &ServiceProviderUpdate::Unavailable,
                &Context::background(),
            );
        }
        Ok(())
    }

    /// Validates a singleton replacement without changing the provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is disposed, the service is not
    /// allowlisted or not singleton, the implementation has no members or
    /// holds a non-JSON state value, or the member shape does not match the
    /// published facade.
    pub fn validate_replacement(
        &self,
        service_id: &JsString,
        implementation: &ServiceImplementation,
    ) -> Result<(), ServiceError> {
        {
            let inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
        }
        let classified = classify_implementation(service_id, implementation.clone())?;
        let shape = member_shape(&classified);
        let inner = lock(&self.inner);
        assert_active(&inner)?;
        let registration = registration(&inner, service_id)?;
        require_mode(registration, ServiceMode::Singleton)?;
        assert_shape(registration, &shape)
    }

    /// Replaces one singleton while retaining its remote facade shape.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as
    /// [`Self::validate_replacement`].
    pub fn replace(&self, service_id: &JsString, implementation: ServiceImplementation) -> Result<(), ServiceError> {
        {
            let inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
        }
        let classified = classify_implementation(service_id, implementation)?;
        let shape = member_shape(&classified);
        {
            let inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            assert_shape(registration, &shape)?;
        }
        let replacement = self.create_instance(service_id, classified, None);
        let previous = {
            let mut inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration_mut(&mut inner, service_id)?;
            require_mode(registration, ServiceMode::Singleton)?;
            assert_shape(registration, &shape)?;
            let previous = registration.singleton.replace(Arc::clone(&replacement));
            registration.singleton_shape = Some(shape);
            previous
        };
        if let Some(previous) = previous {
            deactivate_instance(&previous);
        }
        let snapshot = snapshot_instance(&replacement);
        emit_update_inner(
            &self.inner,
            service_id,
            &ServiceProviderUpdate::Replaced { snapshot },
            &Context::background(),
        );
        Ok(())
    }

    /// Returns a clone of a locally published singleton implementation.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is disposed, the service is not
    /// allowlisted, or no singleton provider is published.
    pub fn use_service(&self, service_id: &JsString) -> Result<ServiceImplementation, ServiceError> {
        let inner = lock(&self.inner);
        assert_active(&inner)?;
        let registration = registration(&inner, service_id)?;
        if registration.mode != ServiceMode::Singleton {
            return Err(not_found(service_id));
        }
        let Some(singleton) = registration.singleton.as_ref() else {
            return Err(not_found(service_id));
        };
        Ok(singleton.implementation.as_ref().clone())
    }

    /// Rust spelling of the source `use` operation.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`Self::use_service`].
    pub fn use_(&self, service_id: &JsString) -> Result<ServiceImplementation, ServiceError> {
        self.use_service(service_id)
    }

    /// Spawns one keyed generation and returns its idempotent close token.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is disposed, the service is not
    /// allowlisted or not keyed, the key is empty or already live, or the
    /// implementation has no members or holds a non-JSON state value.
    pub fn spawn(
        &self,
        service_id: &JsString,
        key: JsString,
        implementation: ServiceImplementation,
    ) -> Result<ServiceInstanceHandle, ServiceError> {
        let generation = {
            let mut inner = lock(&self.inner);
            assert_active(&inner)?;
            let registration = registration(&inner, service_id)?;
            if key.as_utf16().is_empty() {
                return Err(ServiceError::local("Remote service instance key must not be empty"));
            }
            require_mode(registration, ServiceMode::Keyed)?;
            if registration.instances.contains_key(&key) {
                return Err(instance_exists(service_id, &key));
            }
            let generation = registration
                .generations
                .get(&key)
                .map_or(JsInteger::one(), JsInteger::next);
            // Reserve the generation before constructing listeners.  A
            // concurrent spawn can consume a later generation, but can never
            // make this one reusable after a close.
            registration_mut(&mut inner, service_id)?
                .generations
                .insert(key.clone(), generation);
            generation
        };
        let classified = classify_implementation(service_id, implementation)?;
        let address = ServiceInstanceAddress {
            key: key.clone(),
            generation,
        };
        let instance = self.create_instance(service_id, classified, Some(address.clone()));
        let result: Result<_, ServiceError> = {
            let mut inner = lock(&self.inner);
            (|| {
                assert_active(&inner)?;
                let registration = registration_mut(&mut inner, service_id)?;
                require_mode(registration, ServiceMode::Keyed)?;
                if registration.instances.contains_key(&key) {
                    return Err(instance_exists(service_id, &key));
                }
                registration.instances.insert(key, Arc::clone(&instance));
                Ok(snapshot_instance(&instance))
            })()
        };
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                deactivate_instance(&instance);
                return Err(error);
            }
        };
        emit_update_inner(
            &self.inner,
            service_id,
            &ServiceProviderUpdate::Spawned { instance: snapshot },
            &Context::background(),
        );
        Ok(ServiceInstanceHandle {
            provider: Arc::downgrade(&self.inner),
            service_id: service_id.clone(),
            address,
            instance,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Invokes a method through the provider's loopback boundary.
    #[must_use]
    pub fn invoke(
        &self,
        call: ServiceCall,
        context: Context,
    ) -> BoxFuture<'static, Result<Option<JsonValue>, ServiceError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { invoke_inner(&inner, call, context).await })
    }

    /// Opens a buffered subscription through the provider's loopback boundary.
    pub fn subscribe(
        &self,
        service_id: JsString,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        _context: Context,
    ) -> BoxFuture<'static, Result<Arc<dyn ServiceSubscription>, ServiceError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { subscribe_inner(&inner, service_id, mode, listener) })
    }

    /// Disposes this provider and emits unavailable/closed updates once.
    pub fn dispose(&self) {
        let mut events = Vec::new();
        {
            let mut inner = lock(&self.inner);
            if inner.disposed {
                return;
            }
            inner.disposed = true;
            for registration in inner.registrations.values_mut() {
                if let Some(instance) = registration.singleton.take() {
                    events.push((
                        registration.service_id.clone(),
                        instance,
                        ServiceProviderUpdate::Unavailable,
                    ));
                }
                let instances: Vec<_> = registration.instances.values().cloned().collect();
                registration.instances.clear();
                for instance in instances {
                    let Some(address) = instance.address.clone() else {
                        continue;
                    };
                    events.push((
                        registration.service_id.clone(),
                        instance,
                        ServiceProviderUpdate::Closed { instance: address },
                    ));
                }
            }
        }
        let mut panic_payload = None;
        for (service_id, instance, update) in events {
            deactivate_instance(&instance);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                emit_update_inner(&self.inner, &service_id, &update, &Context::background());
            }));
            if let Err(payload) = result
                && panic_payload.is_none()
            {
                panic_payload = Some(payload);
            }
        }
        let mut inner = lock(&self.inner);
        for registration in inner.registrations.values_mut() {
            for subscriber in &registration.subscribers {
                let mut state = lock(&subscriber.state);
                if state.active {
                    state.closed = true;
                    state.buffer.clear();
                } else {
                    state.terminated = true;
                }
                drop(state);
            }
            registration.subscribers.clear();
        }
        inner.registrations.clear();
        drop(inner);
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }

    }
    fn create_instance(
        &self,
        service_id: &JsString,
        implementation: ServiceImplementation,
        address: Option<ServiceInstanceAddress>,
    ) -> Arc<ProviderInstance> {
        let instance = Arc::new(ProviderInstance {
            address,
            implementation: Arc::new(implementation),
            active: AtomicBool::new(true),
            remove_member_listeners: Mutex::new(Vec::new()),
        });
        for (member_name, member) in instance.implementation.iter() {
            let weak_provider = Arc::downgrade(&self.inner);
            let ServiceMember::State(state) = member else {
                continue;
            };
            let service_id = service_id.clone();
            let member_name = member_name.clone();
            let address = instance.address.clone();
            let weak_instance = Arc::downgrade(&instance);
            let listener: ReplicatedSourceListener = Arc::new(move |ops, sequence, context| {
                let Some(instance) = weak_instance.upgrade() else {
                    return;
                };
                if !instance.active.load(Ordering::Acquire) {
                    return;
                }
                let Some(provider) = weak_provider.upgrade() else {
                    return;
                };
                emit_update_inner(
                    &provider,
                    &service_id,
                    &ServiceProviderUpdate::State {
                        instance: address.clone(),
                        member: member_name.clone(),
                        sequence,
                        ops: ops.to_vec(),
                    },
                    &context,
                );
            });
            let remove = state.subscribe_source(listener);
            lock(&instance.remove_member_listeners).push(remove);
        }
        instance
    }
}

impl RemoteServiceTransport for RemoteServiceProvider {
    fn invoke(
        &self,
        call: ServiceCall,
        cx: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, ServiceError>> {
        RemoteServiceProvider::invoke(self, call, cx)
    }

    fn subscribe(
        &self,
        service_id: JsString,
        mode: ServiceMode,
        listener: ServiceUpdateListener,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn ServiceSubscription>, ServiceError>> {
        RemoteServiceProvider::subscribe(self, service_id, mode, listener, cx)
    }
}

struct ProviderSubscription {
    provider: Weak<Mutex<ProviderInner>>,
    service_id: JsString,
    subscriber: Arc<ProviderSubscriber>,
    snapshot: ServiceSubscriptionSnapshot<DeltaOp>,
}

impl ServiceSubscription for ProviderSubscription {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot<DeltaOp> {
        &self.snapshot
    }

    fn activate(&self) {
        let current_thread = thread::current().id();
        {
            let mut state = lock(&self.subscriber.state);
            if state.closed || state.active {
                return;
            }
            state.active = true;
            state.draining_thread = Some(current_thread);
        }
        let panic_payload = drain_subscriber(&self.subscriber);
        if lock(&self.subscriber.state).closed {
            self.remove_from_provider();
        }
        if let Some(payload) = panic_payload {
            std::panic::resume_unwind(payload);
        }
    }

    fn close(&self, _cx: Context) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            {
                let mut state = lock(&self.subscriber.state);
                if state.closed {
                    return Ok(());
                }
                state.closed = true;
                state.buffer.clear();
            }
            self.remove_from_provider();
            Ok(())
        })
    }
}

impl ProviderSubscription {
    fn remove_from_provider(&self) {
        let Some(provider) = self.provider.upgrade() else {
            return;
        };
        let mut inner = lock(&provider);
        let Some(registration) = inner.registrations.get_mut(&self.service_id) else {
            return;
        };
        registration
            .subscribers
            .retain(|subscriber| !Arc::ptr_eq(subscriber, &self.subscriber));
    }
}

async fn invoke_inner(
    provider: &Arc<Mutex<ProviderInner>>,
    call: ServiceCall,
    context: Context,
) -> Result<Option<JsonValue>, ServiceError> {
    let method = {
        let inner = lock(provider);
        assert_active(&inner)?;
        let registration = registration(&inner, &call.service_id)?;
        let instance = resolve_instance(registration, call.instance.as_ref())?;
        let Some(ServiceMember::Method(method)) = instance.implementation.get(&call.member) else {
            if instance.implementation.contains_key(&call.member) {
                return Err(ServiceError::remote(
                    RemoteServiceErrorCode::ServiceMemberMismatch,
                    format!(
                        "Remote service member {}.{} is not a method",
                        display_js_string(&call.service_id),
                        display_js_string(&call.member)
                    ),
                ));
            }
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceMemberNotFound,
                format!(
                    "Unknown remote service member {}.{}",
                    display_js_string(&call.service_id),
                    display_js_string(&call.member)
                ),
            ));
        };
        Arc::clone(method)
    };
    if call.args.iter().any(|value| !is_json_value(value)) {
        return Err(ServiceError::remote(
            RemoteServiceErrorCode::ServiceInvalidValue,
            "Remote service invocation arguments must be strict JSON",
        ));
    }
    let result = method(call.args, context).await?;
    if result.as_ref().is_some_and(|value| !is_json_value(value)) {
        return Err(ServiceError::remote(
            RemoteServiceErrorCode::ServiceInvalidValue,
            "Remote service method result must be strict JSON",
        ));
    }
    Ok(result)
}

fn subscribe_inner(
    provider: &Arc<Mutex<ProviderInner>>,
    service_id: JsString,
    mode: ServiceMode,
    listener: ServiceUpdateListener,
) -> Result<Arc<dyn ServiceSubscription>, ServiceError> {
    let states = {
        let inner = lock(provider);
        assert_active(&inner)?;
        let registration = registration(&inner, &service_id)?;
        require_mode(registration, mode)?;
        if mode == ServiceMode::Singleton && registration.singleton.is_none() {
            return Err(not_found(&service_id));
        }
        let mut states = Vec::new();
        let instances = if mode == ServiceMode::Singleton {
            registration.singleton.iter().cloned().collect::<Vec<_>>()
        } else {
            registration.instances.values().cloned().collect::<Vec<_>>()
        };
        for instance in instances {
            for member in instance.implementation.values() {
                if let ServiceMember::State(state) = member {
                    states.push(Arc::clone(state));
                }
            }
        }
        states
    };
    for state in states {
        state.publish(Context::background())?;
    }
    let (subscriber, snapshot) = {
        let mut inner = lock(provider);
        assert_active(&inner)?;
        let registration = registration_mut(&mut inner, &service_id)?;
        require_mode(registration, mode)?;
        if mode == ServiceMode::Singleton && registration.singleton.is_none() {
            return Err(not_found(&service_id));
        }
        let subscriber = Arc::new(ProviderSubscriber {
            listener,
            state: Mutex::new(SubscriberState {
                buffer: Vec::new(),
                active: false,
                draining_thread: None,
                terminated: false,
                closed: false,
            }),
        });
        let snapshot = snapshot_registration(registration);
        registration.subscribers.push(Arc::clone(&subscriber));
        (subscriber, snapshot)
    };
    Ok(Arc::new(ProviderSubscription {
        provider: Arc::downgrade(provider),
        service_id,
        subscriber,
        snapshot,
    }))
}

fn emit_update_inner(
    provider: &Arc<Mutex<ProviderInner>>,
    service_id: &JsString,
    update: &ServiceProviderUpdate<DeltaOp>,
    context: &Context,
) {
    /// One subscriber's disposition for this publication.
    enum Delivery {
        /// Same-thread reentrant publication, delivered immediately and never
        /// queued behind the outer drain, matching the source.
        Immediate(Arc<ProviderSubscriber>, ServiceProviderUpdate<DeltaOp>, Context),
        /// Freshly claimed serial ownership, drained after internal locks are
        /// released so no lock is held through the listener call.
        Claim(Arc<ProviderSubscriber>),
    }
    let current_thread = thread::current().id();
    let deliveries = {
        let inner = lock(provider);
        let Some(registration) = inner.registrations.get(service_id) else {
            return;
        };
        let mut deliveries = Vec::new();
        for subscriber in &registration.subscribers {
            let mut state = lock(&subscriber.state);
            if state.closed {
                continue;
            }
            let owner = state.draining_thread;
            if !state.active || owner.is_some_and(|thread| thread != current_thread) {
                // Preactivation entries and other-thread publications queue in
                // order for the current or next delivery owner.  The emitting
                // thread never waits for delivery completion.
                state.buffer.push((update.clone(), context.clone()));
            } else if owner == Some(current_thread) {
                deliveries.push(Delivery::Immediate(
                    Arc::clone(subscriber),
                    update.clone(),
                    context.clone(),
                ));
            } else {
                state.draining_thread = Some(current_thread);
                state.buffer.push((update.clone(), context.clone()));
                deliveries.push(Delivery::Claim(Arc::clone(subscriber)));
            }
        }
        deliveries
    };
    let mut panic_payload = None;
    for delivery in deliveries {
        let payload = match delivery {
            Delivery::Immediate(subscriber, update, context) => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (subscriber.listener)(&update, &context);
                }))
                .err()
            }
            Delivery::Claim(subscriber) => drain_subscriber(&subscriber),
        };
        if payload.is_some() && panic_payload.is_none() {
            panic_payload = payload;
        }
    }
    if let Some(payload) = panic_payload {
        std::panic::resume_unwind(payload);
    }
}

/// Drains one subscriber's buffer in order as the sole delivery owner.  The
/// owner is released before returning, including after a listener panic, and
/// entries queued by other threads while draining are picked up in order.
fn drain_subscriber(
    subscriber: &Arc<ProviderSubscriber>,
) -> Option<Box<dyn std::any::Any + Send>> {
    let mut panic_payload = None;
    loop {
        let entries = {
            let mut state = lock(&subscriber.state);
            if state.buffer.is_empty() {
                state.draining_thread = None;
                if state.terminated {
                    state.closed = true;
                }
                return panic_payload;
            }
            mem::take(&mut state.buffer)
        };
        for (update, context) in entries {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (subscriber.listener)(&update, &context);
            }));
            if let Err(payload) = result
                && panic_payload.is_none()
            {
                panic_payload = Some(payload);
            }
        }
    }
}

fn snapshot_registration(registration: &ServiceRegistration) -> ServiceSubscriptionSnapshot<DeltaOp> {
    let instances = if registration.mode == ServiceMode::Singleton {
        registration
            .singleton
            .as_ref()
            .map_or_else(Vec::new, |instance| vec![snapshot_instance(instance)])
    } else {
        registration
            .instances
            .values()
            .map(|instance| snapshot_instance(instance))
            .collect()
    };
    ServiceSubscriptionSnapshot {
        service_id: registration.service_id.clone(),
        mode: registration.mode,
        instances,
    }
}

fn snapshot_instance(instance: &ProviderInstance) -> ServiceInstanceSnapshot<DeltaOp> {
    let members = instance
        .implementation
        .iter()
        .map(|(name, member)| match member {
            ServiceMember::Method(_) => ServiceMemberSnapshot::Method { name: name.clone() },
            ServiceMember::State(state) => ServiceMemberSnapshot::State {
                name: name.clone(),
                sequence: state.sequence(),
                ops: vec![DeltaOp::Replace((*state.value()).clone())],
            },
        })
        .collect();
    ServiceInstanceSnapshot {
        instance: instance.address.clone(),
        members,
    }
}

fn classify_implementation(
    service_id: &JsString,
    implementation: ServiceImplementation,
) -> Result<ServiceImplementation, ServiceError> {
    if implementation.is_empty() {
        return Err(ServiceError::local(format!(
            "Remote service {} has no members",
            display_js_string(service_id)
        )));
    }
    for member in implementation.values() {
        if let ServiceMember::State(state) = member
            && !state.is_strict_json()
        {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceInvalidValue,
                format!("Remote service {} state value must be strict JSON", display_js_string(service_id)),
            ));
        }
    }
    Ok(implementation)
}

fn member_shape(implementation: &ServiceImplementation) -> BTreeMap<JsString, ServiceMemberKind> {
    implementation
        .iter()
        .map(|(name, member)| {
            (
                name.clone(),
                match member {
                    ServiceMember::Method(_) => ServiceMemberKind::Method,
                    ServiceMember::State(_) => ServiceMemberKind::State,
                },
            )
        })
        .collect()
}

fn assert_shape(
    registration: &ServiceRegistration,
    replacement: &BTreeMap<JsString, ServiceMemberKind>,
) -> Result<(), ServiceError> {
    let Some(current) = registration.singleton_shape.as_ref() else {
        return Ok(());
    };
    if current == replacement {
        return Ok(());
    }
    Err(ServiceError::remote(
        RemoteServiceErrorCode::ServiceMemberMismatch,
        format!(
            "Remote service {} replacement must preserve its member shape",
            display_js_string(&registration.service_id)
        ),
    ))
}

fn resolve_instance<'a>(
    registration: &'a ServiceRegistration,
    address: Option<&ServiceInstanceAddress>,
) -> Result<&'a Arc<ProviderInstance>, ServiceError> {
    if registration.mode == ServiceMode::Singleton {
        if address.is_some() {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceModeMismatch,
                format!("Remote service {} is singleton", display_js_string(&registration.service_id)),
            ));
        }
        return registration
            .singleton
            .as_ref()
            .ok_or_else(|| not_found(&registration.service_id));
    }
    let Some(address) = address else {
        return Err(ServiceError::remote(
            RemoteServiceErrorCode::ServiceModeMismatch,
            format!("Remote service {} is keyed", display_js_string(&registration.service_id)),
        ));
    };
    let Some(instance) = registration.instances.get(&address.key) else {
        return Err(ServiceError::remote(
            RemoteServiceErrorCode::ServiceInstanceNotFound,
            format!(
                "Remote service {} has no instance {}",
                display_js_string(&registration.service_id),
                display_js_string(&address.key)
            ),
        ));
    };
    if instance.address.as_ref().is_none_or(|current| current.generation != address.generation) {
        return Err(ServiceError::remote(
            RemoteServiceErrorCode::ServiceStaleInstance,
            format!(
                "Remote service {} instance {} is stale",
                display_js_string(&registration.service_id),
                display_js_string(&address.key)
            ),
        ));
    }
    Ok(instance)
}

fn registration<'a>(
    inner: &'a ProviderInner,
    service_id: &JsString,
) -> Result<&'a ServiceRegistration, ServiceError> {
    inner
        .registrations
        .get(service_id)
        .ok_or_else(|| ServiceError::remote(
            RemoteServiceErrorCode::ServiceNotAllowed,
            format!("Remote service {} is not allowlisted", display_js_string(service_id)),
        ))
}

fn registration_mut<'a>(
    inner: &'a mut ProviderInner,
    service_id: &JsString,
) -> Result<&'a mut ServiceRegistration, ServiceError> {
    inner
        .registrations
        .get_mut(service_id)
        .ok_or_else(|| ServiceError::remote(
            RemoteServiceErrorCode::ServiceNotAllowed,
            format!("Remote service {} is not allowlisted", display_js_string(service_id)),
        ))
}

fn require_mode(registration: &ServiceRegistration, expected: ServiceMode) -> Result<(), ServiceError> {
    if registration.mode == expected {
        return Ok(());
    }
    Err(ServiceError::remote(
        RemoteServiceErrorCode::ServiceModeMismatch,
        format!(
            "Remote service {} is {}, not {}",
            display_js_string(&registration.service_id),
            registration.mode.as_str(),
            expected.as_str()
        ),
    ))
}

fn assert_active(inner: &ProviderInner) -> Result<(), ServiceError> {
    if inner.disposed {
        return Err(ServiceError::disposed("Remote service provider is disposed"));
    }
    Ok(())
}

fn not_found(service_id: &JsString) -> ServiceError {
    ServiceError::remote(
        RemoteServiceErrorCode::ServiceNotFound,
        format!("Remote service {} has no local provider", display_js_string(service_id)),
    )
}

fn instance_exists(service_id: &JsString, key: &JsString) -> ServiceError {
    ServiceError::remote(
        RemoteServiceErrorCode::ServiceModeMismatch,
        format!(
            "Remote service {} already has a live instance with key {}",
            display_js_string(service_id),
            display_js_string(key)
        ),
    )
}

fn deactivate_instance(instance: &Arc<ProviderInstance>) {
    if !instance.active.swap(false, Ordering::AcqRel) {
        return;
    }
    let removers = mem::take(&mut *lock(&instance.remove_member_listeners));
    for remove in removers {
        remove();
    }
}

fn display_js_string(value: &JsString) -> String {
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
    reason = "test fixtures and bounded waits use contextual failure messages"
)]
#[expect(clippy::panic, reason = "test failure paths panic with context")]
mod tests {
    //! Focused regressions for serial nonblocking delivery ownership.

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Upper bound for every cross-thread wait.  Bounded waits fail the test
    /// deterministically instead of hanging; every joinable worker is joined
    /// before the failing assertion.
    const BOUND: Duration = Duration::from_secs(10);

    struct Record {
        entries: Mutex<Vec<(String, Option<JsInteger>)>>,
        threads: Mutex<Vec<thread::ThreadId>>,
    }
    impl Record {
        fn new() -> Self {
            Self {
                entries: Mutex::new(Vec::new()),
                threads: Mutex::new(Vec::new()),
            }
        }

        fn push(&self, label: &str, sequence: Option<JsInteger>) {
            lock(&self.entries).push((String::from(label), sequence));
            lock(&self.threads).push(thread::current().id());
        }

        fn entries(&self) -> Vec<(String, Option<JsInteger>)> {
            lock(&self.entries).clone()
        }

        fn labels(&self) -> Vec<String> {
            lock(&self.entries)
                .iter()
                .map(|(label, _)| label.clone())
                .collect()
        }

        fn on_one_thread(&self) -> bool {
            let threads = lock(&self.threads);
            threads.iter().all(|thread| Some(thread) == threads.first())
        }
    }
    fn labels(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| String::from(*value)).collect()
    }
    fn integer(value: f64) -> JsInteger {
        JsInteger::new(value).expect("nonnegative integer")
    }

    fn definition(id: &str, mode: ServiceMode) -> ServiceDefinition {
        ServiceDefinition {
            id: JsString::from_utf8(id),
            local: false,
            mode,
        }
    }

    fn provider(definitions: Vec<ServiceDefinition>) -> Arc<RemoteServiceProvider> {
        Arc::new(RemoteServiceProvider::new(definitions).expect("catalogue"))
    }

    fn object(count: f64) -> JsonValue {
        let mut fields = BTreeMap::new();
        fields.insert(
            JsString::from_utf8("count"),
            JsonValue::Number(count),
        );
        JsonValue::Object(fields)
    }

    fn state_members(state: &Arc<MutableReplicatedState>) -> ServiceImplementation {
        let mut members = BTreeMap::new();
        members.insert(
            JsString::from_utf8("state"),
            ServiceMember::State(Arc::clone(state)),
        );
        members
    }

    fn members_with_method(state: &Arc<MutableReplicatedState>) -> ServiceImplementation {
        let mut members = state_members(state);
        let published = Arc::clone(state);
        let bump: ServiceMethod = Arc::new(move |_args: Vec<JsonValue>, _context: Context| {
            let state = Arc::clone(&published);
            Box::pin(async move {
                state.with_state_mut(|value| *value = object(2.0));
                state.publish(Context::background())?;
                Ok(Some(JsonValue::Null))
            })
        });
        members.insert(JsString::from_utf8("bump"), ServiceMember::Method(bump));
        members
    }

    fn publish(state: &MutableReplicatedState, count: f64) {
        state.with_state_mut(|value| *value = object(count));
        state.publish(Context::background()).expect("state publish");
    }

    fn bump_call() -> ServiceCall {
        ServiceCall {
            service_id: JsString::from_utf8("svc"),
            instance: None,
            member: JsString::from_utf8("bump"),
            args: Vec::new(),
        }
    }

    fn recording_listener(
        record: Arc<Record>,
        on_state: Arc<dyn Fn(JsInteger) + Send + Sync>,
    ) -> ServiceUpdateListener {
        Arc::new(move |update: &ServiceProviderUpdate<DeltaOp>, _context: &Context| {
            match update {
                ServiceProviderUpdate::State { sequence, .. } => {
                    record.push("enter", Some(*sequence));
                    on_state(*sequence);
                    record.push("exit", Some(*sequence));
                }
                ServiceProviderUpdate::Unavailable => record.push("unavailable", None),
                ServiceProviderUpdate::Replaced { .. } => record.push("replaced", None),
                ServiceProviderUpdate::Spawned { .. } => record.push("spawned", None),
                ServiceProviderUpdate::Closed { .. } => record.push("closed", None),
            }
        })
    }

    /// Shared slot for a worker handle installed once by the listener hook.
    type SharedSlot<T> = Arc<Mutex<Option<T>>>;

    /// Runs `operation` on a nested worker under the delivery deadline and
    /// reports `Some(outcome)` once it finishes.  On deadline the report is
    /// `None`, and the worker keeps waiting briefly so a late finisher is
    /// still joined before the worker exits.
    fn supervised_call<T, F>(operation: F) -> (thread::JoinHandle<()>, mpsc::Receiver<Option<T>>)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (report_tx, report_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let completed = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&completed);
            let mut inner = Some(thread::spawn(move || {
                let outcome = operation();
                flag.store(true, Ordering::Release);
                outcome
            }));
            let deadline = Instant::now() + BOUND;
            while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            let finished = completed.load(Ordering::Acquire);
            let report = if finished {
                Some(
                    inner
                        .take()
                        .expect("supervised worker handle")
                        .join()
                        .expect("supervised worker panicked"),
                )
            } else {
                None
            };
            report_tx.send(report).ok();
            if !finished {
                let release_deadline = Instant::now() + BOUND;
                while !completed.load(Ordering::Acquire) && Instant::now() < release_deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                if completed.load(Ordering::Acquire) {
                    inner
                        .take()
                        .expect("supervised worker handle")
                        .join()
                        .expect("supervised worker panicked");
                }
            }
        });
        (worker, report_rx)
    }

    /// Joins the worker stored in `slot` when the callback left one running.
    /// `name` identifies the worker in the panic message when it failed.
    fn join_worker(slot: &Mutex<Option<thread::JoinHandle<()>>>, name: &str) {
        if let Some(worker) = lock(slot).take() {
            worker.join().unwrap_or_else(|_| panic!("{name} panicked"));
        }
    }

    /// Invokes `state` on one keyed generation and returns the outcome.
    async fn invoke_state(
        provider: &RemoteServiceProvider,
        room: &JsString,
        key: &JsString,
        generation: JsInteger,
    ) -> Result<Option<JsonValue>, ServiceError> {
        provider
            .invoke(
                ServiceCall {
                    service_id: room.clone(),
                    instance: Some(ServiceInstanceAddress {
                        key: key.clone(),
                        generation,
                    }),
                    member: JsString::from_utf8("state"),
                    args: Vec::new(),
                },
                Context::background(),
            )
            .await
    }

    #[tokio::test]
    async fn preactivation_buffer_delivers_in_publication_order() {
        let state = MutableReplicatedState::new(object(0.0));
        let svc = JsString::from_utf8("svc");
        let provider = provider(vec![definition("svc", ServiceMode::Singleton)]);
        provider.provide(&svc, state_members(&state)).expect("provide");
        let record = Arc::new(Record::new());
        let listener = recording_listener(Arc::clone(&record), Arc::new(|_: JsInteger| {}));
        let subscription = provider
            .subscribe(svc, ServiceMode::Singleton, listener, Context::background())
            .await
            .expect("subscribe");
        assert!(record.entries().is_empty(), "nothing delivers before activation");
        publish(&state, 1.0);
        publish(&state, 2.0);
        assert!(record.entries().is_empty(), "preactivation updates stay buffered");
        subscription.activate();
        assert_eq!(
            record.entries(),
            vec![
                (String::from("enter"), Some(integer(1.0))),
                (String::from("exit"), Some(integer(1.0))),
                (String::from("enter"), Some(integer(2.0))),
                (String::from("exit"), Some(integer(2.0))),
            ],
            "buffered updates deliver in publication order"
        );
        subscription.close(Context::background()).await.expect("close");
        provider.dispose();
    }

    #[tokio::test]
    async fn source_reentrant_publication_order_is_preserved() {
        let state = MutableReplicatedState::new(object(0.0));
        let svc = JsString::from_utf8("svc");
        let provider = provider(vec![definition("svc", ServiceMode::Singleton)]);
        provider.provide(&svc, state_members(&state)).expect("provide");
        let events = Arc::new(Mutex::new(Vec::<(String, JsonValue, JsInteger)>::new()));
        let nested = Arc::new(AtomicBool::new(false));
        let listener_a: ServiceUpdateListener = {
            let events = Arc::clone(&events);
            let state = Arc::clone(&state);
            let nested = Arc::clone(&nested);
            Arc::new(move |update, _context| {
                let ServiceProviderUpdate::State { sequence, .. } = update else {
                    return;
                };
                lock(&events).push((
                    String::from("A"),
                    state.value().as_ref().clone(),
                    *sequence,
                ));
                if !nested.swap(true, Ordering::AcqRel) {
                    publish(&state, 2.0);
                }
            })
        };
        let listener_b: ServiceUpdateListener = {
            let events = Arc::clone(&events);
            let state = Arc::clone(&state);
            Arc::new(move |update, _context| {
                let ServiceProviderUpdate::State { sequence, .. } = update else {
                    return;
                };
                lock(&events).push((
                    String::from("B"),
                    state.value().as_ref().clone(),
                    *sequence,
                ));
            })
        };
        let subscription_a = provider
            .subscribe(
                svc.clone(),
                ServiceMode::Singleton,
                listener_a,
                Context::background(),
            )
            .await
            .expect("subscribe A");
        let subscription_b = provider
            .subscribe(svc, ServiceMode::Singleton, listener_b, Context::background())
            .await
            .expect("subscribe B");
        subscription_a.activate();
        subscription_b.activate();
        publish(&state, 1.0);
        assert_eq!(
            lock(&events).clone(),
            vec![
                (String::from("A"), object(1.0), integer(1.0)),
                (String::from("A"), object(2.0), integer(2.0)),
                (String::from("B"), object(2.0), integer(2.0)),
                (String::from("B"), object(2.0), integer(1.0)),
            ],
            "same-thread reentry retains the source's nested listener order"
        );
        subscription_a
            .close(Context::background())
            .await
            .expect("close A");
        subscription_b
            .close(Context::background())
            .await
            .expect("close B");
        provider.dispose();
    }

    #[tokio::test]
    async fn other_thread_publication_queues_without_waiting() {
        let state = MutableReplicatedState::new(object(0.0));
        let svc = JsString::from_utf8("svc");
        let provider = provider(vec![definition("svc", ServiceMode::Singleton)]);
        provider.provide(&svc, state_members(&state)).expect("provide");
        let record = Arc::new(Record::new());
        let spawns = Arc::new(AtomicUsize::new(0));
        let worker_slot: SharedSlot<thread::JoinHandle<()>> = Arc::new(Mutex::new(None));
        let timed_out = Arc::new(AtomicBool::new(false));
        let hook = {
            let spawns = Arc::clone(&spawns);
            let provider = Arc::clone(&provider);
            let service_id = svc.clone();
            let worker_slot = Arc::clone(&worker_slot);
            let timed_out = Arc::clone(&timed_out);
            Arc::new(move |_sequence: JsInteger| {
                if spawns.fetch_add(1, Ordering::AcqRel) != 0 {
                    return;
                }
                let provider = Arc::clone(&provider);
                let service_id = service_id.clone();
                let (worker, report) = supervised_call(move || {
                    provider.withdraw(&service_id).expect("worker withdraw");
                });
                lock(&worker_slot).replace(worker);
                let Ok(published) = report.recv_timeout(BOUND) else {
                    timed_out.store(true, Ordering::Release);
                    return;
                };
                if published.is_none() {
                    timed_out.store(true, Ordering::Release);
                    return;
                }
                join_worker(&worker_slot, "publishing worker");
            })
        };
        let listener = recording_listener(Arc::clone(&record), hook);
        let subscription = provider
            .subscribe(svc, ServiceMode::Singleton, listener, Context::background())
            .await
            .expect("subscribe");
        subscription.activate();
        publish(&state, 1.0);
        join_worker(&worker_slot, "publishing worker");
        assert!(
            !timed_out.load(Ordering::Acquire),
            "other-thread provider update did not return during the callback"
        );
        assert_eq!(
            record.entries(),
            vec![
                (String::from("enter"), Some(integer(1.0))),
                (String::from("exit"), Some(integer(1.0))),
                (String::from("unavailable"), None),
            ],
            "queued other-thread update delivers in order after the outer drain"
        );
        assert!(record.on_one_thread(), "the owner thread delivers queued updates");
        subscription.close(Context::background()).await.expect("close");
        provider.dispose();
    }

    #[tokio::test]
    async fn callback_worker_invoke_cycle_completes_without_deadlock() {
        let state = MutableReplicatedState::new(object(0.0));
        let svc = JsString::from_utf8("svc");
        let provider = provider(vec![definition("svc", ServiceMode::Singleton)]);
        provider
            .provide(&svc, members_with_method(&state))
            .expect("provide");
        let record = Arc::new(Record::new());
        let spawns = Arc::new(AtomicUsize::new(0));
        let worker_slot: SharedSlot<thread::JoinHandle<()>> = Arc::new(Mutex::new(None));
        let timed_out = Arc::new(AtomicBool::new(false));
        let cycle_result = Arc::new(Mutex::new(None::<(bool, bool)>));
        let hook = {
            let spawns = Arc::clone(&spawns);
            let provider = Arc::clone(&provider);
            let record = Arc::clone(&record);
            let worker_slot = Arc::clone(&worker_slot);
            let timed_out = Arc::clone(&timed_out);
            let cycle_result = Arc::clone(&cycle_result);
            Arc::new(move |_sequence: JsInteger| {
                if spawns.fetch_add(1, Ordering::AcqRel) != 0 {
                    return;
                }
                let provider = Arc::clone(&provider);
                let (worker, report) = supervised_call(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("invoke runtime");
                    runtime
                        .block_on(provider.invoke(bump_call(), Context::background()))
                        .is_ok()
                });
                lock(&worker_slot).replace(worker);
                let Ok(outcome) = report.recv_timeout(BOUND) else {
                    timed_out.store(true, Ordering::Release);
                    return;
                };
                let result = (outcome.is_some(), outcome.unwrap_or(false));
                *lock(&cycle_result) = Some(result);
                if result != (true, true) {
                    timed_out.store(true, Ordering::Release);
                    return;
                }
                join_worker(&worker_slot, "cycle worker");
                record.push("joined", None);
            })
        };
        let listener = recording_listener(Arc::clone(&record), hook);
        let subscription = provider
            .subscribe(svc, ServiceMode::Singleton, listener, Context::background())
            .await
            .expect("subscribe");
        subscription.activate();
        publish(&state, 1.0);
        join_worker(&worker_slot, "cycle worker");
        assert!(
            !timed_out.load(Ordering::Acquire),
            "callback-to-worker invoke cycle timed out"
        );
        assert_eq!(
            *lock(&cycle_result),
            Some((true, true)),
            "loopback invoke must complete before the callback returns"
        );
        assert_eq!(
            record.entries(),
            vec![
                (String::from("enter"), Some(integer(1.0))),
                (String::from("joined"), None),
                (String::from("exit"), Some(integer(1.0))),
                (String::from("enter"), Some(integer(2.0))),
                (String::from("exit"), Some(integer(2.0))),
            ],
            "callback joins the worker, then the owner delivers the worker-triggered update"
        );
        assert!(record.on_one_thread(), "delivery stays on the owning thread");
        subscription.close(Context::background()).await.expect("close");
        provider.dispose();
    }

    #[tokio::test]
    async fn stale_close_token_cannot_close_replacement_generation() {
        let state_one = MutableReplicatedState::new(object(0.0));
        let state_two = MutableReplicatedState::new(object(0.0));
        let room = JsString::from_utf8("room");
        let key = JsString::from_utf8("k");
        let provider = provider(vec![definition("room", ServiceMode::Keyed)]);
        let record = Arc::new(Record::new());
        let listener = recording_listener(Arc::clone(&record), Arc::new(|_: JsInteger| {}));
        let subscription = provider
            .subscribe(room.clone(), ServiceMode::Keyed, listener, Context::background())
            .await
            .expect("subscribe");
        subscription.activate();

        let handle_one = provider
            .spawn(&room, key.clone(), state_members(&state_one))
            .expect("spawn one");
        assert_eq!(handle_one.address().generation, integer(1.0));
        assert!(
            provider
                .spawn(&room, key.clone(), state_members(&state_one))
                .is_err(),
            "duplicate live key is rejected"
        );
        handle_one.close();
        handle_one.close();
        let handle_two = provider
            .spawn(&room, key.clone(), state_members(&state_two))
            .expect("spawn two");
        assert_eq!(handle_two.address().generation, integer(2.0));
        handle_one.close();
        assert_eq!(
            record.labels(),
            labels(&["spawned", "closed", "spawned"]),
            "stale token cannot close the replacement generation"
        );

        match invoke_state(&provider, &room, &key, integer(1.0)).await {
            Err(ServiceError::Remote(remote)) => {
                assert_eq!(remote.code, RemoteServiceErrorCode::ServiceStaleInstance);
            }
            other => panic!("expected stale instance error, got {other:?}"),
        }
        match invoke_state(&provider, &room, &key, integer(2.0)).await {
            Err(ServiceError::Remote(remote)) => {
                assert_eq!(remote.code, RemoteServiceErrorCode::ServiceMemberMismatch);
            }
            other => panic!("expected member mismatch for the live generation, got {other:?}"),
        }

        publish(&state_one, 1.0);
        assert_eq!(
            record.labels(),
            labels(&["spawned", "closed", "spawned"]),
            "deactivated instance publishes nothing"
        );
        publish(&state_two, 1.0);
        assert_eq!(
            record.entries(),
            vec![
                (String::from("spawned"), None),
                (String::from("closed"), None),
                (String::from("spawned"), None),
                (String::from("enter"), Some(integer(1.0))),
                (String::from("exit"), Some(integer(1.0))),
            ],
            "replacement publishes under its own generation"
        );
        handle_two.close();
        publish(&state_two, 2.0);
        assert_eq!(
            record.labels(),
            labels(&["spawned", "closed", "spawned", "enter", "exit", "closed"]),
            "closed replacement publishes nothing further"
        );
        subscription.close(Context::background()).await.expect("close");
        provider.dispose();
    }

    #[tokio::test]
    async fn replacement_and_disposal_notifications_preserve_facade_identity() {
        let state_one = MutableReplicatedState::new(object(0.0));
        let state_two = MutableReplicatedState::new(object(0.0));
        let svc = JsString::from_utf8("svc");
        let provider = provider(vec![definition("svc", ServiceMode::Singleton)]);
        provider
            .provide(&svc, state_members(&state_one))
            .expect("provide");
        let record = Arc::new(Record::new());
        let listener = recording_listener(Arc::clone(&record), Arc::new(|_: JsInteger| {}));
        let subscription = provider
            .subscribe(svc.clone(), ServiceMode::Singleton, listener, Context::background())
            .await
            .expect("subscribe");
        subscription.activate();
        assert!(record.entries().is_empty());

        provider.replace(&svc, state_members(&state_two)).expect("replace");
        assert_eq!(
            record.labels(),
            labels(&["replaced"]),
            "replacement retains the facade and emits no unavailable"
        );
        publish(&state_one, 1.0);
        assert_eq!(
            record.labels(),
            labels(&["replaced"]),
            "replaced instance publishes nothing"
        );
        publish(&state_two, 1.0);
        assert_eq!(record.labels(), labels(&["replaced", "enter", "exit"]));

        provider.withdraw(&svc).expect("withdraw");
        assert_eq!(
            record.labels(),
            labels(&["replaced", "enter", "exit", "unavailable"]),
        );
        publish(&state_two, 2.0);
        assert_eq!(
            record.labels(),
            labels(&["replaced", "enter", "exit", "unavailable"]),
            "withdrawn instance publishes nothing"
        );

        provider.provide(&svc, state_members(&state_two)).expect("re-provide");
        publish(&state_two, 3.0);
        assert_eq!(
            record.labels(),
            labels(&["replaced", "enter", "exit", "unavailable", "enter", "exit"]),
        );

        provider.dispose();
        assert_eq!(
            record.labels(),
            labels(&[
                "replaced",
                "enter",
                "exit",
                "unavailable",
                "enter",
                "exit",
                "unavailable",
            ]),
            "dispose emits the final unavailable"
        );
        provider.dispose();
        assert_eq!(record.labels().len(), 7, "dispose is idempotent");
        subscription.close(Context::background()).await.expect("close after dispose");
        publish(&state_two, 4.0);
        assert_eq!(record.labels().len(), 7, "nothing delivers after disposal");
    }
}
