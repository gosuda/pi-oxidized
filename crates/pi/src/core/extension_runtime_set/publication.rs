//! Facade publication state machine and live-endpoint fan-out combinators.
//!
//! # Shape (WHY)
//!
//! `PublishedRuntimeState` is the mutable, lock-guarded mirror of the
//! currently published endpoint generation: slots, relay routes, retired
//! endpoints, and the aggregate `ModelRuntime` provider wiring. The facade
//! (`ExtensionRuntimeSet` in the parent module) holds it behind an
//! `Arc<StdMutex<…>>` so relay tasks can read it concurrently while reload
//! and shutdown swap it under the same lock.
//!
//! `FacadeChannels` owns the broadcast/mpsc/watch senders that relays fan
//! events into. Both types are `pub(super)` so the parent module's relay
//! wiring and the facade's own methods can drive them directly; nothing
//! escapes the `extension_runtime_set` module.
//!
//! The fan-out combinators at the bottom collapse the repeated
//! `lease().live_endpoints()` iteration shapes that the facade and the
//! `ExtensionRunner` impl used to hand-roll per method. Each combinator
//! owns one honest iteration shape; sites that do not fit any shape stay
//! inline at their call site with a WHY comment.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use futures::stream::{FuturesUnordered, StreamExt};
use pi_ext::client::HostUiRequest;
use pi_ext::protocol::{ExtensionErrorEvent, ProviderEvent, ProvidersUpdate, ToolUpdate};
use pi_ext::sanitize::SanitizedSlot;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};

use super::PendingEndpointBridges;
use super::generation::{Endpoint, EndpointId, Generation, GenerationLease};
use crate::core::agent_session::BridgeRequestId;
use crate::core::extension_host::{EVENT_CHANNEL_CAPACITY, ExtensionUiEvent, SessionBridgeEvent};
use crate::core::model_runtime::{ModelRuntime, ModelRuntimeError};

/// Every relay for one endpoint shares this stable routing identity.
///
/// Private to this module: only `PublishedRuntimeState` ever reads or writes
/// the route table.
#[derive(Clone, Copy)]
pub(super) struct CorrelationRoute {
    endpoint: EndpointId,
    local: BridgeRequestId,
}

/// Mutable, lock-guarded mirror of the published endpoint generation.
///
/// Fields are `pub(super)` because the parent module's relay wiring reads and
/// mutates them directly under the same `StdMutex` guard (retirement checks,
/// provider-runtime swaps during cutover). They never escape the module.
pub(super) struct PublishedRuntimeState {
    pub(super) generation: Arc<Generation>,
    slots: HashMap<String, BTreeMap<EndpointId, SanitizedSlot>>,
    pub(super) routes: HashMap<BridgeRequestId, CorrelationRoute>,
    pub(super) retired: BTreeSet<EndpointId>,
    pub(super) provider_runtime: Option<ModelRuntime>,
    next_route_id: BridgeRequestId,
    pub(super) stale: bool,
    pub(super) shutdown_done: bool,
}

impl PublishedRuntimeState {
    pub(super) fn new(generation: Arc<Generation>) -> Self {
        Self {
            generation,
            slots: HashMap::new(),
            routes: HashMap::new(),
            retired: BTreeSet::new(),
            provider_runtime: None,
            next_route_id: BridgeRequestId(1),
            stale: false,
            shutdown_done: false,
        }
    }

    pub(super) fn lease(&self) -> GenerationLease {
        let counted = !self.stale && !self.shutdown_done;
        if counted {
            self.generation.count_lease();
        }
        GenerationLease {
            generation: Arc::clone(&self.generation),
            counted,
        }
    }

    pub(super) fn is_current_generation_endpoint(&self, endpoint: EndpointId) -> bool {
        self.generation.endpoint(endpoint).is_some()
    }

    pub(super) fn accepts_relay(&self, endpoint: EndpointId) -> bool {
        !self.stale
            && !self.shutdown_done
            && !self.retired.contains(&endpoint)
            && self
                .generation
                .endpoint(endpoint)
                .is_some_and(|endpoint| endpoint.runner.is_active())
    }

    /// Apply a live `providers.update` from one endpoint.
    ///
    /// Replaces only that endpoint's provider snapshot in the runner, then
    /// rebuilds the aggregate in live endpoint order and rewires
    /// `ModelRuntime`. The snapshot write lock is released before any
    /// `ModelRuntime` call (it is held inside `apply_providers_update`
    /// on the runner and released before we touch the runtime).
    pub(super) fn apply_providers_update(&mut self, endpoint: &Endpoint, update: &ProvidersUpdate) {
        // Remove the pre-update aggregate before replacing this endpoint's
        // authoritative snapshot, or an unregistered name disappears before
        // ModelRuntime can remove its stale config and stream adapter.
        let runtime = self.provider_runtime.clone();
        if let Some(runtime) = &runtime {
            unregister_endpoint_providers(&self.generation.endpoints, runtime);
        }

        endpoint.runner.apply_providers_update(update);

        let Some(runtime) = runtime else {
            return;
        };
        let active = self
            .generation
            .endpoints
            .iter()
            .filter(|ep| !self.retired.contains(&ep.id) && ep.runner.is_active());
        register_endpoint_providers(active, &runtime);
    }

    pub(super) fn retire_endpoint(
        &mut self,
        endpoint: EndpointId,
        channels: &FacadeChannels,
    ) -> bool {
        if self.stale
            || self.shutdown_done
            || !self.is_current_generation_endpoint(endpoint)
            || self.retired.contains(&endpoint)
        {
            return false;
        }

        let dead_provider_names = self
            .generation
            .endpoint(endpoint)
            .map(|dead| {
                dead.runner
                    .provider_configs()
                    .into_keys()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let owned_provider_names = dead_provider_names
            .into_iter()
            .filter(|name| {
                self.generation
                    .endpoints
                    .iter()
                    .find(|candidate| {
                        (candidate.id == endpoint
                            || (!self.retired.contains(&candidate.id)
                                && candidate.runner.is_active()))
                            && candidate.runner.provider_configs().contains_key(name)
                    })
                    .is_some_and(|owner| owner.id == endpoint)
            })
            .collect::<Vec<_>>();

        self.retired.insert(endpoint);
        self.routes.retain(|_, route| route.endpoint != endpoint);

        let mut keys = self.slots.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let Some(owners) = self.slots.get_mut(&key) else {
                continue;
            };
            let old_owner = owners.last_key_value().map(|(owner, _)| *owner);
            owners.remove(&endpoint);
            if old_owner != Some(endpoint) {
                continue;
            }
            if let Some((_, fallback)) = owners.last_key_value() {
                let _ = channels
                    .ui_tx
                    .send(ExtensionUiEvent::Slot(fallback.clone()));
            } else {
                self.slots.remove(&key);
                let _ = channels.ui_tx.send(ExtensionUiEvent::Dispose { key });
            }
        }

        if let Some(runtime) = self.provider_runtime.clone() {
            for name in owned_provider_names {
                runtime.unregister_provider(&name);
                let fallback = self.generation.endpoints.iter().find(|candidate| {
                    !self.retired.contains(&candidate.id)
                        && candidate.runner.is_active()
                        && candidate.runner.provider_configs().contains_key(&name)
                });
                if let Some(fallback) = fallback {
                    let (path, outcome) = register_endpoint_provider(fallback, &name, &runtime);
                    if let Err(error) = outcome {
                        channels.publish_error(
                            "extension_provider_rewire_failed",
                            format!(
                                "Extension {path:?} provider {name:?} failed to rewire: {error}"
                            ),
                            None,
                        );
                    }
                }
            }
        }
        if let Some(dead) = self.generation.endpoint(endpoint) {
            dead.runner.invalidate();
        }
        channels.publish_registry_change();
        true
    }

    pub(super) fn reloadable(&self) -> bool {
        !self.stale && !self.shutdown_done && self.generation.has_one_active_compat_endpoint()
    }

    pub(super) fn allocate_route(
        &mut self,
        endpoint: EndpointId,
        local: BridgeRequestId,
    ) -> Option<BridgeRequestId> {
        if !self.accepts_relay(endpoint) {
            return None;
        }
        loop {
            let id = self.next_route_id;
            self.next_route_id = BridgeRequestId(id.0.wrapping_add(1));
            if id.0 == 0 || self.routes.contains_key(&id) {
                continue;
            }
            self.routes.insert(id, CorrelationRoute { endpoint, local });
            return Some(id);
        }
    }

    pub(super) fn release_route(&mut self, id: BridgeRequestId) {
        self.routes.remove(&id);
    }

    pub(super) fn claim_route(
        &mut self,
        id: BridgeRequestId,
    ) -> Option<(GenerationLease, Endpoint, BridgeRequestId)> {
        let route = *self.routes.get(&id)?;
        let endpoint = self.generation.endpoint(route.endpoint)?.clone();
        self.routes.remove(&id);
        Some((self.lease(), endpoint, route.local))
    }

    pub(super) fn record_slot(
        &mut self,
        endpoint: EndpointId,
        slot: SanitizedSlot,
        channels: &FacadeChannels,
    ) {
        if !self.accepts_relay(endpoint) {
            return;
        }
        let owners = self.slots.entry(slot.key.clone()).or_default();
        owners.insert(endpoint, slot.clone());
        if owners.last_key_value().map(|(owner, _)| *owner) == Some(endpoint) {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Slot(slot));
        }
    }

    pub(super) fn dispose_slot(
        &mut self,
        endpoint: EndpointId,
        key: String,
        channels: &FacadeChannels,
    ) {
        if !self.accepts_relay(endpoint) {
            return;
        }
        let Some(owners) = self.slots.get_mut(&key) else {
            return;
        };
        let was_owner = owners.last_key_value().map(|(owner, _)| *owner) == Some(endpoint);
        owners.remove(&endpoint);
        if !was_owner {
            return;
        }
        let event = if let Some((_, fallback)) = owners.last_key_value() {
            ExtensionUiEvent::Slot(fallback.clone())
        } else {
            self.slots.remove(&key);
            ExtensionUiEvent::Dispose { key }
        };
        let _ = channels.ui_tx.send(event);
    }

    pub(super) fn slot_owner(&self, key: &str) -> Option<(GenerationLease, Endpoint)> {
        let endpoint = *self.slots.get(key)?.last_key_value()?.0;
        if !self.accepts_relay(endpoint) {
            return None;
        }
        Some((self.lease(), self.generation.endpoint(endpoint)?.clone()))
    }

    pub(super) fn current_slots(&self) -> Vec<SanitizedSlot> {
        let mut slots = self
            .slots
            .values()
            .filter_map(|owners| owners.last_key_value().map(|(_, slot)| slot.clone()))
            .collect::<Vec<_>>();
        slots.sort_by(|left, right| left.key.cmp(&right.key));
        slots
    }

    pub(super) fn slot_keys(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

    pub(super) fn publish_initial_slots(
        &mut self,
        pending: &[PendingEndpointBridges],
        channels: &FacadeChannels,
    ) {
        for pending_endpoint in pending {
            for slot in &pending_endpoint.slots {
                self.record_slot(pending_endpoint.endpoint.id, slot.clone(), channels);
            }
        }
    }

    pub(super) fn replace_generation(
        &mut self,
        next: Arc<Generation>,
        pending: &[PendingEndpointBridges],
        channels: &FacadeChannels,
    ) -> Arc<Generation> {
        let old = std::mem::replace(&mut self.generation, next);
        self.retired.clear();
        let mut keys = self.slots.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        self.slots.clear();
        self.routes.clear();
        for key in keys {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
        self.publish_initial_slots(pending, channels);
        channels.publish_registry_change();
        old
    }

    pub(super) fn quiesce(&mut self, channels: &FacadeChannels) -> Arc<Generation> {
        self.stale = true;
        self.provider_runtime = None;
        let mut keys = self.slots.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        self.slots.clear();
        self.routes.clear();
        for key in keys {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
        channels.publish_registry_change();
        Arc::clone(&self.generation)
    }

    pub(super) fn invalidate(&mut self, channels: &FacadeChannels) -> Option<Arc<Generation>> {
        if self.stale {
            return None;
        }
        Some(self.quiesce(channels))
    }

    pub(super) fn begin_shutdown(&mut self, channels: &FacadeChannels) -> Option<Arc<Generation>> {
        if self.shutdown_done {
            return None;
        }
        self.shutdown_done = true;
        Some(self.quiesce(channels))
    }
}

/// Broadcast/mpsc/watch senders that relays fan events into.
///
/// Fields are `pub(super)` because the parent module's relay wiring and the
/// facade subscribe/publish directly. They never escape the module.
pub(super) struct FacadeChannels {
    pub(super) tool_updates_tx: broadcast::Sender<ToolUpdate>,
    pub(super) provider_events_tx: broadcast::Sender<ProviderEvent>,
    pub(super) errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    pub(super) ui_tx: broadcast::Sender<ExtensionUiEvent>,
    pub(super) ui_requests_tx: mpsc::Sender<HostUiRequest>,
    pub(super) ui_requests_rx: StdMutex<Option<mpsc::Receiver<HostUiRequest>>>,
    pub(super) session_bridge_tx: mpsc::Sender<SessionBridgeEvent>,
    pub(super) session_bridge_rx: StdMutex<Option<mpsc::Receiver<SessionBridgeEvent>>>,
    pub(super) registry_revision_tx: watch::Sender<u64>,
}

impl FacadeChannels {
    pub(super) fn new() -> Self {
        let (tool_updates_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (provider_events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (errors_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_requests_tx, ui_requests_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (session_bridge_tx, session_bridge_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (registry_revision_tx, _) = watch::channel(0_u64);
        Self {
            tool_updates_tx,
            provider_events_tx,
            errors_tx,
            ui_tx,
            ui_requests_tx,
            ui_requests_rx: StdMutex::new(Some(ui_requests_rx)),
            session_bridge_tx,
            session_bridge_rx: StdMutex::new(Some(session_bridge_rx)),
            registry_revision_tx,
        }
    }

    pub(super) fn publish_registry_change(&self) {
        self.registry_revision_tx
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(super) fn publish_error(&self, code: &str, message: String, data: Option<Value>) {
        let _ = self.errors_tx.send(ExtensionErrorEvent {
            code: code.to_owned(),
            message,
            retryable: false,
            data,
        });
    }
}

// `PendingEndpointBridges` is relay-side (it holds broadcast receivers
// spawned per endpoint) and stays in the parent module; it is imported at
// the top so `publish_initial_slots`/`replace_generation` can read its
// `endpoint` and `slots` fields without owning the relay wiring.

/// Register one endpoint's provider `name` onto `runtime`.
///
/// Returns the path used for diagnostics and the registration outcome. The
/// stream-adapter rewire is best-effort: a missing config is treated as a
/// no-op so an unregistered name does not poison the aggregate.
pub(super) fn register_endpoint_provider(
    endpoint: &Endpoint,
    name: &str,
    runtime: &ModelRuntime,
) -> (String, Result<(), ModelRuntimeError>) {
    let configs = endpoint.runner.provider_configs();
    let paths = endpoint.runner.provider_extension_paths();
    let path = paths.get(name).cloned().unwrap_or_else(|| name.to_owned());
    let Some(config) = configs.get(name) else {
        return (path, Ok(()));
    };
    let outcome = runtime.register_provider(name, config);
    if outcome.is_ok()
        && endpoint.runner.stream_provider_ids().contains(name)
        && let Some(adapter) = endpoint.runner.providers().remove(name)
    {
        runtime.register_extension_stream_provider(name.to_owned(), Arc::new(adapter));
    }
    (path, outcome)
}

/// Register each endpoint's first-owned providers in endpoint order.
pub(super) fn register_endpoint_providers<'a>(
    endpoints: impl IntoIterator<Item = &'a Endpoint>,
    runtime: &ModelRuntime,
) -> Vec<(String, Result<(), ModelRuntimeError>)> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for endpoint in endpoints {
        for name in endpoint.runner.provider_configs().into_keys() {
            if !seen.insert(name.clone()) {
                continue;
            }
            results.push(register_endpoint_provider(endpoint, &name, runtime));
        }
    }
    results
}

/// Remove each first-owned provider once.
pub(super) fn unregister_endpoint_providers(endpoints: &[Endpoint], runtime: &ModelRuntime) {
    let mut seen = HashSet::new();
    for endpoint in endpoints {
        for name in endpoint.runner.provider_configs().into_keys() {
            if seen.insert(name.clone()) {
                runtime.unregister_provider(&name);
            }
        }
    }
}

/// Diagnostic path label for one endpoint (its registered label).
pub(super) fn endpoint_diagnostic_path(endpoint: &Endpoint) -> String {
    endpoint.label.clone()
}

// ---------------------------------------------------------------------------
// Live-endpoint fan-out combinators.
//
// Each combinator owns one iteration shape that the facade and the
// `ExtensionRunner` impl repeat across many methods. They take a borrowed
// `GenerationLease` and a closure; the closure never sees the lease itself,
// only the `&Endpoint` it needs to dispatch. Sites whose shape does not fit
// any combinator stay inline at the call site with a WHY comment.
// ---------------------------------------------------------------------------

/// Sequential async fold over live endpoints, fail-fast on `Err`.
///
/// The closure receives the running state by value and returns the next
/// state (or an error, which propagates via `?` and aborts the remaining
/// endpoints). No early stop: every live endpoint is visited in order.
///
/// The `'a` lifetime ties the borrowed endpoints (which live in `lease`) to
/// the futures the closure returns, so the closure may hand `&Endpoint`
/// straight into an `async move` block that awaits a runner call.
pub(super) async fn fan_out_try_fold<'a, St, E, F, Fut>(
    lease: &'a GenerationLease,
    mut state: St,
    mut f: F,
) -> Result<St, E>
where
    F: FnMut(&'a Endpoint, St) -> Fut,
    Fut: Future<Output = Result<St, E>> + 'a,
{
    for endpoint in lease.live_endpoints() {
        state = f(endpoint, state).await?;
    }
    Ok(state)
}

/// Sequential async scan, stopping at the first endpoint that yields `Some`.
///
/// The closure returns `Ok(None)` to continue or `Ok(Some(_))` to stop and
/// return that value; `Err` propagates immediately. This is the
/// first-cancel / first-block shape used by the `ExtensionRunner` emit
/// family. The `'a` tie is the same as `fan_out_try_fold`.
pub(super) async fn fan_out_try_first<'a, R, E, F, Fut>(
    lease: &'a GenerationLease,
    mut f: F,
) -> Result<Option<R>, E>
where
    F: FnMut(&'a Endpoint) -> Fut,
    Fut: Future<Output = Result<Option<R>, E>> + 'a,
{
    for endpoint in lease.live_endpoints() {
        if let Some(result) = f(endpoint).await? {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

/// Concurrent fan-out: drive one future per live endpoint and drain all.
///
/// Futures are collected into a `FuturesUnordered` and awaited to completion;
/// results are ignored. This is the broadcast shape (`push_theme_update`,
/// `push_ui_state`, `push_session_state`). The `'a` tie lets each future
/// borrow its `&Endpoint` for the duration of the drain.
pub(super) async fn fan_out_concurrent<'a, F, Fut>(lease: &'a GenerationLease, f: F)
where
    F: Fn(&'a Endpoint) -> Fut,
    Fut: Future<Output = ()> + 'a,
{
    let mut sends = lease
        .live_endpoints()
        .map(f)
        .collect::<FuturesUnordered<_>>();
    while sends.next().await.is_some() {}
}

/// Sync first-wins map merge across live endpoints.
///
/// The closure yields each endpoint's `(key, value)` pairs in endpoint order;
/// the first endpoint to claim a key wins, matching the
/// `entry(k).or_insert(v)` loops this replaces. Works for both `HashMap` and
/// `BTreeMap` targets: pairs are de-duplicated before collection, so
/// `BTreeMap::from_iter` keeps the first-seen value per key while sorting by
/// key, and `HashMap::from_iter` keeps the first-seen value per key.
pub(super) fn fan_out_first_wins<K, V, M, I, F>(lease: &GenerationLease, mut f: F) -> M
where
    K: Eq + std::hash::Hash + Clone,
    M: FromIterator<(K, V)>,
    F: FnMut(&Endpoint) -> I,
    I: IntoIterator<Item = (K, V)>,
{
    let mut seen = HashSet::new();
    lease
        .live_endpoints()
        .flat_map(&mut f)
        .filter(|(key, _)| seen.insert(key.clone()))
        .collect()
}
