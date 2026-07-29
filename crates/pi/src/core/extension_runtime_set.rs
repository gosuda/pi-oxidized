//! Stable product facade over an ordered set of extension host endpoints.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use pi_agent::{AfterToolCallResult, AgentMessage, AgentTool, BeforeToolCallResult};
use pi_ai::{AssistantMessageEvent, ToolResultContent};
use pi_ext::adapters::{ExtensionProvider, Registry, RendererKind, ShortcutRegistration};
use pi_ext::client::{HostClient, HostClientError, HostUiRequest, HostUiResponse};
use pi_ext::host::{self, HostSource, HostSpec};
use pi_ext::protocol::{
    self, ExtensionErrorEvent, FlagValueWire, FrameId, ProviderEvent, SessionStateWire,
    ShortcutExecuteResponse, ThemeUpdate, ToolUpdate, UiEventRequest, UiEventResponse, UiStateWire,
};
use pi_ext::sanitize::SanitizedSlot;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc};

use super::agent_session::events::AgentSessionEvent;
use super::agent_session::extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, ExtensionRunnerError,
    InputTransformResult,
};
use super::agent_session_services::ExtensionFlagType;
use super::extension_host::{
    EVENT_CHANNEL_CAPACITY, ExtensionUiEvent, HOOK_TIMEOUT, HostExtensionRunner, HostStartError,
    SessionBridgeEvent, ToolRenderPhase, default_ui_response,
};
use super::extension_manifest::{ClassifiedExtension, ExtensionRuntime, classify};
use super::model_runtime::{ModelRuntime, ModelRuntimeError};
use super::resources::ResourceExtensionPaths;

/// One aggregate deadline shared by every terminal-input endpoint request.
pub const TERMINAL_INPUT_DEADLINE: Duration = Duration::from_millis(4);

/// Runtime used by one endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointKind {
    /// Compatibility TypeScript host.
    TsCompat,
    /// Direct native JSONL host.
    Native,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EndpointPlan {
    position: usize,
    kind: EndpointKind,
    entries: Vec<String>,
    diagnostic_paths: Vec<String>,
    builtins: bool,
    label: String,
}

/// One path-scoped startup or load failure. Other paths may remain active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionSetDiagnostic {
    /// Discovery path, resolved manifest entry, or `<builtins>`.
    pub path: String,
    /// Typed classification, spawn, handshake, or load failure text.
    pub message: String,
}

/// Result of best-effort cold startup.
pub struct ExtensionSetStart {
    /// Stable facade, absent only when no endpoint became ready.
    pub set: Option<Arc<ExtensionRuntimeSet>>,
    /// Ordered path-scoped failures.
    pub diagnostics: Vec<ExtensionSetDiagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct EndpointId {
    generation: u64,
    position: usize,
}

#[derive(Clone)]
struct Endpoint {
    id: EndpointId,
    kind: EndpointKind,
    label: String,
    runner: Arc<HostExtensionRunner>,
}

struct Generation {
    id: u64,
    endpoints: Arc<[Endpoint]>,
    bridges: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    leases: AtomicUsize,
    drained: tokio::sync::Notify,
}

impl Generation {
    fn endpoint(&self, id: EndpointId) -> Option<&Endpoint> {
        if id.generation != self.id {
            return None;
        }
        self.endpoints.get(id.position)
    }
    fn has_one_active_compat_endpoint(&self) -> bool {
        let mut active = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.runner.is_active());
        matches!(active.next(), Some(endpoint) if endpoint.kind == EndpointKind::TsCompat && active.next().is_none())
    }
    fn is_single_compat_replacement(&self) -> bool {
        self.endpoints.len() == 1 && self.endpoints[0].kind == EndpointKind::TsCompat
    }
}

struct GenerationLease {
    generation: Arc<Generation>,
    stale: bool,
}

impl GenerationLease {
    fn endpoints(&self) -> &[Endpoint] {
        &self.generation.endpoints
    }

    fn is_active(&self) -> bool {
        !self.stale
            && self
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.is_active())
    }

    fn is_running(&self) -> bool {
        !self.stale
            && self
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.is_running())
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        if self.generation.leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.generation.drained.notify_one();
        }
    }
}

struct PendingEndpointBridges {
    endpoint: Endpoint,
    tool_updates: broadcast::Receiver<ToolUpdate>,
    provider_events: broadcast::Receiver<ProviderEvent>,
    errors: broadcast::Receiver<ExtensionErrorEvent>,
    ui: broadcast::Receiver<ExtensionUiEvent>,
    ui_requests: Option<mpsc::Receiver<HostUiRequest>>,
    session_bridge: Option<mpsc::Receiver<SessionBridgeEvent>>,
    slots: Vec<SanitizedSlot>,
}

/// Every relay for one endpoint shares this stable routing identity.
struct EndpointRelayContext {
    state: Weak<StdMutex<PublishedRuntimeState>>,
    channels: Arc<FacadeChannels>,
    endpoint: Endpoint,
}

type PendingBridges = Vec<PendingEndpointBridges>;

#[derive(Clone, Copy)]
struct CorrelationRoute {
    endpoint: EndpointId,
    local: FrameId,
}

struct PublishedRuntimeState {
    generation: Arc<Generation>,
    slots: HashMap<String, BTreeMap<EndpointId, SanitizedSlot>>,
    routes: HashMap<FrameId, CorrelationRoute>,
    next_route_id: FrameId,
    stale: bool,
    shutdown_done: bool,
}

impl PublishedRuntimeState {
    fn new(generation: Arc<Generation>) -> Self {
        Self {
            generation,
            slots: HashMap::new(),
            routes: HashMap::new(),
            next_route_id: 1,
            stale: false,
            shutdown_done: false,
        }
    }

    fn lease(&self) -> GenerationLease {
        self.generation.leases.fetch_add(1, Ordering::Relaxed);
        GenerationLease {
            generation: Arc::clone(&self.generation),
            stale: self.stale,
        }
    }

    fn reloadable(&self) -> bool {
        !self.stale && !self.shutdown_done && self.generation.has_one_active_compat_endpoint()
    }

    fn allocate_route(&mut self, endpoint: EndpointId, local: FrameId) -> Option<FrameId> {
        self.generation.endpoint(endpoint)?;
        loop {
            let id = self.next_route_id;
            self.next_route_id = self.next_route_id.wrapping_add(1);
            if id == 0 || self.routes.contains_key(&id) {
                continue;
            }
            self.routes.insert(id, CorrelationRoute { endpoint, local });
            return Some(id);
        }
    }

    fn release_route(&mut self, id: FrameId) {
        self.routes.remove(&id);
    }

    fn claim_route(&mut self, id: FrameId) -> Option<(GenerationLease, Endpoint, FrameId)> {
        let route = *self.routes.get(&id)?;
        let endpoint = self.generation.endpoint(route.endpoint)?.clone();
        self.routes.remove(&id);
        Some((self.lease(), endpoint, route.local))
    }

    fn record_slot(
        &mut self,
        endpoint: EndpointId,
        slot: SanitizedSlot,
        channels: &FacadeChannels,
    ) {
        if self.generation.endpoint(endpoint).is_none() {
            return;
        }
        let owners = self.slots.entry(slot.key.clone()).or_default();
        owners.insert(endpoint, slot.clone());
        if owners.last_key_value().map(|(owner, _)| *owner) == Some(endpoint) {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Slot(slot));
        }
    }

    fn dispose_slot(&mut self, endpoint: EndpointId, key: String, channels: &FacadeChannels) {
        if self.generation.endpoint(endpoint).is_none() {
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

    fn slot_owner(&self, key: &str) -> Option<(GenerationLease, Endpoint)> {
        let endpoint = *self.slots.get(key)?.last_key_value()?.0;
        Some((self.lease(), self.generation.endpoint(endpoint)?.clone()))
    }

    fn current_slots(&self) -> Vec<SanitizedSlot> {
        let mut slots = self
            .slots
            .values()
            .filter_map(|owners| owners.last_key_value().map(|(_, slot)| slot.clone()))
            .collect::<Vec<_>>();
        slots.sort_by(|left, right| left.key.cmp(&right.key));
        slots
    }

    fn slot_keys(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

    fn publish_initial_slots(&mut self, pending: &PendingBridges, channels: &FacadeChannels) {
        for pending_endpoint in pending {
            for slot in &pending_endpoint.slots {
                self.record_slot(pending_endpoint.endpoint.id, slot.clone(), channels);
            }
        }
    }

    fn replace_generation(
        &mut self,
        next: Arc<Generation>,
        pending: &PendingBridges,
        channels: &FacadeChannels,
    ) -> Arc<Generation> {
        let old = std::mem::replace(&mut self.generation, next);
        let mut keys = self.slots.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        self.slots.clear();
        self.routes.clear();
        for key in keys {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
        self.publish_initial_slots(pending, channels);
        old
    }

    fn quiesce(&mut self, channels: &FacadeChannels) -> Arc<Generation> {
        self.stale = true;
        let mut keys = self.slots.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        self.slots.clear();
        self.routes.clear();
        for key in keys {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
        Arc::clone(&self.generation)
    }

    fn invalidate(&mut self, channels: &FacadeChannels) -> Option<Arc<Generation>> {
        if self.stale {
            return None;
        }
        Some(self.quiesce(channels))
    }

    fn begin_shutdown(&mut self, channels: &FacadeChannels) -> Option<Arc<Generation>> {
        if self.shutdown_done {
            return None;
        }
        self.shutdown_done = true;
        Some(self.quiesce(channels))
    }
}

struct FacadeChannels {
    tool_updates_tx: broadcast::Sender<ToolUpdate>,
    provider_events_tx: broadcast::Sender<ProviderEvent>,
    errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    ui_tx: broadcast::Sender<ExtensionUiEvent>,
    ui_requests_tx: mpsc::Sender<HostUiRequest>,
    ui_requests_rx: StdMutex<Option<mpsc::Receiver<HostUiRequest>>>,
    session_bridge_tx: mpsc::Sender<SessionBridgeEvent>,
    session_bridge_rx: StdMutex<Option<mpsc::Receiver<SessionBridgeEvent>>>,
}

impl FacadeChannels {
    fn new() -> Self {
        let (tool_updates_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (provider_events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (errors_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_requests_tx, ui_requests_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (session_bridge_tx, session_bridge_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            tool_updates_tx,
            provider_events_tx,
            errors_tx,
            ui_tx,
            ui_requests_tx,
            ui_requests_rx: StdMutex::new(Some(ui_requests_rx)),
            session_bridge_tx,
            session_bridge_rx: StdMutex::new(Some(session_bridge_rx)),
        }
    }

    fn publish_error(&self, code: &str, message: String, data: Option<Value>) {
        let _ = self.errors_tx.send(ExtensionErrorEvent {
            code: code.to_owned(),
            message,
            retryable: false,
            data,
        });
    }
}

/// Stable facade whose published endpoint generation can be replaced in place.
pub struct ExtensionRuntimeSet {
    state: Arc<StdMutex<PublishedRuntimeState>>,
    channels: Arc<FacadeChannels>,
    discovered_paths: Vec<String>,
    load_cwd: String,
    project_trusted: bool,
    reload_lock: tokio::sync::Mutex<()>,
    #[cfg(test)]
    test_prepared_reload: StdMutex<Option<TestPreparedReload>>,
}

#[cfg(test)]
enum TestPreparedReload {
    Replacement {
        generation: Generation,
        pending: PendingBridges,
    },
    ReplacementThenInvalidation {
        generation: Generation,
        pending: PendingBridges,
    },
}

impl ExtensionRuntimeSet {
    /// Classify and start all valid endpoint plans. Cold startup is best-effort.
    pub async fn start(
        discovered_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
    ) -> ExtensionSetStart {
        if discovered_paths.is_empty() {
            return ExtensionSetStart {
                set: None,
                diagnostics: Vec::new(),
            };
        }
        let (classified, mut diagnostics) = classify_paths(&discovered_paths);
        let plans = plan_endpoints(&classified);
        let (generation, pending, mut build_diagnostics) =
            build_generation(1, plans, &load_cwd, project_trusted, false).await;
        diagnostics.append(&mut build_diagnostics);
        let Some(generation) = generation else {
            return ExtensionSetStart {
                set: None,
                diagnostics,
            };
        };
        let set = Arc::new(Self::from_generation(
            generation,
            discovered_paths,
            load_cwd,
            project_trusted,
        ));
        set.install(pending);
        ExtensionSetStart {
            set: Some(set),
            diagnostics,
        }
    }

    fn from_generation(
        generation: Generation,
        discovered_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
    ) -> Self {
        Self {
            state: Arc::new(StdMutex::new(PublishedRuntimeState::new(Arc::new(
                generation,
            )))),
            channels: Arc::new(FacadeChannels::new()),
            discovered_paths,
            load_cwd,
            project_trusted,
            reload_lock: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            test_prepared_reload: StdMutex::new(None),
        }
    }

    /// Bind pre-built single-endpoint runners (focused fake-host tests).
    #[cfg(test)]
    pub(crate) fn bind(endpoints: Vec<(EndpointKind, Arc<HostExtensionRunner>)>) -> Arc<Self> {
        let endpoints = endpoints
            .into_iter()
            .enumerate()
            .map(|(index, (kind, runner))| (kind, format!("<test:{index}>"), runner))
            .collect::<Vec<_>>();
        let (generation, pending) = generation_from_endpoints(1, endpoints);
        let set = Arc::new(Self::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.install(pending);
        set
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PublishedRuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lease(&self) -> GenerationLease {
        self.state().lease()
    }

    #[cfg(test)]
    fn inject_prepared_replacement_for_reload(
        &self,
        generation: Generation,
        pending: PendingBridges,
    ) {
        let mut prepared = self
            .test_prepared_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            prepared.is_none(),
            "reload test preparation was already injected"
        );
        *prepared = Some(TestPreparedReload::Replacement {
            generation,
            pending,
        });
    }

    #[cfg(test)]
    fn inject_prepared_replacement_then_invalidation_for_reload(
        &self,
        generation: Generation,
        pending: PendingBridges,
    ) {
        let mut prepared = self
            .test_prepared_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            prepared.is_none(),
            "reload test preparation was already injected"
        );
        *prepared = Some(TestPreparedReload::ReplacementThenInvalidation {
            generation,
            pending,
        });
    }

    async fn build_reload_generation(
        &self,
        id: u64,
        plans: Vec<EndpointPlan>,
    ) -> (
        Option<Generation>,
        PendingBridges,
        Vec<ExtensionSetDiagnostic>,
    ) {
        #[cfg(test)]
        {
            let prepared = self
                .test_prepared_reload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(prepared) = prepared {
                return match prepared {
                    TestPreparedReload::Replacement {
                        generation,
                        pending,
                    } => (Some(generation), pending, Vec::new()),
                    TestPreparedReload::ReplacementThenInvalidation {
                        generation,
                        pending,
                    } => {
                        self.invalidate();
                        (Some(generation), pending, Vec::new())
                    }
                };
            }
        }
        build_generation(id, plans, &self.load_cwd, self.project_trusted, true).await
    }

    #[cfg(test)]
    async fn cutover(&self, next: Generation, pending: PendingBridges) {
        let _ = self.try_cutover(next, pending).await;
    }

    #[cfg(test)]
    async fn try_cutover(
        &self,
        next: Generation,
        pending: PendingBridges,
    ) -> Result<(), Arc<Generation>> {
        let next = Arc::new(next);
        let old = {
            let mut state = self.state();
            if state.stale {
                return Err(next);
            }
            state.replace_generation(Arc::clone(&next), &pending, &self.channels)
        };
        self.start_bridges(&next, pending);
        drain_leases(&old).await;
        for endpoint in old.endpoints.iter() {
            endpoint.runner.invalidate();
        }
        abort_bridges(&old);
        stop_generation(&old).await;
        Ok(())
    }

    fn install(&self, pending: PendingBridges) {
        let generation = {
            let mut state = self.state();
            state.publish_initial_slots(&pending, &self.channels);
            Arc::clone(&state.generation)
        };
        self.start_bridges(&generation, pending);
    }

    fn start_bridges(&self, generation: &Arc<Generation>, pending: PendingBridges) {
        let mut handles = Vec::new();
        for pending_endpoint in pending {
            let PendingEndpointBridges {
                endpoint,
                tool_updates,
                provider_events,
                errors,
                ui,
                ui_requests,
                session_bridge,
                slots: _,
            } = pending_endpoint;
            let context = EndpointRelayContext {
                state: Arc::downgrade(&self.state),
                channels: Arc::clone(&self.channels),
                endpoint,
            };
            handles.extend(spawn_broadcast_relays(
                &context,
                tool_updates,
                provider_events,
                errors,
            ));
            handles.extend(spawn_ui_relays(&context, ui, ui_requests));
            if let Some(handle) = spawn_session_relay(&context, session_bridge) {
                handles.push(handle);
            }
        }
        generation
            .bridges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(handles);
    }

    /// Registered flag types, first endpoint wins.
    #[must_use]
    pub fn registered_flag_types(&self) -> BTreeMap<String, ExtensionFlagType> {
        let lease = self.lease();
        let mut flags = BTreeMap::new();
        for endpoint in lease.endpoints() {
            for (name, kind) in endpoint.runner.registered_flag_types() {
                flags.entry(name).or_insert(kind);
            }
        }
        flags
    }

    /// Registered custom providers, first endpoint wins.
    #[must_use]
    pub fn providers(&self) -> HashMap<String, ExtensionProvider> {
        let lease = self.lease();
        let mut providers = HashMap::new();
        for endpoint in lease.endpoints() {
            for (name, provider) in endpoint.runner.providers() {
                providers.entry(name).or_insert(provider);
            }
        }
        providers
    }

    /// Register first-owned providers in endpoint order.
    #[must_use]
    pub fn register_providers_on(
        &self,
        runtime: &ModelRuntime,
    ) -> Vec<(String, Result<(), ModelRuntimeError>)> {
        let lease = self.lease();
        register_endpoint_providers(lease.endpoints(), runtime)
    }

    /// Remove each first-owned provider once.
    pub fn unregister_providers_from(&self, runtime: &ModelRuntime) {
        let lease = self.lease();
        unregister_endpoint_providers(lease.endpoints(), runtime);
    }

    /// Aggregate registry with existing first-wins semantics.
    #[must_use]
    pub fn registry(&self) -> Registry {
        let lease = self.lease();
        let mut aggregate = Registry::new();
        for endpoint in lease.endpoints() {
            let registry = endpoint.runner.registry();
            for item in registry.tools() {
                aggregate.register_tool(item.clone());
            }
            for item in registry.commands() {
                aggregate.register_command(item.clone());
            }
            for item in registry.shortcuts() {
                aggregate.register_shortcut(item.clone());
            }
            for item in registry.flags() {
                aggregate.register_flag(item.clone());
            }
            for item in registry.renderers() {
                aggregate.register_renderer(item.clone());
            }
            for item in registry.providers() {
                aggregate.register_provider(item.clone());
            }
        }
        aggregate
    }

    /// Raw shortcut registrations in endpoint order; product filtering remains last-wins.
    #[must_use]
    pub fn raw_shortcuts(&self) -> Vec<ShortcutRegistration> {
        let lease = self.lease();
        lease
            .endpoints()
            .iter()
            .flat_map(|endpoint| endpoint.runner.raw_shortcuts())
            .collect()
    }

    /// Current published generation id.
    #[must_use]
    pub fn reload_generation(&self) -> u64 {
        self.lease().generation.id
    }

    /// Whether any non-stale endpoint transport is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.lease().is_running()
    }

    /// Whether the published generation supports a live reload.
    #[must_use]
    pub(crate) fn can_reload(&self) -> bool {
        self.state().reloadable()
    }

    /// Synchronize flags to all active endpoints; siblings are attempted after an error.
    ///
    /// # Errors
    ///
    /// Returns the first active endpoint error after attempting every endpoint.
    pub async fn apply_flag_values(
        &self,
        values: &BTreeMap<String, FlagValueWire>,
    ) -> Result<(), HostClientError> {
        let lease = self.lease();
        let mut first_error = None;
        for endpoint in lease.endpoints() {
            if !endpoint.runner.is_active() {
                continue;
            }
            if let Err(error) = endpoint.runner.apply_flag_values(values).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Execute the last registration of a shortcut key.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning endpoint host rejects the shortcut.
    pub async fn execute_shortcut(
        &self,
        key: impl Into<String>,
    ) -> Result<ShortcutExecuteResponse, HostClientError> {
        let key = key.into();
        let lease = self.lease();
        for endpoint in lease.endpoints().iter().rev() {
            if endpoint
                .runner
                .raw_shortcuts()
                .iter()
                .any(|shortcut| shortcut.key == key)
            {
                return endpoint.runner.execute_shortcut(key).await;
            }
        }
        Ok(ShortcutExecuteResponse { handled: false })
    }

    /// Route a UI event to the endpoint currently owning its slot key.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning endpoint host rejects the event.
    pub async fn send_ui_event(
        &self,
        request: UiEventRequest,
    ) -> Result<UiEventResponse, HostClientError> {
        let Some((_lease, endpoint)) = self.state().slot_owner(&request.key) else {
            return Ok(UiEventResponse { delivered: false });
        };
        endpoint.runner.send_ui_event(request).await
    }

    /// Currently live effective slot keys.
    #[must_use]
    pub fn slot_keys(&self) -> Vec<String> {
        self.state().slot_keys()
    }

    /// Current last-owner slot per key, sorted by key.
    #[must_use]
    pub fn current_slots(&self) -> Vec<SanitizedSlot> {
        self.state().current_slots()
    }

    /// Subscribe to aggregate tool updates.
    #[must_use]
    pub fn subscribe_tool_updates(&self) -> broadcast::Receiver<ToolUpdate> {
        self.channels.tool_updates_tx.subscribe()
    }

    /// Subscribe to aggregate provider events.
    #[must_use]
    pub fn subscribe_provider_events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.channels.provider_events_tx.subscribe()
    }

    /// Subscribe to aggregate extension errors.
    #[must_use]
    pub fn subscribe_errors(&self) -> broadcast::Receiver<ExtensionErrorEvent> {
        self.channels.errors_tx.subscribe()
    }

    /// Whether any active endpoint handles terminal input.
    #[must_use]
    pub fn has_terminal_input_handlers(&self) -> bool {
        let lease = self.lease();
        lease.endpoints().iter().any(|endpoint| {
            endpoint.runner.is_active() && endpoint.runner.has_terminal_input_handlers()
        })
    }

    /// Fan the original data to all participants under one request-only deadline.
    ///
    /// # Errors
    ///
    /// Never returns `Err`; unavailable and late endpoint responses are represented in the result.
    pub async fn terminal_input(
        &self,
        data: &str,
    ) -> Result<protocol::TerminalInputResult, HostClientError> {
        let lease = self.lease();
        let mut pending = FuturesUnordered::new();
        for (index, endpoint) in lease.endpoints().iter().enumerate() {
            if endpoint.runner.is_active() && endpoint.runner.has_terminal_input_handlers() {
                let runner = Arc::clone(&endpoint.runner);
                let data = data.to_owned();
                pending.push(async move { (index, runner.terminal_input(&data).await) });
            }
        }
        if pending.is_empty() {
            return Ok(protocol::TerminalInputResult::default());
        }
        let deadline = tokio::time::Instant::now() + TERMINAL_INPUT_DEADLINE;
        let mut replies = vec![None; lease.endpoints().len()];
        while !pending.is_empty() {
            match tokio::time::timeout_at(deadline, pending.next()).await {
                Ok(Some((index, Ok(reply)))) => replies[index] = Some(reply),
                Ok(Some((_index, Err(_)))) => {}
                Ok(None) | Err(_) => break,
            }
        }
        if replies.iter().flatten().any(|reply| reply.consume) {
            return Ok(protocol::TerminalInputResult {
                consume: true,
                data: None,
            });
        }
        let rewrite = replies
            .into_iter()
            .flatten()
            .rfind(|reply| reply.data.as_deref().is_some_and(|rewrite| rewrite != data))
            .and_then(|reply| reply.data);
        Ok(protocol::TerminalInputResult {
            consume: false,
            data: rewrite,
        })
    }

    /// Subscribe to aggregate UI activity.
    #[must_use]
    pub fn subscribe_ui(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.channels.ui_tx.subscribe()
    }

    /// Highest endpoint theme generation.
    #[must_use]
    pub fn theme_generation(&self) -> u64 {
        let lease = self.lease();
        lease
            .endpoints()
            .iter()
            .map(|endpoint| endpoint.runner.theme_generation())
            .max()
            .unwrap_or(0)
    }

    /// Broadcast a theme update to all active endpoints.
    pub async fn push_theme_update(&self, update: &ThemeUpdate) {
        let lease = self.lease();
        for endpoint in lease.endpoints() {
            endpoint.runner.push_theme_update(update).await;
        }
    }

    /// Claim the persistent facade UI-request receiver once.
    #[must_use]
    pub fn take_ui_requests(&self) -> Option<mpsc::Receiver<HostUiRequest>> {
        self.channels
            .ui_requests_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Route a correlated UI response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub async fn respond_ui(&self, response: HostUiResponse) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(ui_response_id(&response))
        else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_ui(map_ui_response_id(response, local))
            .await
    }

    /// Whether this facade has at least one active endpoint.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.lease().is_active()
    }

    /// Claim the persistent facade session bridge once.
    #[must_use]
    pub fn take_session_bridge(&self) -> Option<mpsc::Receiver<SessionBridgeEvent>> {
        self.channels
            .session_bridge_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Route a correlated model response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub async fn respond_set_model(
        &self,
        id: FrameId,
        success: bool,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint.runner.respond_set_model(local, success).await
    }

    /// Route a correlated compaction response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub async fn respond_compact(
        &self,
        id: FrameId,
        outcome: Result<Value, String>,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint.runner.respond_compact(local, outcome).await
    }

    /// Broadcast mirrored session state.
    pub async fn push_session_state(&self, state: &SessionStateWire) {
        let lease = self.lease();
        for endpoint in lease.endpoints() {
            endpoint.runner.push_session_state(state).await;
        }
    }

    /// Broadcast mirrored UI state.
    pub async fn push_ui_state(&self, state: &UiStateWire) {
        let lease = self.lease();
        for endpoint in lease.endpoints() {
            endpoint.runner.push_ui_state(state).await;
        }
    }

    /// Render with the first endpoint owning the tool renderer.
    pub async fn render_extension_tool_html(
        &self,
        phase: ToolRenderPhase,
        tool_name: &str,
        payload: &Value,
    ) -> Option<String> {
        let lease = self.lease();
        for endpoint in lease.endpoints() {
            if endpoint
                .runner
                .registry()
                .renderers()
                .iter()
                .any(|renderer| renderer.kind == RendererKind::Tool && renderer.name == tool_name)
            {
                return endpoint
                    .runner
                    .render_extension_tool_html(phase, tool_name, payload)
                    .await;
            }
        }
        None
    }

    /// Build a complete replacement, preserve the facade, then reap the old generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime is no longer reloadable, a replacement endpoint fails to
    /// start, or a replacement provider cannot be registered.
    pub async fn restart_and_rewire(
        &self,
        runtime: &ModelRuntime,
        preserved_flags: HashMap<String, Value>,
    ) -> Result<(), HostStartError> {
        let _reload = self.reload_lock.lock().await;
        if !self.state().reloadable() {
            return Err(HostStartError::Load(
                "extension runtime is not reloadable".to_owned(),
            ));
        }
        let (classified, diagnostics) = classify_paths(&self.discovered_paths);
        if let Some(first) = diagnostics.first() {
            return Err(HostStartError::Load(format!(
                "{}: {}",
                first.path, first.message
            )));
        }
        let plans = plan_endpoints(&classified);
        let next_id = self
            .reload_generation()
            .checked_add(1)
            .ok_or_else(|| HostStartError::Load("extension generation exhausted".to_owned()))?;
        let (next, pending, diagnostics) = self.build_reload_generation(next_id, plans).await;
        let Some(next) = next else {
            let message = diagnostics.first().map_or_else(
                || "no extension endpoint started".to_owned(),
                |d| format!("{}: {}", d.path, d.message),
            );
            return Err(HostStartError::Load(message));
        };
        if let Some(first) = diagnostics.first() {
            stop_generation(&next).await;
            return Err(HostStartError::Load(format!(
                "{}: {}",
                first.path, first.message
            )));
        }
        if !next.is_single_compat_replacement() {
            stop_generation(&next).await;
            return Err(HostStartError::Load(
                "extension runtime is not reloadable".to_owned(),
            ));
        }
        let flags = match encode_flags(preserved_flags) {
            Ok(flags) => flags,
            Err(error) => {
                stop_generation(&next).await;
                return Err(error);
            }
        };
        if let Err(error) = apply_flags_to_generation(&next, &flags).await {
            stop_generation(&next).await;
            return Err(HostStartError::FlagSync(error.to_string()));
        }
        if let Err((path, error)) = validate_generation_providers(&next) {
            stop_generation(&next).await;
            return Err(HostStartError::Load(format!("{path}: {error}")));
        }
        let next = Arc::new(next);
        let old = {
            let mut state = self.state();
            if state.reloadable() {
                let old = Arc::clone(&state.generation);
                unregister_endpoint_providers(&old.endpoints, runtime);
                let registrations = register_endpoint_providers(&next.endpoints, runtime);
                if let Some((path, Err(error))) = registrations
                    .iter()
                    .find(|(_path, outcome)| outcome.is_err())
                {
                    unregister_endpoint_providers(&next.endpoints, runtime);
                    let _ = register_endpoint_providers(&old.endpoints, runtime);
                    Err(format!("{path}: {error}"))
                } else {
                    Ok(state.replace_generation(Arc::clone(&next), &pending, &self.channels))
                }
            } else {
                Err("extension runtime was invalidated during reload".to_owned())
            }
        };
        let old = match old {
            Ok(old) => old,
            Err(message) => {
                stop_generation(&next).await;
                return Err(HostStartError::Load(message));
            }
        };
        self.start_bridges(&next, pending);
        drain_leases(&old).await;
        for endpoint in old.endpoints.iter() {
            endpoint.runner.invalidate();
        }
        abort_bridges(&old);
        stop_generation(&old).await;
        Ok(())
    }

    /// Invalidate all endpoints and synchronously dispose product-visible slots.
    pub fn invalidate(&self) {
        let Some(generation) = self.state().invalidate(&self.channels) else {
            return;
        };
        for endpoint in generation.endpoints.iter() {
            endpoint.runner.invalidate();
        }
    }

    /// Gracefully stop every endpoint exactly once.
    pub async fn shutdown_once(&self) {
        let _reload = self.reload_lock.lock().await;
        let Some(generation) = self.state().begin_shutdown(&self.channels) else {
            return;
        };
        drain_leases(&generation).await;
        stop_generation(&generation).await;
        abort_bridges(&generation);
    }
}

impl ExtensionRunner for ExtensionRuntimeSet {
    fn has_handlers(&self, event: &str) -> bool {
        let lease = self.lease();
        lease.is_active()
            && lease
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.has_handlers(event))
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            for endpoint in lease.endpoints() {
                if let Some(result) = endpoint.runner.emit(event.clone()).await?
                    && result.cancel
                {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })
    }

    fn emit_message_update_delta<'a>(
        &'a self,
        event: &'a AssistantMessageEvent,
    ) -> BoxFuture<'a, Result<Option<CancelResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            for endpoint in lease.endpoints() {
                if let Some(result) = endpoint.runner.emit_message_update_delta(event).await?
                    && result.cancel
                {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })
    }

    fn emit_message_end(
        &self,
        message: AgentMessage,
    ) -> BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            let mut current = message;
            let mut changed = false;
            for endpoint in lease.endpoints() {
                if let Some(replacement) = endpoint.runner.emit_message_end(current.clone()).await?
                {
                    current = replacement;
                    changed = true;
                }
            }
            Ok(changed.then_some(current))
        })
    }

    fn emit_tool_call<'a>(
        &'a self,
        tool_name: &'a str,
        tool_call_id: &'a str,
        input: Map<String, Value>,
    ) -> BoxFuture<'a, Result<Option<BeforeToolCallResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            for endpoint in lease.endpoints() {
                if let Some(result) = endpoint
                    .runner
                    .emit_tool_call(tool_name, tool_call_id, input.clone())
                    .await?
                    && result.block
                {
                    return Ok(Some(result));
                }
            }
            Ok(None)
        })
    }

    fn emit_tool_result<'a>(
        &'a self,
        tool_name: &'a str,
        tool_call_id: &'a str,
        input: Map<String, Value>,
        content: Vec<ToolResultContent>,
        details: Value,
        is_error: bool,
    ) -> BoxFuture<'a, Result<Option<AfterToolCallResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            let mut current_content = content;
            let mut current_details = details;
            let mut current_error = is_error;
            let mut output = AfterToolCallResult::default();
            let mut changed = false;
            for endpoint in lease.endpoints() {
                if let Some(result) = endpoint
                    .runner
                    .emit_tool_result(
                        tool_name,
                        tool_call_id,
                        input.clone(),
                        current_content.clone(),
                        current_details.clone(),
                        current_error,
                    )
                    .await?
                {
                    if let Some(content) = result.content {
                        current_content.clone_from(&content);
                        output.content = Some(content);
                        changed = true;
                    }
                    if let Some(details) = result.details {
                        current_details.clone_from(&details);
                        output.details = Some(details);
                        changed = true;
                    }
                    if let Some(is_error) = result.is_error {
                        current_error = is_error;
                        output.is_error = Some(is_error);
                        changed = true;
                    }
                    if let Some(terminate) = result.terminate {
                        output.terminate = Some(terminate);
                        changed = true;
                    }
                }
            }
            Ok(changed.then_some(output))
        })
    }

    fn emit_input<'a>(
        &'a self,
        text: &'a str,
        images: Option<Value>,
        source: &'a str,
        streaming_behavior: Option<&'a str>,
    ) -> BoxFuture<'a, Result<InputTransformResult, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            let mut current_text = text.to_owned();
            let mut current_images = images;
            let mut text_changed = false;
            let mut images_changed = false;
            for endpoint in lease.endpoints() {
                let result = endpoint
                    .runner
                    .emit_input(
                        &current_text,
                        current_images.clone(),
                        source,
                        streaming_behavior,
                    )
                    .await?;
                if result.handled {
                    return Ok(result);
                }
                if let Some(text) = result.text {
                    current_text = text;
                    text_changed = true;
                }
                if let Some(images) = result.images {
                    current_images = Some(images);
                    images_changed = true;
                }
            }
            Ok(InputTransformResult {
                handled: false,
                text: text_changed.then_some(current_text),
                images: images_changed.then_some(current_images).flatten(),
            })
        })
    }

    fn emit_before_agent_start<'a>(
        &'a self,
        prompt: &'a str,
        images: Option<Value>,
    ) -> BoxFuture<'a, Result<Option<BeforeAgentStartResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            let mut messages = Vec::new();
            let mut system_prompt = None;
            let mut changed = false;
            for endpoint in lease.endpoints() {
                if let Some(result) = endpoint
                    .runner
                    .emit_before_agent_start(prompt, images.clone())
                    .await?
                {
                    changed = true;
                    messages.extend(result.messages);
                    if system_prompt.is_none() {
                        system_prompt = result.system_prompt;
                    }
                }
            }
            Ok(changed.then_some(BeforeAgentStartResult {
                messages,
                system_prompt,
            }))
        })
    }

    fn emit_resources_discover<'a>(
        &'a self,
        cwd: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<ResourceExtensionPaths, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            let mut aggregate = ResourceExtensionPaths::default();
            for endpoint in lease.endpoints() {
                let paths = endpoint.runner.emit_resources_discover(cwd, reason).await?;
                aggregate.skill_paths.extend(paths.skill_paths);
                aggregate.prompt_paths.extend(paths.prompt_paths);
                aggregate.theme_paths.extend(paths.theme_paths);
            }
            Ok(aggregate)
        })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        let lease = self.lease();
        let mut seen = HashSet::new();
        let mut commands = Vec::new();
        for endpoint in lease.endpoints() {
            for command in endpoint.runner.get_registered_commands() {
                if seen.insert(command.clone()) {
                    commands.push(command);
                }
            }
        }
        commands
    }

    fn execute_command<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<bool, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            for endpoint in lease.endpoints() {
                if endpoint
                    .runner
                    .registry()
                    .commands()
                    .iter()
                    .any(|command| command.name == name)
                {
                    return endpoint.runner.execute_command(name, args).await;
                }
            }
            Ok(false)
        })
    }

    fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>> {
        let lease = self.lease();
        let mut tools = HashMap::new();
        for endpoint in lease.endpoints() {
            for (name, tool) in endpoint.runner.get_all_registered_tools() {
                tools.entry(name).or_insert(tool);
            }
        }
        tools
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        let lease = self.lease();
        let mut flags = HashMap::new();
        for endpoint in lease.endpoints() {
            for (name, value) in endpoint.runner.get_flag_values() {
                flags.entry(name).or_insert(value);
            }
        }
        flags
    }

    fn invalidate(&self) {
        Self::invalidate(self);
    }

    fn emit_error(&self, message: String) {
        self.channels
            .publish_error("extension_error", message, None);
    }
}

fn classify_paths(
    discovered_paths: &[String],
) -> (Vec<ClassifiedExtension>, Vec<ExtensionSetDiagnostic>) {
    let mut classified = Vec::new();
    let mut diagnostics = Vec::new();
    for path in discovered_paths {
        match classify(path) {
            Ok(extension) => classified.push(extension),
            Err(error) => diagnostics.push(ExtensionSetDiagnostic {
                path: path.clone(),
                message: error.to_string(),
            }),
        }
    }
    (classified, diagnostics)
}

fn plan_endpoints(classified: &[ClassifiedExtension]) -> Vec<EndpointPlan> {
    let mut plans: Vec<EndpointPlan> = Vec::new();
    for extension in classified {
        let kind = match extension.runtime {
            ExtensionRuntime::TsCompat => EndpointKind::TsCompat,
            ExtensionRuntime::Native => EndpointKind::Native,
        };
        if kind != EndpointKind::Native
            && let Some(last) = plans.last_mut()
            && last.kind == kind
        {
            last.entries.push(extension.entry.clone());
            last.diagnostic_paths.push(extension.discovered.clone());
            continue;
        }
        plans.push(EndpointPlan {
            position: 0,
            kind,
            entries: vec![extension.entry.clone()],
            diagnostic_paths: vec![extension.discovered.clone()],
            builtins: false,
            label: extension.discovered.clone(),
        });
    }
    if plans.is_empty() {
        return plans;
    }
    if plans[0].kind == EndpointKind::TsCompat {
        plans[0].builtins = true;
    } else {
        plans.insert(
            0,
            EndpointPlan {
                position: 0,
                kind: EndpointKind::TsCompat,
                entries: Vec::new(),
                diagnostic_paths: Vec::new(),
                builtins: true,
                label: "<builtins>".to_owned(),
            },
        );
    }
    for (position, plan) in plans.iter_mut().enumerate() {
        plan.position = position;
    }
    plans
}

async fn build_generation(
    id: u64,
    plans: Vec<EndpointPlan>,
    load_cwd: &str,
    project_trusted: bool,
    all_or_nothing: bool,
) -> (
    Option<Generation>,
    PendingBridges,
    Vec<ExtensionSetDiagnostic>,
) {
    let needs_typescript = plans.iter().any(|plan| plan.kind != EndpointKind::Native);
    let ts_spec = needs_typescript.then(host::resolve_host);
    let mut starts = FuturesUnordered::new();
    for plan in plans {
        let spec = match plan.kind {
            EndpointKind::Native => plan.entries.first().map(PathBuf::from).map_or_else(
                || Err("native endpoint plan is missing its executable".to_owned()),
                |program| {
                    Ok(HostSpec {
                        source: HostSource::NativeExtension(program.clone()),
                        program,
                        args: Vec::new(),
                    })
                },
            ),
            EndpointKind::TsCompat => match &ts_spec {
                Some(Ok(spec)) => {
                    let mut spec = spec.clone();
                    if !plan.builtins {
                        spec.args.push("--no-builtins".to_owned());
                    }
                    Ok(spec)
                }
                Some(Err(error)) => Err(error.to_string()),
                None => Err("compatibility endpoint plan has no resolved host".to_owned()),
            },
        };
        let cwd = load_cwd.to_owned();
        starts.push(async move {
            let position = plan.position;
            let result = match spec {
                Ok(spec) => start_endpoint(plan.clone(), spec, cwd, project_trusted).await,
                Err(message) => Err(message),
            };
            (position, plan, result)
        });
    }

    let mut results = Vec::new();
    while let Some(result) = starts.next().await {
        results.push(result);
    }
    results.sort_by_key(|(position, _, _)| *position);

    let mut endpoints = Vec::new();
    let mut diagnostics = Vec::new();
    for (_, plan, result) in results {
        match result {
            Ok(runner) => {
                for (path, message) in runner.load_errors() {
                    let path = plan
                        .entries
                        .iter()
                        .position(|entry| entry == &path)
                        .and_then(|index| plan.diagnostic_paths.get(index))
                        .cloned()
                        .unwrap_or(path);
                    diagnostics.push(ExtensionSetDiagnostic { path, message });
                }
                endpoints.push((plan.kind, plan.label, runner));
            }
            Err(message) => {
                let paths = if plan.diagnostic_paths.is_empty() {
                    vec![plan.label]
                } else {
                    plan.diagnostic_paths
                };
                diagnostics.extend(paths.into_iter().map(|path| ExtensionSetDiagnostic {
                    path,
                    message: message.clone(),
                }));
            }
        }
    }

    if all_or_nothing && !diagnostics.is_empty() {
        for (_, _, endpoint) in &endpoints {
            endpoint.shutdown_once().await;
        }
        return (None, Vec::new(), diagnostics);
    }
    if endpoints.is_empty() {
        return (None, Vec::new(), diagnostics);
    }
    let (generation, pending) = generation_from_endpoints(id, endpoints);
    (Some(generation), pending, diagnostics)
}

async fn start_endpoint(
    plan: EndpointPlan,
    spec: HostSpec,
    load_cwd: String,
    project_trusted: bool,
) -> Result<Arc<HostExtensionRunner>, String> {
    let client = Arc::new(HostClient::spawn(&spec).map_err(|error| error.to_string())?);
    let result = HostExtensionRunner::connect_with_cwd_and_trust(
        Arc::clone(&client),
        plan.entries,
        load_cwd,
        project_trusted,
        HOOK_TIMEOUT,
    )
    .await;
    if result.is_err() {
        let _ = client.shutdown().await;
    }
    result.map_err(|error| error.to_string())
}

fn generation_from_endpoints(
    id: u64,
    endpoints: Vec<(EndpointKind, String, Arc<HostExtensionRunner>)>,
) -> (Generation, PendingBridges) {
    let endpoints = endpoints
        .into_iter()
        .enumerate()
        .map(|(position, (kind, label, runner))| Endpoint {
            id: EndpointId {
                generation: id,
                position,
            },
            kind,
            label,
            runner,
        })
        .collect::<Vec<_>>();
    let pending = endpoints
        .iter()
        .cloned()
        .map(|endpoint| PendingEndpointBridges {
            tool_updates: endpoint.runner.subscribe_tool_updates(),
            provider_events: endpoint.runner.subscribe_provider_events(),
            errors: endpoint.runner.subscribe_errors(),
            ui: endpoint.runner.subscribe_ui(),
            ui_requests: endpoint.runner.take_ui_requests(),
            session_bridge: endpoint.runner.take_session_bridge(),
            slots: endpoint.runner.current_slots(),
            endpoint,
        })
        .collect();
    (
        Generation {
            id,
            endpoints: endpoints.into(),
            bridges: StdMutex::new(Vec::new()),
            leases: AtomicUsize::new(0),
            drained: tokio::sync::Notify::new(),
        },
        pending,
    )
}

fn spawn_broadcast_relay<T, F>(
    state: Weak<StdMutex<PublishedRuntimeState>>,
    endpoint: EndpointId,
    channels: Arc<FacadeChannels>,
    label: String,
    mut receiver: broadcast::Receiver<T>,
    publish: F,
) -> tokio::task::JoinHandle<()>
where
    T: Clone + Send + 'static,
    F: Fn(&mut PublishedRuntimeState, &FacadeChannels, T) + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(item) => {
                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    let mut state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.generation.endpoint(endpoint).is_some() {
                        publish(&mut state, &channels, item);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    let state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.generation.endpoint(endpoint).is_some() {
                        channels.publish_error(
                            "extension_event_lagged",
                            format!("extension {label:?} relay lagged by {count} events"),
                            None,
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Spawn bounded broadcast relays that only publish their own current generation.
fn spawn_broadcast_relays(
    context: &EndpointRelayContext,
    tool_updates: broadcast::Receiver<ToolUpdate>,
    provider_events: broadcast::Receiver<ProviderEvent>,
    errors: broadcast::Receiver<ExtensionErrorEvent>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let state = &context.state;
    let channels = &context.channels;
    let label = &context.endpoint.label;
    let endpoint = context.endpoint.id;
    let error_runner = Arc::clone(&context.endpoint.runner);
    vec![
        spawn_broadcast_relay(
            Weak::clone(state),
            endpoint,
            Arc::clone(channels),
            label.clone(),
            tool_updates,
            |_state, channels, item| {
                let _ = channels.tool_updates_tx.send(item);
            },
        ),
        spawn_broadcast_relay(
            Weak::clone(state),
            endpoint,
            Arc::clone(channels),
            label.clone(),
            provider_events,
            |_state, channels, item| {
                let _ = channels.provider_events_tx.send(item);
            },
        ),
        spawn_broadcast_relay(
            Weak::clone(state),
            endpoint,
            Arc::clone(channels),
            label.clone(),
            errors,
            move |state, channels, item| {
                if !error_runner.is_active() {
                    state.routes.retain(|_, route| route.endpoint != endpoint);
                }
                let _ = channels.errors_tx.send(item);
            },
        ),
    ]
}

/// Spawn UI relays that preserve generation filtering and release failed request routes.
fn spawn_ui_relays(
    context: &EndpointRelayContext,
    mut ui: broadcast::Receiver<ExtensionUiEvent>,
    ui_requests: Option<mpsc::Receiver<HostUiRequest>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let ui_state = Weak::clone(&context.state);
    let ui_channels = Arc::clone(&context.channels);
    let ui_label = context.endpoint.label.clone();
    let endpoint = context.endpoint.id;
    let mut handles = vec![tokio::spawn(async move {
        loop {
            match ui.recv().await {
                Ok(event) => {
                    let Some(state) = ui_state.upgrade() else {
                        break;
                    };
                    let mut state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match event {
                        ExtensionUiEvent::Slot(slot) => {
                            state.record_slot(endpoint, slot, &ui_channels);
                        }
                        ExtensionUiEvent::Dispose { key } => {
                            state.dispose_slot(endpoint, key, &ui_channels);
                        }
                        other if state.generation.endpoint(endpoint).is_some() => {
                            let _ = ui_channels.ui_tx.send(other);
                        }
                        _ => {}
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    let Some(state) = ui_state.upgrade() else {
                        break;
                    };
                    let state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.generation.endpoint(endpoint).is_some() {
                        ui_channels.publish_error(
                            "extension_event_lagged",
                            format!("extension {ui_label:?} UI relay lagged by {count} events"),
                            None,
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })];

    if let Some(mut requests) = ui_requests {
        let request_state = Weak::clone(&context.state);
        let request_channels = Arc::clone(&context.channels);
        let runner = Arc::clone(&context.endpoint.runner);
        handles.push(tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let fallback = request.clone();
                let Some(state) = request_state.upgrade() else {
                    break;
                };
                let send_failed = {
                    let mut state = state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state
                        .allocate_route(endpoint, request.id())
                        .map(|routed_id| {
                            let routed = map_ui_request_id(request, routed_id);
                            let failed = request_channels.ui_requests_tx.try_send(routed).is_err();
                            if failed {
                                state.release_route(routed_id);
                            }
                            failed
                        })
                };
                if send_failed.unwrap_or(true) {
                    let _ = runner.respond_ui(default_ui_response(&fallback)).await;
                }
            }
        }));
    }
    handles
}

/// Spawn a session relay that cleans every failed correlated route before replying locally.
fn spawn_session_relay(
    context: &EndpointRelayContext,
    session_bridge: Option<mpsc::Receiver<SessionBridgeEvent>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut session = session_bridge?;
    let state = Weak::clone(&context.state);
    let channels = Arc::clone(&context.channels);
    let runner = Arc::clone(&context.endpoint.runner);
    let endpoint = context.endpoint.id;
    Some(tokio::spawn(async move {
        while let Some(event) = session.recv().await {
            let fallback = event.clone();
            let Some(state) = state.upgrade() else {
                break;
            };
            let send_failed = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.generation.endpoint(endpoint).is_none() {
                    None
                } else {
                    let (routed, route_id) = match event {
                        SessionBridgeEvent::SetModel { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::SetModel { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::Compact { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::Compact { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::Command(command) => {
                            (Some(SessionBridgeEvent::Command(command)), None)
                        }
                    };
                    routed.map(|routed| {
                        let failed = channels.session_bridge_tx.try_send(routed).is_err();
                        if failed && let Some(route_id) = route_id {
                            state.release_route(route_id);
                        }
                        failed
                    })
                }
            };
            if send_failed.unwrap_or(true) {
                answer_unclaimed_session(&runner, fallback).await;
            }
        }
    }))
}

async fn drain_leases(generation: &Generation) {
    while generation.leases.load(Ordering::Acquire) != 0 {
        generation.drained.notified().await;
    }
}

async fn stop_generation(generation: &Generation) {
    for endpoint in generation.endpoints.iter() {
        endpoint.runner.shutdown_once().await;
    }
}

fn abort_bridges(generation: &Generation) {
    let handles = std::mem::take(
        &mut *generation
            .bridges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for handle in handles {
        handle.abort();
    }
}

fn validate_generation_providers(
    generation: &Generation,
) -> Result<(), (String, ModelRuntimeError)> {
    let mut seen = HashSet::new();
    for endpoint in generation.endpoints.iter() {
        let paths = endpoint.runner.provider_extension_paths();
        for (name, config) in endpoint.runner.provider_configs() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let path = paths.get(&name).cloned().unwrap_or_else(|| name.clone());
            ModelRuntime::validate_provider_registration(&name, &config)
                .map_err(|error| (path, error))?;
        }
    }
    Ok(())
}

fn register_endpoint_providers(
    endpoints: &[Endpoint],
    runtime: &ModelRuntime,
) -> Vec<(String, Result<(), ModelRuntimeError>)> {
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for endpoint in endpoints {
        let configs = endpoint.runner.provider_configs();
        let stream_ids = endpoint.runner.stream_provider_ids();
        let paths = endpoint.runner.provider_extension_paths();
        let mut adapters = endpoint.runner.providers();
        for (name, config) in configs {
            if !seen.insert(name.clone()) {
                continue;
            }
            let path = paths.get(&name).cloned().unwrap_or_else(|| name.clone());
            let outcome = runtime.register_provider(&name, config);
            if outcome.is_ok()
                && stream_ids.contains(&name)
                && let Some(adapter) = adapters.remove(&name)
            {
                runtime.register_extension_stream_provider(name, Arc::new(adapter));
            }
            results.push((path, outcome));
        }
    }
    results
}

fn unregister_endpoint_providers(endpoints: &[Endpoint], runtime: &ModelRuntime) {
    let mut seen = HashSet::new();
    for endpoint in endpoints {
        for name in endpoint.runner.provider_configs().into_keys() {
            if seen.insert(name.clone()) {
                runtime.unregister_provider(&name);
            }
        }
    }
}

async fn apply_flags_to_generation(
    generation: &Generation,
    flags: &BTreeMap<String, FlagValueWire>,
) -> Result<(), HostClientError> {
    let mut first_error = None;
    for endpoint in generation.endpoints.iter() {
        if let Err(error) = endpoint.runner.apply_flag_values(flags).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn encode_flags(
    values: HashMap<String, Value>,
) -> Result<BTreeMap<String, FlagValueWire>, HostStartError> {
    values
        .into_iter()
        .map(|(name, value)| {
            let value = match value {
                Value::Bool(value) => FlagValueWire::Boolean(value),
                Value::String(value) => FlagValueWire::String(value),
                other => {
                    return Err(HostStartError::FlagSync(format!(
                        "flag {name:?} has unsupported value {other}"
                    )));
                }
            };
            Ok((name, value))
        })
        .collect()
}

fn map_ui_request_id(request: HostUiRequest, id: FrameId) -> HostUiRequest {
    match request {
        HostUiRequest::Select { request, .. } => HostUiRequest::Select { id, request },
        HostUiRequest::Confirm { request, .. } => HostUiRequest::Confirm { id, request },
        HostUiRequest::Input { request, .. } => HostUiRequest::Input { id, request },
        HostUiRequest::Editor { request, .. } => HostUiRequest::Editor { id, request },
    }
}

fn ui_response_id(response: &HostUiResponse) -> FrameId {
    match response {
        HostUiResponse::Select { id, .. }
        | HostUiResponse::Confirm { id, .. }
        | HostUiResponse::Input { id, .. }
        | HostUiResponse::Editor { id, .. } => *id,
    }
}

fn map_ui_response_id(response: HostUiResponse, id: FrameId) -> HostUiResponse {
    match response {
        HostUiResponse::Select { value, .. } => HostUiResponse::Select { id, value },
        HostUiResponse::Confirm { confirmed, .. } => HostUiResponse::Confirm { id, confirmed },
        HostUiResponse::Input { value, .. } => HostUiResponse::Input { id, value },
        HostUiResponse::Editor { value, .. } => HostUiResponse::Editor { id, value },
    }
}

async fn answer_unclaimed_session(runner: &HostExtensionRunner, event: SessionBridgeEvent) {
    match event {
        SessionBridgeEvent::SetModel { id, .. } => {
            let _ = runner.respond_set_model(id, false).await;
        }
        SessionBridgeEvent::Compact { id, .. } => {
            let _ = runner
                .respond_compact(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::Command(_) => {}
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::error::Error;
    use std::sync::atomic::AtomicUsize;

    use pi_ext::protocol::{Frame, FrameKind, HelloAck};
    use pi_ext::sanitize::{SanitizedRun, SanitizedSlot};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    const TEST_TIMEOUT: Duration = Duration::from_millis(500);

    enum FakeCommand {
        Emit(Frame),
        Close,
    }

    struct FakeHostState {
        frames: StdMutex<Vec<Frame>>,
        exits: AtomicUsize,
        parked_methods: StdMutex<HashMap<String, mpsc::Sender<FrameId>>>,
    }

    #[derive(Clone)]
    pub(crate) struct FakeHost {
        commands: mpsc::Sender<FakeCommand>,
        responses: Arc<StdMutex<HashMap<String, Value>>>,
        dropped_methods: Arc<StdMutex<HashSet<String>>>,
        state: Arc<FakeHostState>,
    }

    impl FakeHost {
        fn set_response(&self, method: &str, payload: Value) {
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(method.to_owned(), payload);
        }

        fn drop_method(&self, method: &str) {
            self.dropped_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(method.to_owned());
        }

        fn park_method(&self, method: &str) -> mpsc::Receiver<FrameId> {
            let (tx, rx) = mpsc::channel(1);
            self.state
                .parked_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(method.to_owned(), tx);
            rx
        }

        async fn emit(&self, frame: Frame) {
            let _ = self.commands.send(FakeCommand::Emit(frame)).await;
        }

        async fn close(&self) {
            let _ = self.commands.send(FakeCommand::Close).await;
        }

        pub(crate) async fn wait_for_request(&self, method: &str) -> TestResult {
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if self
                        .state
                        .frames
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .any(|frame| frame.kind == FrameKind::Req && frame.method == method)
                    {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .map_err(|_| format!("fake host did not receive {method}"))?;
            Ok(())
        }

        async fn wait_for_response(&self, method: &str, id: FrameId) -> TestResult {
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if self
                        .state
                        .frames
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .any(|frame| {
                            frame.kind == FrameKind::Res && frame.method == method && frame.id == id
                        })
                    {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .map_err(|_| format!("fake host did not receive {method} response {id}"))?;
            Ok(())
        }

        pub(crate) fn request_count(&self, method: &str) -> usize {
            self.state
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|frame| frame.kind == FrameKind::Req && frame.method == method)
                .count()
        }

        async fn wait_for_exit(&self) -> TestResult {
            tokio::time::timeout(TEST_TIMEOUT, async {
                while self.state.exits.load(Ordering::Acquire) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .map_err(|_| "fake host did not observe transport EOF")?;
            Ok(())
        }

        fn exit_count(&self) -> usize {
            self.state.exits.load(Ordering::Acquire)
        }

        fn response_payload(&self, method: &str, id: FrameId) -> Option<Value> {
            self.state
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|frame| {
                    frame.kind == FrameKind::Res && frame.method == method && frame.id == id
                })
                .map(|frame| frame.payload.clone())
        }
    }

    pub(crate) async fn make_runner(
        snapshot: Value,
    ) -> TestResult<(Arc<HostExtensionRunner>, FakeHost)> {
        make_runner_with_hook_timeout(snapshot, Duration::from_millis(200)).await
    }

    async fn make_runner_with_hook_timeout(
        snapshot: Value,
        hook_timeout: Duration,
    ) -> TestResult<(Arc<HostExtensionRunner>, FakeHost)> {
        let (client_to_host, host_read) = tokio::io::duplex(64 * 1024);
        let (host_write, client_read) = tokio::io::duplex(64 * 1024);
        let (error_write, _error_read) = tokio::io::duplex(4096);
        let client = Arc::new(HostClient::connect_boxed(
            Box::new(client_to_host),
            Box::new(client_read),
            Box::new(error_write),
            None,
        ));
        let responses = Arc::new(StdMutex::new(HashMap::new()));
        let dropped_methods = Arc::new(StdMutex::new(HashSet::new()));
        let state = Arc::new(FakeHostState {
            frames: StdMutex::new(Vec::new()),
            exits: AtomicUsize::new(0),
            parked_methods: StdMutex::new(HashMap::new()),
        });
        let (commands, command_rx) = mpsc::channel(32);
        tokio::spawn(fake_host_task(
            host_read,
            host_write,
            snapshot,
            Arc::clone(&responses),
            Arc::clone(&dropped_methods),
            Arc::clone(&state),
            command_rx,
        ));
        let runner = HostExtensionRunner::connect_with_cwd_and_trust(
            client,
            Vec::new(),
            "/workspace",
            false,
            hook_timeout,
        )
        .await?;
        Ok((
            runner,
            FakeHost {
                commands,
                responses,
                dropped_methods,
                state,
            },
        ))
    }

    async fn fake_host_task(
        read: tokio::io::DuplexStream,
        mut write: tokio::io::DuplexStream,
        snapshot: Value,
        responses: Arc<StdMutex<HashMap<String, Value>>>,
        dropped_methods: Arc<StdMutex<HashSet<String>>>,
        state: Arc<FakeHostState>,
        mut commands: mpsc::Receiver<FakeCommand>,
    ) {
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            tokio::select! {
                biased;
                command = commands.recv() => match command {
                    Some(FakeCommand::Emit(frame)) => {
                        if let Ok(bytes) = pi_ext::protocol::encode_frame(&frame) {
                            let _ = write.write_all(&bytes).await;
                            let _ = write.flush().await;
                        }
                    }
                    Some(FakeCommand::Close) => {
                        let _ = write.shutdown().await;
                        break;
                    }
                    None => break,
                },
                received = reader.read_line(&mut line) => match received {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(frame) = pi_ext::protocol::decode_frame_str(&line) {
                            state
                                .frames
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(frame.clone());
                            if frame.kind == FrameKind::Req {
                                let parked = state
                                    .parked_methods
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .get(&frame.method)
                                    .cloned();
                                if let Some(sender) = parked {
                                    let _ = sender.try_send(frame.id);
                                    line.clear();
                                    continue;
                                }
                            }
                            if frame.kind == FrameKind::Req
                                && !dropped_methods
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .contains(&frame.method)
                            {
                                let payload = if frame.method == "hello" {
                                    serde_json::to_value(HelloAck::local()).unwrap_or(Value::Null)
                                } else if frame.method == "extensions.load" {
                                    snapshot.clone()
                                } else {
                                    responses
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .get(&frame.method)
                                        .cloned()
                                        .unwrap_or_else(|| json!({}))
                                };
                                let response = Frame {
                                    id: frame.id,
                                    kind: FrameKind::Res,
                                    method: frame.method,
                                    payload,
                                };
                                if let Ok(bytes) = pi_ext::protocol::encode_frame(&response) {
                                    let _ = write.write_all(&bytes).await;
                                    let _ = write.flush().await;
                                }
                            }
                        }
                        line.clear();
                    }
                },
            }
        }
        state.exits.fetch_add(1, Ordering::Release);
    }

    fn snapshot(handlers: &[&str]) -> Value {
        json!({
            "tools": [],
            "commands": [],
            "shortcuts": [],
            "renderers": [],
            "handlers": handlers,
            "terminalInput": handlers.contains(&"terminalInput"),
        })
    }

    #[cfg(unix)]
    fn write_native_snapshot_host(directory: &std::path::Path, snapshot: Value) -> TestResult {
        let executable = directory.join("replacement");
        let hello = String::from_utf8(pi_ext::protocol::encode_frame(&Frame {
            id: 1,
            kind: FrameKind::Res,
            method: "hello".to_owned(),
            payload: serde_json::to_value(HelloAck::local())?,
        })?)?;
        let load = String::from_utf8(pi_ext::protocol::encode_frame(&Frame {
            id: 2,
            kind: FrameKind::Res,
            method: "extensions.load".to_owned(),
            payload: snapshot,
        })?)?;
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 IFS= read -r request || exit 10\n\
                 printf '%s' '{hello}'\n\
                 IFS= read -r request || exit 11\n\
                 printf '%s' '{load}'\n\
                 while IFS= read -r request; do :; done\n"
            ),
        )?;
        let mut permissions = std::fs::metadata(&executable)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(executable, permissions)?;
        Ok(())
    }

    fn slot_frame(key: &str, text: &str) -> Frame {
        Frame {
            id: 0,
            kind: FrameKind::Event,
            method: "uiSlot".to_owned(),
            payload: json!({
                "key": key,
                "generation": 1,
                "placement": "aboveEditor",
                "height": 1,
                "runs": [[{"text": text}]],
                "focusable": false,
            }),
        }
    }

    async fn wait_for_slot_text(set: &ExtensionRuntimeSet, text: &str) -> TestResult {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if set.current_slots().first().is_some_and(|slot| {
                    slot.lines
                        .first()
                        .and_then(|line| line.first())
                        .is_some_and(|run| run.text == text)
                }) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| format!("slot did not become {text}"))?;
        Ok(())
    }

    async fn wait_for_dispose(
        receiver: &mut broadcast::Receiver<ExtensionUiEvent>,
        key: &str,
    ) -> TestResult {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if let ExtensionUiEvent::Dispose { key: disposed } = receiver.recv().await?
                    && disposed == key
                {
                    return Ok::<_, broadcast::error::RecvError>(());
                }
            }
        })
        .await??;
        Ok(())
    }

    fn classified(runtime: ExtensionRuntime, entry: &str) -> ClassifiedExtension {
        ClassifiedExtension {
            runtime,
            discovered: entry.to_owned(),
            entry: entry.to_owned(),
        }
    }

    #[test]
    fn manifestless_compat_entries_stay_one_builtin_endpoint() {
        let plans = plan_endpoints(&[
            classified(ExtensionRuntime::TsCompat, "a.ts"),
            classified(ExtensionRuntime::TsCompat, "b.ts"),
        ]);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].builtins);
        assert_eq!(plans[0].entries, ["a.ts", "b.ts"]);
    }

    #[test]
    fn compat_groups_separated_by_native_entries_preserve_order() {
        let plans = plan_endpoints(&[
            classified(ExtensionRuntime::TsCompat, "a.ts"),
            classified(ExtensionRuntime::TsCompat, "b.ts"),
            classified(ExtensionRuntime::Native, "n1"),
            classified(ExtensionRuntime::Native, "n2"),
            classified(ExtensionRuntime::TsCompat, "c.ts"),
        ]);
        assert_eq!(plans.len(), 4);
        assert_eq!(plans[0].entries, ["a.ts", "b.ts"]);
        assert_eq!(plans[1].entries, ["n1"]);
        assert_eq!(plans[2].entries, ["n2"]);
        assert_eq!(plans[3].entries, ["c.ts"]);
        assert_eq!(plans.iter().filter(|plan| plan.builtins).count(), 1);
    }

    #[test]
    fn native_first_gets_exactly_one_builtin_compat_prefix() {
        let plans = plan_endpoints(&[
            classified(ExtensionRuntime::Native, "native"),
            classified(ExtensionRuntime::TsCompat, "plugin.ts"),
        ]);
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].label, "<builtins>");
        assert!(plans[0].entries.is_empty());
        assert!(plans[0].builtins);
        assert_eq!(plans.iter().filter(|plan| plan.builtins).count(), 1);
        assert!(!plans[2].builtins);
    }

    #[test]
    fn empty_discovery_does_not_start_a_builtin_host() {
        assert!(plan_endpoints(&[]).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_endpoint_starts_direct_jsonl_executable() -> TestResult {
        const COMMAND: &str = "native-spawn-guard-direct-command";

        let directory = tempfile::tempdir()?;
        let executable = directory.path().join("native-jsonl-host");
        let hello = String::from_utf8(pi_ext::protocol::encode_frame(&Frame {
            id: 1,
            kind: FrameKind::Res,
            method: "hello".to_owned(),
            payload: serde_json::to_value(HelloAck::local())?,
        })?)?;
        let load = String::from_utf8(pi_ext::protocol::encode_frame(&Frame {
            id: 2,
            kind: FrameKind::Res,
            method: "extensions.load".to_owned(),
            payload: json!({
                "commands": [{"name": COMMAND}],
            }),
        })?)?;
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 IFS= read -r request || exit 10\n\
                 printf '%s' '{hello}'\n\
                 IFS= read -r request || exit 11\n\
                 printf '%s' '{load}'\n\
                 while IFS= read -r request; do :; done\n"
            ),
        )?;
        let mut permissions = std::fs::metadata(&executable)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&executable, permissions)?;

        let executable_path = executable.to_string_lossy().into_owned();
        let load_cwd = directory.path().to_string_lossy().into_owned();
        let (generation, pending, diagnostics) = build_generation(
            1,
            vec![EndpointPlan {
                position: 0,
                kind: EndpointKind::Native,
                entries: vec![executable_path.clone()],
                diagnostic_paths: vec![executable_path.clone()],
                builtins: false,
                label: executable_path,
            }],
            &load_cwd,
            false,
            false,
        )
        .await;
        let generation = generation.ok_or("native endpoint did not start")?;
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            load_cwd,
            false,
        ));
        set.install(pending);

        let lease = set.lease();
        let endpoint_count = lease.endpoints().len();
        let command_registered = lease.endpoints().first().is_some_and(|endpoint| {
            endpoint
                .runner
                .registry()
                .commands()
                .iter()
                .any(|command| command.name == COMMAND)
        });
        drop(lease);
        set.shutdown_once().await;

        assert!(
            diagnostics.is_empty(),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert_eq!(endpoint_count, 1);
        assert!(
            command_registered,
            "native endpoint registry lost {COMMAND}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_folds_mutable_hooks_in_endpoint_order() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&["input"])).await?;
        let (second, second_host) = make_runner(snapshot(&["input"])).await?;
        first_host.set_response("input", json!({"action": "transform", "text": "first"}));
        second_host.set_response("input", json!({"action": "transform", "text": "second"}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);

        let result = set.emit_input("original", None, "user", None).await?;

        assert_eq!(result.text.as_deref(), Some("second"));
        second_host.wait_for_request("input").await?;
        let second_input = second_host
            .state
            .frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|frame| frame.kind == FrameKind::Req && frame.method == "input")
            .map(|frame| frame.payload["text"].clone());
        assert_eq!(second_input, Some(json!("first")));
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_routes_duplicate_commands_and_tools_to_first_endpoint() -> TestResult {
        let snapshot = json!({
            "tools": [{"name": "sharedTool", "label": "first", "description": "", "parameters": {}}],
            "commands": [{"name": "sharedCommand"}],
            "renderers": [{"type": "tool", "name": "sharedTool"}],
        });
        let (first, first_host) = make_runner(snapshot.clone()).await?;
        let (second, second_host) = make_runner(json!({
            "tools": [{"name": "sharedTool", "label": "second", "description": "", "parameters": {}}],
            "commands": [{"name": "sharedCommand"}],
            "renderers": [{"type": "tool", "name": "sharedTool"}],
        })).await?;
        first_host.set_response("command.execute", json!({"ok": true}));
        second_host.set_response("command.execute", json!({"ok": false}));
        first_host.set_response("tool.renderHtml", json!({"html": "first"}));
        second_host.set_response("tool.renderHtml", json!({"html": "second"}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);

        assert_eq!(set.registry().tools()[0].label, "first");
        assert!(set.execute_command("sharedCommand", "").await?);
        assert_eq!(
            set.render_extension_tool_html(ToolRenderPhase::Call, "sharedTool", &json!({}))
                .await
                .as_deref(),
            Some("first")
        );
        first_host.wait_for_request("command.execute").await?;
        first_host.wait_for_request("tool.renderHtml").await?;
        assert_eq!(second_host.request_count("command.execute"), 0);
        assert_eq!(second_host.request_count("tool.renderHtml"), 0);
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_correlates_same_local_ui_id_per_endpoint() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&[])).await?;
        let (second, second_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        let mut requests = set.take_ui_requests().ok_or("ui bridge missing")?;
        let request = || Frame {
            id: 3,
            kind: FrameKind::Req,
            method: "select".to_owned(),
            payload: json!({"title": "Pick", "options": ["a"]}),
        };
        first_host.emit(request()).await;
        second_host.emit(request()).await;

        let first_request = tokio::time::timeout(TEST_TIMEOUT, requests.recv())
            .await?
            .ok_or("ui bridge closed")?;
        let second_request = tokio::time::timeout(TEST_TIMEOUT, requests.recv())
            .await?
            .ok_or("ui bridge closed")?;
        assert_ne!(first_request.id(), second_request.id());
        set.respond_ui(HostUiResponse::Select {
            id: first_request.id(),
            value: Some("ack".to_owned()),
        })
        .await?;
        set.respond_ui(HostUiResponse::Select {
            id: second_request.id(),
            value: Some("ack".to_owned()),
        })
        .await?;
        first_host.wait_for_response("select", 3).await?;
        second_host.wait_for_response("select", 3).await?;
        assert_eq!(
            first_host
                .response_payload("select", 3)
                .as_ref()
                .and_then(|value| value["value"].as_str()),
            Some("ack")
        );
        assert_eq!(
            second_host
                .response_payload("select", 3)
                .as_ref()
                .and_then(|value| value["value"].as_str()),
            Some("ack")
        );
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_uses_last_slot_owner_then_restores_fallback() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&[])).await?;
        let (second, second_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);

        first_host.emit(slot_frame("shared", "first")).await;
        wait_for_slot_text(&set, "first").await?;
        second_host.emit(slot_frame("shared", "second")).await;
        wait_for_slot_text(&set, "second").await?;
        second_host
            .emit(Frame {
                id: 0,
                kind: FrameKind::Event,
                method: "disposeSlot".to_owned(),
                payload: json!({"key": "shared"}),
            })
            .await;
        wait_for_slot_text(&set, "first").await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_terminal_input_uses_one_deadline_and_consume_precedence() -> TestResult {
        let (slow, slow_host) = make_runner(snapshot(&["terminalInput"])).await?;
        let (rewrite, rewrite_host) = make_runner(snapshot(&["terminalInput"])).await?;
        let (consume, consume_host) = make_runner(snapshot(&["terminalInput"])).await?;
        slow_host.drop_method("terminalInput");
        rewrite_host.set_response(
            "terminalInput",
            json!({"consume": false, "data": "rewritten"}),
        );
        consume_host.set_response("terminalInput", json!({"consume": true, "data": "ignored"}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, consume),
            (EndpointKind::Native, rewrite),
            (EndpointKind::Native, slow),
        ]);

        let result = set.terminal_input("original").await?;

        assert!(result.consume);
        assert!(result.data.is_none());
        slow_host.wait_for_request("terminalInput").await?;
        rewrite_host.wait_for_request("terminalInput").await?;
        consume_host.wait_for_request("terminalInput").await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_keeps_sibling_hook_live_after_endpoint_failure() -> TestResult {
        let (failed, failed_host) = make_runner(snapshot(&["input"])).await?;
        let (live, live_host) = make_runner(snapshot(&["input"])).await?;
        live_host.set_response("input", json!({"action": "transform", "text": "live"}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, failed),
            (EndpointKind::Native, live),
        ]);
        failed_host.close().await;

        let result = set.emit_input("original", None, "user", None).await?;

        assert_eq!(result.text.as_deref(), Some("live"));
        live_host.wait_for_request("input").await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_restart_cutover_publishes_before_retiring_old_generation() -> TestResult {
        let (first, _first_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, first)]);
        let mut ui_requests = set.take_ui_requests().ok_or("ui bridge missing")?;
        let mut session_bridge = set.take_session_bridge().ok_or("session bridge missing")?;
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (next, mut pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        let (ui_tx, ui) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        pending[0].ui = ui;
        assert!(
            ui_tx
                .send(ExtensionUiEvent::Slot(SanitizedSlot {
                    key: "buffered".to_owned(),
                    height: 1,
                    lines: vec![vec![SanitizedRun {
                        text: "buffered".to_owned(),
                        ..SanitizedRun::default()
                    }]],
                    ..SanitizedSlot::default()
                }))
                .is_ok()
        );

        set.cutover(next, pending).await;

        assert_eq!(set.reload_generation(), 2);
        assert!(set.is_active());
        wait_for_slot_text(&set, "buffered").await?;

        replacement_host
            .emit(Frame {
                id: 7,
                kind: FrameKind::Req,
                method: "select".to_owned(),
                payload: json!({"title": "Pick", "options": ["a"]}),
            })
            .await;
        replacement_host
            .emit(Frame {
                id: 8,
                kind: FrameKind::Req,
                method: protocol::SESSION_SET_MODEL_METHOD.to_owned(),
                payload: json!({"model": {"provider": "p", "id": "m"}}),
            })
            .await;

        let ui_request = tokio::time::timeout(TEST_TIMEOUT, ui_requests.recv())
            .await?
            .ok_or("ui bridge closed")?;
        let event = tokio::time::timeout(TEST_TIMEOUT, session_bridge.recv())
            .await?
            .ok_or("session bridge closed")?;
        assert!(ui_request.id() > 0);
        let SessionBridgeEvent::SetModel { id, .. } = event else {
            return Err("expected set-model bridge event".into());
        };
        set.respond_set_model(id, true).await?;
        replacement_host
            .wait_for_response(protocol::SESSION_SET_MODEL_METHOD, 8)
            .await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn cutover_lets_a_hook_leased_before_publish_finish_on_the_old_endpoint() -> TestResult {
        let (old, old_host) =
            make_runner_with_hook_timeout(snapshot(&["input"]), HOOK_TIMEOUT).await?;
        let mut parked = old_host.park_method("input");
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        old_host.emit(slot_frame("old", "old")).await;
        wait_for_slot_text(&set, "old").await?;
        let mut ui = set.subscribe_ui();

        let caller_set = Arc::clone(&set);
        let caller =
            tokio::spawn(
                async move { caller_set.emit_input("original", None, "user", None).await },
            );
        let request_id = parked.recv().await.ok_or("parked hook closed")?;
        let (replacement, _) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        let cutover_set = Arc::clone(&set);
        let cutover = tokio::spawn(async move { cutover_set.cutover(next, pending).await });
        wait_for_dispose(&mut ui, "old").await?;

        assert_eq!(set.reload_generation(), 2);
        assert_eq!(old_host.request_count("shutdown"), 0);
        old_host
            .emit(Frame {
                id: request_id,
                kind: FrameKind::Res,
                method: "input".to_owned(),
                payload: json!({"action": "transform", "text": "old-result"}),
            })
            .await;
        let result = caller.await??;
        assert_eq!(result.text.as_deref(), Some("old-result"));
        cutover.await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn cutover_drops_a_slot_event_from_the_previous_generation() -> TestResult {
        let (old, old_host) =
            make_runner_with_hook_timeout(snapshot(&["input"]), HOOK_TIMEOUT).await?;
        let mut parked = old_host.park_method("input");
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        old_host.emit(slot_frame("old", "old")).await;
        wait_for_slot_text(&set, "old").await?;
        let mut ui = set.subscribe_ui();
        let caller_set = Arc::clone(&set);
        let caller =
            tokio::spawn(
                async move { caller_set.emit_input("original", None, "user", None).await },
            );
        let request_id = parked.recv().await.ok_or("parked hook closed")?;
        let (replacement, _) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        let cutover_set = Arc::clone(&set);
        let cutover = tokio::spawn(async move { cutover_set.cutover(next, pending).await });
        wait_for_dispose(&mut ui, "old").await?;

        old_host.emit(slot_frame("late", "late")).await;
        old_host
            .emit(Frame {
                id: request_id,
                kind: FrameKind::Res,
                method: "input".to_owned(),
                payload: json!({"action": "continue"}),
            })
            .await;
        caller.await??;
        cutover.await?;
        assert!(!set.slot_keys().iter().any(|key| key == "late"));
        assert!(!set.current_slots().iter().any(|slot| slot.key == "late"));
        assert!(ui.try_recv().is_err());
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn retired_relays_answer_locally_and_publish_nothing() -> TestResult {
        let (old, old_host) =
            make_runner_with_hook_timeout(snapshot(&["input"]), HOOK_TIMEOUT).await?;
        let mut parked = old_host.park_method("input");
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        old_host.emit(slot_frame("old", "old")).await;
        wait_for_slot_text(&set, "old").await?;
        let mut ui = set.subscribe_ui();
        let mut ui_requests = set.take_ui_requests().ok_or("ui bridge missing")?;
        let mut session = set.take_session_bridge().ok_or("session bridge missing")?;
        let mut tools = set.subscribe_tool_updates();
        let caller_set = Arc::clone(&set);
        let caller =
            tokio::spawn(
                async move { caller_set.emit_input("original", None, "user", None).await },
            );
        let request_id = parked.recv().await.ok_or("parked hook closed")?;
        let (replacement, _) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        let cutover_set = Arc::clone(&set);
        let cutover = tokio::spawn(async move { cutover_set.cutover(next, pending).await });
        wait_for_dispose(&mut ui, "old").await?;

        old_host
            .emit(Frame {
                id: 41,
                kind: FrameKind::Req,
                method: "select".to_owned(),
                payload: json!({"title": "Pick", "options": ["a"]}),
            })
            .await;
        old_host
            .emit(Frame {
                id: 42,
                kind: FrameKind::Req,
                method: protocol::SESSION_SET_MODEL_METHOD.to_owned(),
                payload: json!({"model": {"provider": "p", "id": "m"}}),
            })
            .await;
        old_host
            .emit(Frame {
                id: 0,
                kind: FrameKind::Event,
                method: "toolUpdate".to_owned(),
                payload: json!({"toolCallId": "c1", "toolName": "t", "partialResult": {"content": []}}),
            })
            .await;
        old_host.wait_for_response("select", 41).await?;
        old_host
            .wait_for_response(protocol::SESSION_SET_MODEL_METHOD, 42)
            .await?;
        assert!(ui_requests.try_recv().is_err());
        assert!(session.try_recv().is_err());
        assert!(tools.try_recv().is_err());
        assert!(matches!(
            set.respond_ui(HostUiResponse::Select { id: 1, value: None })
                .await,
            Err(HostClientError::NotRunning)
        ));

        old_host
            .emit(Frame {
                id: request_id,
                kind: FrameKind::Res,
                method: "input".to_owned(),
                payload: json!({"action": "continue"}),
            })
            .await;
        caller.await??;
        cutover.await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn a_reused_endpoint_position_never_retargets_an_old_slot_or_route() -> TestResult {
        let (old_first, _) = make_runner(snapshot(&[])).await?;
        let (old_owner, old_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, old_first),
            (EndpointKind::Native, old_owner),
        ]);
        old_host.emit(slot_frame("old-key", "old")).await;
        wait_for_slot_text(&set, "old").await?;
        let mut requests = set.take_ui_requests().ok_or("ui bridge missing")?;
        old_host
            .emit(Frame {
                id: FrameId::MAX,
                kind: FrameKind::Req,
                method: "select".to_owned(),
                payload: json!({"title": "Pick", "options": ["a"]}),
            })
            .await;
        let old_route = requests.recv().await.ok_or("ui route missing")?.id();

        let (new_first, _) = make_runner(snapshot(&[])).await?;
        let (new_owner, new_host) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![
                (EndpointKind::TsCompat, "<new-first>".to_owned(), new_first),
                (EndpointKind::Native, "<new-owner>".to_owned(), new_owner),
            ],
        );
        set.cutover(next, pending).await;

        assert!(matches!(
            set.respond_ui(HostUiResponse::Select {
                id: old_route,
                value: None
            })
            .await,
            Err(HostClientError::NotRunning)
        ));
        assert_eq!(new_host.request_count("select"), 0);
        assert!(
            !set.send_ui_event(UiEventRequest {
                key: "old-key".to_owned(),
                generation: 1,
                event: protocol::UiEventWire::Key {
                    code: "x".to_owned(),
                    modifiers: protocol::KeyModifiersWire::default(),
                    kind: protocol::KeyEventKindWire::Press,
                },
                data: None,
            })
            .await?
            .delivered
        );
        assert_eq!(new_host.request_count("uiEvent"), 0);
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn old_endpoints_are_retired_only_after_their_leases_drain() -> TestResult {
        let (old, old_host) =
            make_runner_with_hook_timeout(snapshot(&["input"]), HOOK_TIMEOUT).await?;
        let old_runner = Arc::clone(&old);
        let mut parked = old_host.park_method("input");
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        old_host.emit(slot_frame("old", "old")).await;
        wait_for_slot_text(&set, "old").await?;
        let mut ui = set.subscribe_ui();
        let caller_set = Arc::clone(&set);
        let caller =
            tokio::spawn(
                async move { caller_set.emit_input("original", None, "user", None).await },
            );
        let request_id = parked.recv().await.ok_or("parked hook closed")?;
        let (replacement, _) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        let cutover_set = Arc::clone(&set);
        let cutover = tokio::spawn(async move { cutover_set.cutover(next, pending).await });
        wait_for_dispose(&mut ui, "old").await?;

        assert_eq!(old_host.request_count("shutdown"), 0);
        assert_eq!(old_host.exit_count(), 0);
        assert!(old_runner.is_active());
        old_host
            .emit(Frame {
                id: request_id,
                kind: FrameKind::Res,
                method: "input".to_owned(),
                payload: json!({"action": "continue"}),
            })
            .await;
        caller.await??;
        cutover.await?;
        old_host.wait_for_exit().await?;
        assert_eq!(old_host.exit_count(), 1);
        assert!(!old_runner.is_active());
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn single_endpoint_publication_remains_reloadable() -> TestResult {
        let (old, _old_host) = make_runner(snapshot(&["input"])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        let (replacement, _replacement_host) = make_runner(snapshot(&["input"])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );

        assert!(set.can_reload());
        assert!(set.try_cutover(next, pending).await.is_ok());
        assert_eq!(set.reload_generation(), 2);
        assert!(set.can_reload());

        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn invalidation_refuses_single_endpoint_publication_and_reaps_replacement() -> TestResult
    {
        let (old, _) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );

        set.invalidate();
        let Err(next) = set.try_cutover(next, pending).await else {
            return Err(std::io::Error::other("stale publication succeeded").into());
        };
        stop_generation(&next).await;
        replacement_host.wait_for_exit().await?;
        assert_eq!(set.reload_generation(), 1);
        assert!(!set.can_reload());

        set.shutdown_once().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_manifest_builds_multi_endpoint_replacement_without_publication() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&["input"])).await?;
        let directory = tempfile::tempdir()?;
        write_native_snapshot_host(directory.path(), snapshot(&[]))?;
        std::fs::write(
            directory.path().join("pi-extension.json"),
            r#"{"runtime":"native","entry":"replacement"}"#,
        )?;
        let (generation, pending) =
            generation_from_endpoints(1, vec![(EndpointKind::TsCompat, "<old>".to_owned(), old)]);
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec![directory.path().to_string_lossy().into_owned()],
            String::new(),
            false,
        ));
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        let provider_epoch = runtime.provider_mutation_epoch();

        assert!(
            set.restart_and_rewire(&runtime, HashMap::new())
                .await
                .is_err()
        );
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(set.reload_generation(), 1);
        assert!(set.is_active());
        let result = set.emit_input("original", None, "user", None).await?;
        assert!(!result.handled);
        old_host.wait_for_request("input").await?;

        set.shutdown_once().await;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_set_provider_validation_failure_keeps_live_state_untouched() -> TestResult {
        let old_provider = json!({
            "name": "old-provider",
            "baseUrl": "https://old.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "old-model",
                "name": "Old model",
                "api": "openai-completions",
                "baseUrl": "https://old.example/v1",
                "reasoning": false
            }]
        });
        let (runner, host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        let directory = tempfile::tempdir()?;
        write_native_snapshot_host(
            directory.path(),
            json!({
                "providers": [
                    {
                        "name": "replacement-provider",
                        "baseUrl": "https://replacement.example/v1",
                        "api": "openai-completions",
                        "models": [{
                            "id": "replacement-model",
                            "name": "Replacement model",
                            "api": "openai-completions",
                            "baseUrl": "https://replacement.example/v1",
                            "reasoning": false
                        }]
                    },
                    {
                        "name": "invalid-provider",
                        "models": [{"id": "invalid-model", "reasoning": false}]
                    }
                ]
            }),
        )?;
        std::fs::write(
            directory.path().join("pi-extension.json"),
            r#"{"runtime":"native","entry":"replacement"}"#,
        )?;

        let (generation, pending) = generation_from_endpoints(
            1,
            vec![(EndpointKind::TsCompat, "<old>".to_owned(), runner)],
        );
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec![directory.path().to_string_lossy().into_owned()],
            String::new(),
            false,
        ));
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, result)| result.is_ok())
        );
        let provider_epoch = runtime.provider_mutation_epoch();

        assert!(
            set.restart_and_rewire(&runtime, HashMap::new())
                .await
                .is_err()
        );
        assert_eq!(
            runtime.provider_mutation_epoch(),
            provider_epoch,
            "replacement validation must not publish any provider-map mutation"
        );
        assert_eq!(set.reload_generation(), 1);
        assert!(set.is_active());
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(
            runtime
                .get_registered_provider_config("replacement-provider")
                .is_none()
        );
        assert!(
            runtime
                .get_registered_provider_config("invalid-provider")
                .is_none()
        );
        let result = set.emit_input("original", None, "user", None).await?;
        assert!(!result.handled);
        host.wait_for_request("input").await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_failed_replacement_keeps_old_generation_published() -> TestResult {
        let (runner, host) = make_runner(snapshot(&["input"])).await?;
        let directory = tempfile::tempdir()?;
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, "not executable")?;
        std::fs::write(
            directory.path().join("pi-extension.json"),
            r#"{"runtime":"native","entry":"replacement"}"#,
        )?;
        let (generation, pending) = generation_from_endpoints(
            1,
            vec![(EndpointKind::TsCompat, "<old>".to_owned(), runner)],
        );
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec![directory.path().to_string_lossy().into_owned()],
            String::new(),
            false,
        ));
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;

        assert!(
            set.restart_and_rewire(&runtime, HashMap::new())
                .await
                .is_err()
        );
        assert_eq!(set.reload_generation(), 1);
        assert!(set.is_active());
        let result = set.emit_input("original", None, "user", None).await?;
        assert!(!result.handled);
        host.wait_for_request("input").await?;
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_set_shutdown_closes_every_endpoint_and_repeats_cleanly() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&[])).await?;
        let (second, second_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);

        set.shutdown_once().await;

        first_host.wait_for_exit().await?;
        second_host.wait_for_exit().await?;
        assert_eq!(first_host.exit_count(), 1);
        assert_eq!(second_host.exit_count(), 1);

        // A second facade shutdown also returns cleanly.
        set.shutdown_once().await;
        Ok(())
    }
    #[tokio::test]
    async fn current_reload_admission_covers_every_active_endpoint_class() -> TestResult {
        let (compat, _compat_host) = make_runner(snapshot(&["input"])).await?;
        let compat_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, compat)]);
        assert!(compat_set.can_reload());
        let (stale_compat, _stale_compat_host) = make_runner(snapshot(&["input"])).await?;
        let stale_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, stale_compat)]);
        stale_set.invalidate();
        assert!(!stale_set.can_reload());
        let (inactive_compat, _inactive_compat_host) = make_runner(snapshot(&["input"])).await?;
        inactive_compat.invalidate();
        let inactive_set =
            ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, inactive_compat)]);
        assert!(!inactive_set.can_reload());
        let (native, _native_host) = make_runner(snapshot(&["input"])).await?;
        let native_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::Native, native)]);
        assert!(!native_set.can_reload());
        let (first, _first_host) = make_runner(snapshot(&["input"])).await?;
        let (second, _second_host) = make_runner(snapshot(&["input"])).await?;
        let multi_set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        assert!(!multi_set.can_reload());
        let (active_compat, _active_compat_host) = make_runner(snapshot(&["input"])).await?;
        let (inactive_native, _inactive_native_host) = make_runner(snapshot(&["input"])).await?;
        inactive_native.invalidate();
        let compat_with_inactive_sibling = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, active_compat),
            (EndpointKind::Native, inactive_native),
        ]);
        assert!(compat_with_inactive_sibling.can_reload());
        {
            let runtime = ModelRuntime::create_in_memory().await?;
            let provider_epoch = runtime.provider_mutation_epoch();
            for set in [&stale_set, &inactive_set, &native_set, &multi_set] {
                assert!(
                    set.restart_and_rewire(&runtime, HashMap::new())
                        .await
                        .is_err()
                );
                assert_eq!(set.reload_generation(), 1);
                assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
            }

            let (native_replacement, native_replacement_host) = make_runner(snapshot(&[])).await?;
            let (native_replacement, native_pending) = generation_from_endpoints(
                2,
                vec![(
                    EndpointKind::Native,
                    "<native-replacement>".to_owned(),
                    native_replacement,
                )],
            );
            compat_with_inactive_sibling
                .inject_prepared_replacement_for_reload(native_replacement, native_pending);
            assert!(
                compat_with_inactive_sibling
                    .restart_and_rewire(&runtime, HashMap::new())
                    .await
                    .is_err()
            );
            assert_eq!(compat_with_inactive_sibling.reload_generation(), 1);
            assert!(compat_with_inactive_sibling.is_active());
            assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
            assert!(runtime.get_registered_provider_ids().is_empty());
            native_replacement_host.wait_for_exit().await?;
            assert_eq!(native_replacement_host.exit_count(), 1);

            let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
            let (replacement, replacement_pending) = generation_from_endpoints(
                2,
                vec![(
                    EndpointKind::TsCompat,
                    "<replacement>".to_owned(),
                    replacement,
                )],
            );
            compat_with_inactive_sibling.inject_prepared_replacement_then_invalidation_for_reload(
                replacement,
                replacement_pending,
            );
            assert!(
                compat_with_inactive_sibling
                    .restart_and_rewire(&runtime, HashMap::new())
                    .await
                    .is_err()
            );
            assert_eq!(replacement_host.request_count("flags.set"), 1);
            assert_eq!(compat_with_inactive_sibling.reload_generation(), 1);
            assert!(!compat_with_inactive_sibling.is_active());
            assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
            assert!(runtime.get_registered_provider_ids().is_empty());
            replacement_host.wait_for_exit().await?;
            assert_eq!(replacement_host.exit_count(), 1);

            compat_set.shutdown_once().await;
        }
        stale_set.shutdown_once().await;
        inactive_set.shutdown_once().await;
        native_set.shutdown_once().await;
        multi_set.shutdown_once().await;
        compat_with_inactive_sibling.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn prepared_reload_admission_requires_one_compat_endpoint() -> TestResult {
        let (compat, _compat_host) = make_runner(snapshot(&[])).await?;
        let (compat_generation, _) = generation_from_endpoints(
            2,
            vec![(EndpointKind::TsCompat, "<compat>".to_owned(), compat)],
        );
        assert!(compat_generation.is_single_compat_replacement());
        stop_generation(&compat_generation).await;

        let (native, _native_host) = make_runner(snapshot(&[])).await?;
        let (native_generation, _) = generation_from_endpoints(
            2,
            vec![(EndpointKind::Native, "<native>".to_owned(), native)],
        );
        assert!(!native_generation.is_single_compat_replacement());
        stop_generation(&native_generation).await;

        let (first, _first_host) = make_runner(snapshot(&[])).await?;
        let (second, _second_host) = make_runner(snapshot(&[])).await?;
        let (multi_generation, _) = generation_from_endpoints(
            2,
            vec![
                (EndpointKind::TsCompat, "<compat>".to_owned(), first),
                (EndpointKind::Native, "<native>".to_owned(), second),
            ],
        );
        assert!(!multi_generation.is_single_compat_replacement());
        stop_generation(&multi_generation).await;
        Ok(())
    }

    #[tokio::test]
    async fn prepared_native_reload_rejects_before_mutating_published_state() -> TestResult {
        let old_provider = json!({
            "name": "old-provider",
            "baseUrl": "https://old.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "old-model",
                "name": "Old model",
                "api": "openai-completions",
                "baseUrl": "https://old.example/v1",
                "reasoning": false
            }]
        });
        let replacement_provider = json!({
            "name": "replacement-provider",
            "baseUrl": "https://replacement.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "replacement-model",
                "name": "Replacement model",
                "api": "openai-completions",
                "baseUrl": "https://replacement.example/v1",
                "reasoning": false
            }]
        });
        let (old, old_host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        let (replacement, replacement_host) = make_runner(json!({
            "providers": [replacement_provider],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (generation, pending) =
            generation_from_endpoints(1, vec![(EndpointKind::TsCompat, "<old>".to_owned(), old)]);
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, result)| result.is_ok())
        );
        let provider_epoch = runtime.provider_mutation_epoch();
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::Native,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_for_reload(replacement, pending);

        assert!(
            set.restart_and_rewire(&runtime, HashMap::new())
                .await
                .is_err()
        );
        assert_eq!(replacement_host.request_count("flags.set"), 0);
        assert_eq!(set.reload_generation(), 1);
        assert!(set.is_active());
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        assert!(
            runtime
                .get_registered_provider_config("replacement-provider")
                .is_none()
        );
        replacement_host.wait_for_exit().await?;
        assert_eq!(replacement_host.exit_count(), 1);
        let result = set.emit_input("original", None, "user", None).await?;
        assert!(!result.handled);
        old_host.wait_for_request("input").await?;

        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn invalidation_before_publication_keeps_provider_map_and_reaps_replacement() -> TestResult
    {
        let old_provider = json!({
            "name": "old-provider",
            "baseUrl": "https://old.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "old-model",
                "name": "Old model",
                "api": "openai-completions",
                "baseUrl": "https://old.example/v1",
                "reasoning": false
            }]
        });
        let replacement_provider = json!({
            "name": "replacement-provider",
            "baseUrl": "https://replacement.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "replacement-model",
                "name": "Replacement model",
                "api": "openai-completions",
                "baseUrl": "https://replacement.example/v1",
                "reasoning": false
            }]
        });
        let (old, old_host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        let (replacement, replacement_host) = make_runner(json!({
            "providers": [replacement_provider],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (generation, pending) =
            generation_from_endpoints(1, vec![(EndpointKind::TsCompat, "<old>".to_owned(), old)]);
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, result)| result.is_ok())
        );
        let provider_epoch = runtime.provider_mutation_epoch();
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_then_invalidation_for_reload(replacement, pending);

        assert!(
            set.restart_and_rewire(&runtime, HashMap::new())
                .await
                .is_err()
        );
        assert_eq!(replacement_host.request_count("flags.set"), 1);
        assert_eq!(set.reload_generation(), 1);
        assert!(!set.is_active());
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        assert!(
            runtime
                .get_registered_provider_config("replacement-provider")
                .is_none()
        );
        replacement_host.wait_for_exit().await?;
        assert_eq!(replacement_host.exit_count(), 1);

        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }
}
