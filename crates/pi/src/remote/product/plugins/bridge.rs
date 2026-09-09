//! Native side of the development-only product facet bridge.
//!
//! The bridge keeps Chord values canonical while leaving framing and process
//! ownership to `ExtensionRuntimeSet`. It discovers service member shapes with
//! the Chord control subscription, exposes those members through the native
//! `RemoteServiceProvider`, and routes subsequent state publications back into
//! the same provider-owned states.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::service::delta::{DeltaOp, apply_immutable};
use pi_agent::service::error::ServiceError;
use pi_agent::service::provider::{
    RemoteServiceProvider, ServiceDefinition, ServiceImplementation, ServiceMember, ServiceMethod,
};
use pi_agent::service::value::{JsInteger, JsString, JsonValue};
use pi_agent::service::wire::{
    ServiceCall, ServiceCatalogueEntry, ServiceInstanceAddress, ServiceInstanceSnapshot,
    ServiceMemberSnapshot, ServiceMode, ServiceProviderUpdate, parse_service_subscription_snapshot,
};
use pi_ext::facet::{
    FACET_HOST_DISPOSE_METHOD, FACET_HOST_LOAD_METHOD, FACET_HOST_RELOAD_METHOD,
    FACET_SERVICE_INVOKE_METHOD, FacetHostEntry, FacetHostLoadResponse, FacetServiceInvokeRequest,
    FacetServiceInvokeResult, FacetServiceUpdateEvent, catalogue_into_json,
};

/// Request/response seam implemented by the extension-runtime owner.
///
/// The returned `None` is the response-side equivalent of source `undefined`;
/// a present `JsonValue::Null` remains a real value. Implementations must keep
/// the request alive until the response or cancellation is observed.
pub trait FacetHostTransport: Send + Sync {
    /// Sends one open-method request to the extension host.
    fn request(
        &self,
        method: &'static str,
        payload: JsonValue,
        context: Context,
    ) -> BoxFuture<'static, Result<Option<JsonValue>, ServiceError>>;
}

/// Inputs required to create one host generation.
#[derive(Clone, Debug)]
pub struct PluginFacetHostOptions {
    /// Host identity fenced to the owning server/session.
    pub host_id: String,
    /// Session or presentation entry.
    pub entry: FacetHostEntry,
    /// On-disk bundle manifests.
    pub manifest_paths: Option<Vec<String>>,
    /// Materialized bundle artifacts.
    pub artifacts: Option<Vec<JsonValue>>,
    /// Native services available to loaded facets.
    pub builtin_catalogue: Vec<ServiceCatalogueEntry>,
}

/// Native host facade used by product service providers.
pub trait PluginFacetHost: Send + Sync {
    /// Returns the loaded host's remote service definitions.
    fn catalogue(&self) -> Vec<ServiceDefinition>;
    /// Publishes the discovered implementations into a native provider.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when a definition cannot be published.
    fn provide_into(&self, provider: &RemoteServiceProvider) -> Result<(), ServiceError>;
    /// Reloads the host generation in place.
    fn reload(&self) -> BoxFuture<'static, Result<(), ServiceError>>;
    /// Closes subscriptions and disposes the host generation.
    fn dispose(&self) -> BoxFuture<'static, Result<(), ServiceError>>;
}

/// A loaded host generation backed by an extension-host transport.
pub struct RemotePluginFacetHost {
    host_id: String,
    transport: Arc<dyn FacetHostTransport>,
    state: Arc<Mutex<PluginHostState>>,
    disposed: Arc<AtomicBool>,
}

struct PluginHostState {
    services: Vec<ServiceRegistration>,
    routes: Vec<StateRoute>,
    subscriptions: Vec<JsString>,
    slash_commands: Vec<JsonValue>,
}
#[derive(Clone)]
struct ServiceRegistration {
    definition: ServiceDefinition,
    singleton: Option<ServiceImplementation>,
    keyed: BTreeMap<JsString, ServiceImplementation>,
}

struct StateRoute {
    subscription_id: JsString,
    instance: Option<ServiceInstanceAddress>,
    member: JsString,
    sequence: JsInteger,
    state: Arc<pi_agent::service::replicated::MutableReplicatedState>,
}

impl RemotePluginFacetHost {
    /// Loads one host generation and discovers all service member shapes.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the host load request fails or the response
    /// cannot be parsed.
    pub fn load(
        transport: Arc<dyn FacetHostTransport>,
        options: PluginFacetHostOptions,
        context: Context,
    ) -> BoxFuture<'static, Result<(Arc<Self>, FacetHostLoadResponse), ServiceError>> {
        Box::pin(async move {
            let host = Arc::new(Self {
                host_id: options.host_id.clone(),
                transport,
                state: Arc::new(Mutex::new(PluginHostState {
                    services: Vec::new(),
                    routes: Vec::new(),
                    subscriptions: Vec::new(),
                    slash_commands: Vec::new(),
                })),
                disposed: Arc::new(AtomicBool::new(false)),
            });
            let response = host.request_load(&options, context).await?;
            host.prime_services(
                &response.catalogue,
                response.slash_commands.clone(),
                Context::background(),
            )
            .await?;
            Ok((host, response))
        })
    }

    /// Routes a canonical host update into the matching provider-owned state.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if the update is not addressed to this host or
    /// cannot be applied to the matching provider-owned state.
    pub fn handle_service_update(
        &self,
        event: FacetServiceUpdateEvent,
        context: Context,
    ) -> Result<(), ServiceError> {
        if event.host_id != self.host_id.as_str() {
            return Err(ServiceError::local(
                "facet service update addressed another host",
            ));
        }
        let subscription_id = event.subscription_id;
        match event.update {
            ServiceProviderUpdate::State {
                instance,
                member,
                sequence,
                ops,
            } => self.apply_state_update(
                &subscription_id,
                instance.as_ref(),
                &member,
                sequence,
                &ops,
                context,
            ),
            ServiceProviderUpdate::Replaced { snapshot } => {
                self.apply_snapshot_update(&subscription_id, &snapshot, &context)
            }
            ServiceProviderUpdate::Unavailable
            | ServiceProviderUpdate::Spawned { .. }
            | ServiceProviderUpdate::Closed { .. } => Ok(()),
        }
    }

    /// Returns the latest slash metadata received during load/reload.
    #[must_use]
    pub fn slash_commands(&self) -> Vec<JsonValue> {
        lock_unpoisoned(&self.state).slash_commands.clone()
    }

    async fn request_load(
        &self,
        options: &PluginFacetHostOptions,
        context: Context,
    ) -> Result<FacetHostLoadResponse, ServiceError> {
        let mut fields = BTreeMap::new();
        fields.insert(
            string_key("hostId"),
            JsonValue::String(options.host_id.clone().into()),
        );
        fields.insert(
            string_key("entry"),
            JsonValue::String(options.entry.as_str().into()),
        );
        if let Some(paths) = &options.manifest_paths {
            fields.insert(
                string_key("manifestPaths"),
                JsonValue::Array(
                    paths
                        .iter()
                        .map(|path| JsonValue::String(JsString::from_utf8(path)))
                        .collect(),
                ),
            );
        }
        if let Some(artifacts) = &options.artifacts {
            fields.insert(string_key("artifacts"), JsonValue::Array(artifacts.clone()));
        }
        fields.insert(
            string_key("builtinCatalogue"),
            catalogue_into_json(options.builtin_catalogue.clone()),
        );
        let response = self
            .transport
            .request(FACET_HOST_LOAD_METHOD, JsonValue::Object(fields), context)
            .await?;
        parse_host_response(response)
    }

    async fn prime_services(
        &self,
        catalogue: &[ServiceCatalogueEntry],
        slash_commands: Vec<JsonValue>,
        context: Context,
    ) -> Result<(), ServiceError> {
        let mut next_state = PluginHostState {
            services: Vec::with_capacity(catalogue.len()),
            routes: Vec::new(),
            subscriptions: Vec::with_capacity(catalogue.len()),
            slash_commands,
        };
        for (index, entry) in catalogue.iter().enumerate() {
            let subscription_id =
                JsString::from_utf8(format!("{}:native:{}", self.host_id, index).as_str());
            let snapshot = self
                .subscribe_shape(entry, &subscription_id, context.clone())
                .await?;
            if snapshot.service_id != entry.service_id || snapshot.mode != entry.mode {
                return Err(ServiceError::local(
                    "facet service returned a mismatched subscription shape",
                ));
            }
            let registration = registration_from_snapshot(
                entry,
                &snapshot.instances,
                &subscription_id,
                &self.transport,
                &self.host_id,
                &mut next_state.routes,
            )?;
            next_state.services.push(registration);
            next_state.subscriptions.push(subscription_id);
        }
        *lock_checked(&self.state)? = next_state;
        Ok(())
    }

    async fn subscribe_shape(
        &self,
        entry: &ServiceCatalogueEntry,
        subscription_id: &JsString,
        context: Context,
    ) -> Result<pi_agent::service::wire::ServiceSubscriptionSnapshot<DeltaOp>, ServiceError> {
        let call = ServiceCall {
            service_id: JsString::from_utf8("$chord.service"),
            instance: None,
            member: JsString::from_utf8("subscribe"),
            args: vec![
                JsonValue::String(subscription_id.clone()),
                JsonValue::String(entry.service_id.clone()),
                JsonValue::String(JsString::from_utf8(entry.mode.as_str())),
            ],
        };
        let request = FacetServiceInvokeRequest {
            host_id: JsString::from_utf8(&self.host_id),
            call,
        };
        let result = self
            .transport
            .request(FACET_SERVICE_INVOKE_METHOD, request.into_json(), context)
            .await?
            .ok_or_else(|| ServiceError::local("facet service subscription returned undefined"))?;
        let FacetServiceInvokeResult::Present(snapshot) =
            FacetServiceInvokeResult::from_json(&result)
                .map_err(|error| ServiceError::local(error.to_string()))?
        else {
            return Err(ServiceError::local(
                "facet service subscription returned undefined",
            ));
        };
        parse_service_subscription_snapshot(&snapshot)
            .map_err(|error| ServiceError::local(error.to_string()))
    }

    fn apply_state_update(
        &self,
        subscription_id: &JsString,
        instance: Option<&ServiceInstanceAddress>,
        member: &JsString,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: Context,
    ) -> Result<(), ServiceError> {
        let state = {
            let mut host_state = lock_checked(&self.state)?;
            let Some(route) = host_state.routes.iter_mut().find(|route| {
                &route.subscription_id == subscription_id
                    && &route.member == member
                    && route.instance.as_ref() == instance
            }) else {
                return Err(ServiceError::local(
                    "facet service update has no matching state member",
                ));
            };
            if sequence <= route.sequence {
                return Ok(());
            }
            let current = route.state.state();
            let next = apply_immutable(Some(current.as_ref()), ops)?.ok_or_else(|| {
                ServiceError::local("facet service state update cleared its value")
            })?;
            route.state.with_state_mut(|value| *value = next);
            route.sequence = sequence;
            Arc::clone(&route.state)
        };
        state.publish(context)
    }

    fn apply_snapshot_update(
        &self,
        subscription_id: &JsString,
        snapshot: &ServiceInstanceSnapshot<DeltaOp>,
        context: &Context,
    ) -> Result<(), ServiceError> {
        let states = {
            let mut host_state = lock_checked(&self.state)?;
            let mut states = Vec::new();
            for member in &snapshot.members {
                let ServiceMemberSnapshot::State {
                    name,
                    sequence,
                    ops,
                } = member
                else {
                    continue;
                };
                let Some(route) = host_state.routes.iter_mut().find(|route| {
                    &route.subscription_id == subscription_id
                        && &route.member == name
                        && route.instance.as_ref() == snapshot.instance.as_ref()
                }) else {
                    continue;
                };
                let next = apply_immutable(None, ops)?
                    .ok_or_else(|| ServiceError::local("facet replacement state has no value"))?;
                route.state.with_state_mut(|value| *value = next);
                route.sequence = *sequence;
                states.push(Arc::clone(&route.state));
            }
            states
        };
        for state in states {
            state.publish(context.clone())?;
        }
        Ok(())
    }
}

impl PluginFacetHost for RemotePluginFacetHost {
    fn catalogue(&self) -> Vec<ServiceDefinition> {
        lock_unpoisoned(&self.state)
            .services
            .iter()
            .map(|service| service.definition.clone())
            .collect()
    }

    /// Publishes the discovered implementations into a native provider.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] if a service cannot be registered with the
    /// native provider.
    fn provide_into(&self, provider: &RemoteServiceProvider) -> Result<(), ServiceError> {
        let services = lock_checked(&self.state)?.services.clone();
        for service in &services {
            match service.definition.mode {
                ServiceMode::Singleton => {
                    let implementation = service.singleton.clone().ok_or_else(|| {
                        ServiceError::local("singleton facet service has no implementation")
                    })?;
                    provider.provide(&service.definition.id, implementation)?;
                }
                ServiceMode::Keyed => {
                    for (key, implementation) in &service.keyed {
                        provider.spawn(
                            &service.definition.id,
                            key.clone(),
                            implementation.clone(),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), ServiceError>> {
        let host_id = self.host_id.clone();
        let transport = Arc::clone(&self.transport);
        let state = Arc::clone(&self.state);
        let disposed = Arc::clone(&self.disposed);
        Box::pin(async move {
            if disposed.load(Ordering::Acquire) {
                return Err(ServiceError::disposed("facet host is disposed"));
            }
            let response = transport
                .request(
                    FACET_HOST_RELOAD_METHOD,
                    object_value([(string_key("hostId"), JsonValue::String(host_id.into()))]),
                    Context::background(),
                )
                .await?;
            if disposed.load(Ordering::Acquire) {
                return Err(ServiceError::disposed("facet host is disposed"));
            }
            let response = parse_host_response(response)?;
            let current = lock_checked(&state)?;
            let current_catalogue = current
                .services
                .iter()
                .map(|service| ServiceCatalogueEntry {
                    service_id: service.definition.id.clone(),
                    mode: service.definition.mode,
                })
                .collect::<Vec<_>>();
            if current_catalogue != response.catalogue {
                return Err(ServiceError::local(
                    "facet host reload changed its service catalogue",
                ));
            }
            drop(current);
            lock_checked(&state)?.slash_commands = response.slash_commands;
            Ok(())
        })
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), ServiceError>> {
        let host_id = self.host_id.clone();
        let transport = Arc::clone(&self.transport);
        let state = Arc::clone(&self.state);
        let was_disposed = self.disposed.swap(true, Ordering::AcqRel);
        Box::pin(async move {
            if was_disposed {
                return Ok(());
            }
            let subscriptions = {
                let mut state = lock_checked(&state)?;
                std::mem::take(&mut state.subscriptions)
            };
            let mut errors = Vec::new();
            for subscription_id in subscriptions {
                let call = ServiceCall {
                    service_id: JsString::from_utf8("$chord.service"),
                    instance: None,
                    member: JsString::from_utf8("unsubscribe"),
                    args: vec![JsonValue::String(subscription_id)],
                };
                let request = FacetServiceInvokeRequest {
                    host_id: JsString::from_utf8(&host_id),
                    call,
                };
                if let Err(error) = transport
                    .request(
                        FACET_SERVICE_INVOKE_METHOD,
                        request.into_json(),
                        Context::background(),
                    )
                    .await
                {
                    errors.push(error);
                }
            }
            if let Err(error) = transport
                .request(
                    FACET_HOST_DISPOSE_METHOD,
                    object_value([(string_key("hostId"), JsonValue::String(host_id.into()))]),
                    Context::background(),
                )
                .await
            {
                errors.push(error);
            }
            if errors.len() == 1 {
                return Err(errors.remove(0));
            }
            if !errors.is_empty() {
                return Err(ServiceError::internal(format!(
                    "facet host disposal failed in {} operations",
                    errors.len()
                )));
            }
            Ok(())
        })
    }
}

fn registration_from_snapshot(
    entry: &ServiceCatalogueEntry,
    instances: &[ServiceInstanceSnapshot<DeltaOp>],
    subscription_id: &JsString,
    transport: &Arc<dyn FacetHostTransport>,
    host_id: &str,
    routes: &mut Vec<StateRoute>,
) -> Result<ServiceRegistration, ServiceError> {
    let definition = ServiceDefinition {
        id: entry.service_id.clone(),
        local: false,
        mode: entry.mode,
    };
    match entry.mode {
        ServiceMode::Singleton => {
            let [instance] = instances else {
                return Err(ServiceError::local(
                    "singleton facet service must return one instance",
                ));
            };
            if instance.instance.is_some() {
                return Err(ServiceError::local(
                    "singleton facet service returned an instance address",
                ));
            }
            let implementation = implementation_from_instance(
                &entry.service_id,
                None,
                instance,
                subscription_id,
                transport,
                host_id,
                routes,
            )?;
            Ok(ServiceRegistration {
                definition,
                singleton: Some(implementation),
                keyed: BTreeMap::new(),
            })
        }
        ServiceMode::Keyed => {
            let mut keyed = BTreeMap::new();
            for instance in instances {
                let address = instance.instance.as_ref().ok_or_else(|| {
                    ServiceError::local("keyed facet service omitted its instance address")
                })?;
                let implementation = implementation_from_instance(
                    &entry.service_id,
                    Some(address),
                    instance,
                    subscription_id,
                    transport,
                    host_id,
                    routes,
                )?;
                if keyed.insert(address.key.clone(), implementation).is_some() {
                    return Err(ServiceError::local(
                        "keyed facet service returned duplicate instance keys",
                    ));
                }
            }
            Ok(ServiceRegistration {
                definition,
                singleton: None,
                keyed,
            })
        }
    }
}

fn implementation_from_instance(
    service_id: &JsString,
    instance: Option<&ServiceInstanceAddress>,
    snapshot: &ServiceInstanceSnapshot<DeltaOp>,
    subscription_id: &JsString,
    transport: &Arc<dyn FacetHostTransport>,
    host_id: &str,
    routes: &mut Vec<StateRoute>,
) -> Result<ServiceImplementation, ServiceError> {
    let mut implementation = BTreeMap::new();
    for member in &snapshot.members {
        match member {
            ServiceMemberSnapshot::Method { name } => {
                implementation.insert(
                    name.clone(),
                    ServiceMember::Method(remote_method(
                        transport, host_id, service_id, instance, name,
                    )),
                );
            }
            ServiceMemberSnapshot::State {
                name,
                sequence,
                ops,
            } => {
                let initial = apply_immutable(None, ops)?
                    .ok_or_else(|| ServiceError::local("facet state snapshot has no value"))?;
                let state = pi_agent::service::replicated::MutableReplicatedState::new(initial);
                routes.push(StateRoute {
                    subscription_id: subscription_id.clone(),
                    instance: instance.cloned(),
                    member: name.clone(),
                    sequence: *sequence,
                    state: Arc::clone(&state),
                });
                implementation.insert(name.clone(), ServiceMember::State(state));
            }
        }
    }
    Ok(implementation)
}

fn remote_method(
    transport: &Arc<dyn FacetHostTransport>,
    host_id: &str,
    service_id: &JsString,
    instance: Option<&ServiceInstanceAddress>,
    member: &JsString,
) -> ServiceMethod {
    let host_id = JsString::from_utf8(host_id);
    let service_id = service_id.clone();
    let instance = instance.cloned();
    let member = member.clone();
    let transport = Arc::clone(transport);
    Arc::new(move |args, context| {
        let request = FacetServiceInvokeRequest {
            host_id: host_id.clone(),
            call: ServiceCall {
                service_id: service_id.clone(),
                instance: instance.clone(),
                member: member.clone(),
                args,
            },
        };
        let transport = Arc::clone(&transport);
        Box::pin(async move {
            transport
                .request(FACET_SERVICE_INVOKE_METHOD, request.into_json(), context)
                .await
        })
    })
}

fn parse_host_response(value: Option<JsonValue>) -> Result<FacetHostLoadResponse, ServiceError> {
    let value = value.ok_or_else(|| ServiceError::local("facet host returned undefined"))?;
    let fields = value
        .as_object()
        .ok_or_else(|| ServiceError::local("facet host response must be an object"))?;
    let catalogue_value = fields
        .get(&string_key("catalogue"))
        .ok_or_else(|| ServiceError::local("facet host response omitted catalogue"))?;
    let catalogue = pi_agent::service::wire::parse_service_catalogue(catalogue_value)
        .map_err(|error| ServiceError::local(error.to_string()))?;
    let slash_commands = fields
        .get(&string_key("slashCommands"))
        .and_then(JsonValue::as_array)
        .ok_or_else(|| ServiceError::local("facet host response omitted slashCommands"))?
        .clone();
    Ok(FacetHostLoadResponse {
        catalogue,
        slash_commands,
    })
}

fn object_value<const N: usize>(fields: [(JsString, JsonValue); N]) -> JsonValue {
    JsonValue::Object(fields.into_iter().collect())
}

fn string_key(value: &str) -> JsString {
    JsString::from_utf8(value)
}

fn lock_checked<T>(value: &Mutex<T>) -> Result<MutexGuard<'_, T>, ServiceError> {
    value
        .lock()
        .map_err(|_| ServiceError::internal("facet host state lock poisoned"))
}

fn lock_unpoisoned<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
