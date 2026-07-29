//! Stable product facade over an ordered set of extension host endpoints.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
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
    index: usize,
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

#[derive(Clone)]
struct Endpoint {
    index: usize,
    label: String,
    runner: Arc<HostExtensionRunner>,
}

struct Generation {
    id: u64,
    endpoints: Arc<[Endpoint]>,
    bridges: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
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

/// Every relay for one endpoint shares this generation-tagged routing identity.
struct EndpointRelayContext {
    generation_id: u64,
    current_generation: Arc<AtomicU64>,
    aggregate: Arc<Aggregate>,
    endpoint: Endpoint,
}

type PendingBridges = Vec<PendingEndpointBridges>;

#[derive(Clone, Copy)]
struct CorrelationRoute {
    generation: u64,
    endpoint: usize,
    local: FrameId,
}

struct Aggregate {
    tool_updates_tx: broadcast::Sender<ToolUpdate>,
    provider_events_tx: broadcast::Sender<ProviderEvent>,
    errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    ui_tx: broadcast::Sender<ExtensionUiEvent>,
    ui_requests_tx: mpsc::Sender<HostUiRequest>,
    ui_requests_rx: StdMutex<Option<mpsc::Receiver<HostUiRequest>>>,
    session_bridge_tx: mpsc::Sender<SessionBridgeEvent>,
    session_bridge_rx: StdMutex<Option<mpsc::Receiver<SessionBridgeEvent>>>,
    slots: StdMutex<HashMap<String, BTreeMap<usize, SanitizedSlot>>>,
    next_route_id: AtomicU64,
    routes: StdMutex<HashMap<FrameId, CorrelationRoute>>,
    stale: AtomicBool,
    shutdown_done: AtomicBool,
    shutdown_lock: tokio::sync::Mutex<()>,
}

impl Aggregate {
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
            slots: StdMutex::new(HashMap::new()),
            next_route_id: AtomicU64::new(1),
            routes: StdMutex::new(HashMap::new()),
            stale: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
            shutdown_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn route(&self, generation: u64, endpoint: usize, local: FrameId) -> FrameId {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let id = self.next_route_id.fetch_add(1, Ordering::Relaxed);
            if id == 0 || routes.contains_key(&id) {
                continue;
            }
            routes.insert(
                id,
                CorrelationRoute {
                    generation,
                    endpoint,
                    local,
                },
            );
            return id;
        }
    }

    fn take_route(&self, id: FrameId) -> Option<CorrelationRoute> {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
    }

    fn remove_routes_for(&self, generation: u64, endpoint: usize) {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, route| route.generation != generation || route.endpoint != endpoint);
    }

    fn clear_routes(&self) {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
    fn publish_error(&self, code: &str, message: String, data: Option<Value>) {
        let _ = self.errors_tx.send(ExtensionErrorEvent {
            code: code.to_owned(),
            message,
            retryable: false,
            data,
        });
    }

    fn slot(&self, endpoint: usize, slot: SanitizedSlot) {
        let publish = {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owners = slots.entry(slot.key.clone()).or_default();
            owners.insert(endpoint, slot.clone());
            owners.keys().next_back().copied() == Some(endpoint)
        };
        if publish {
            let _ = self.ui_tx.send(ExtensionUiEvent::Slot(slot));
        }
    }

    fn dispose(&self, endpoint: usize, key: String) {
        let event = {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(owners) = slots.get_mut(&key) else {
                return;
            };
            let was_owner = owners.keys().next_back().copied() == Some(endpoint);
            owners.remove(&endpoint);
            if !was_owner {
                None
            } else if let Some((_, fallback)) = owners.last_key_value() {
                Some(ExtensionUiEvent::Slot(fallback.clone()))
            } else {
                slots.remove(&key);
                Some(ExtensionUiEvent::Dispose { key })
            }
        };
        if let Some(event) = event {
            let _ = self.ui_tx.send(event);
        }
    }

    fn dispose_all_slots(&self) {
        let keys = {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut keys = slots.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            slots.clear();
            keys
        };
        for key in keys {
            let _ = self.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
    }
}

/// Stable facade whose published endpoint generation can be replaced in place.
pub struct ExtensionRuntimeSet {
    generation: RwLock<Arc<Generation>>,
    current_generation: Arc<AtomicU64>,
    aggregate: Arc<Aggregate>,
    discovered_paths: Vec<String>,
    load_cwd: String,
    project_trusted: bool,
    reload_lock: tokio::sync::Mutex<()>,
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
        set.start_bridges(pending);
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
        let id = generation.id;
        Self {
            generation: RwLock::new(Arc::new(generation)),
            current_generation: Arc::new(AtomicU64::new(id)),
            aggregate: Arc::new(Aggregate::new()),
            discovered_paths,
            load_cwd,
            project_trusted,
            reload_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Bind pre-built single-endpoint runners (focused fake-host tests).
    #[cfg(test)]
    pub(crate) fn bind(endpoints: Vec<(EndpointKind, Arc<HostExtensionRunner>)>) -> Arc<Self> {
        let endpoints = endpoints
            .into_iter()
            .enumerate()
            .map(|(index, (_kind, runner))| Endpoint {
                index,
                label: format!("<test:{index}>"),
                runner,
            })
            .collect::<Vec<_>>();
        let (generation, pending) = generation_from_endpoints(1, endpoints);
        let set = Arc::new(Self::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.start_bridges(pending);
        set
    }

    fn endpoints(&self) -> Arc<[Endpoint]> {
        self.generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .endpoints
            .clone()
    }

    fn start_bridges(&self, pending: PendingBridges) {
        let generation_id = self.reload_generation();
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
                slots,
            } = pending_endpoint;
            let context = EndpointRelayContext {
                generation_id,
                current_generation: Arc::clone(&self.current_generation),
                aggregate: Arc::clone(&self.aggregate),
                endpoint,
            };
            for slot in slots {
                context.aggregate.slot(context.endpoint.index, slot);
            }
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

        let generation = self
            .generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if generation.id == generation_id {
            generation
                .bridges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(handles);
        } else {
            for handle in handles {
                handle.abort();
            }
        }
    }

    /// Registered flag types, first endpoint wins.
    #[must_use]
    pub fn registered_flag_types(&self) -> BTreeMap<String, ExtensionFlagType> {
        let mut flags = BTreeMap::new();
        for endpoint in self.endpoints().iter() {
            for (name, kind) in endpoint.runner.registered_flag_types() {
                flags.entry(name).or_insert(kind);
            }
        }
        flags
    }

    /// Registered custom providers, first endpoint wins.
    #[must_use]
    pub fn providers(&self) -> HashMap<String, ExtensionProvider> {
        let mut providers = HashMap::new();
        for endpoint in self.endpoints().iter() {
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
        register_endpoint_providers(&self.endpoints(), runtime)
    }

    /// Remove each first-owned provider once.
    pub fn unregister_providers_from(&self, runtime: &ModelRuntime) {
        unregister_endpoint_providers(&self.endpoints(), runtime);
    }

    /// Aggregate registry with existing first-wins semantics.
    #[must_use]
    pub fn registry(&self) -> Registry {
        let mut aggregate = Registry::new();
        for endpoint in self.endpoints().iter() {
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
        self.endpoints()
            .iter()
            .flat_map(|endpoint| endpoint.runner.raw_shortcuts())
            .collect()
    }

    /// Current published generation id.
    #[must_use]
    pub fn reload_generation(&self) -> u64 {
        self.current_generation.load(Ordering::Acquire)
    }

    /// Whether any non-stale endpoint transport is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.aggregate.stale.load(Ordering::Relaxed)
            && self
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.is_running())
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
        let mut first_error = None;
        for endpoint in self.endpoints().iter() {
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
        let endpoints = self.endpoints();
        for endpoint in endpoints.iter().rev() {
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
        let owner = self
            .aggregate
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&request.key)
            .and_then(|owners| owners.keys().next_back().copied());
        let Some(owner) = owner else {
            return Ok(UiEventResponse { delivered: false });
        };
        let endpoints = self.endpoints();
        let Some(endpoint) = endpoints.get(owner) else {
            return Ok(UiEventResponse { delivered: false });
        };
        endpoint.runner.send_ui_event(request).await
    }

    /// Currently live effective slot keys.
    #[must_use]
    pub fn slot_keys(&self) -> Vec<String> {
        self.aggregate
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Current last-owner slot per key, sorted by key.
    #[must_use]
    pub fn current_slots(&self) -> Vec<SanitizedSlot> {
        let mut slots = self
            .aggregate
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|owners| owners.last_key_value().map(|(_, slot)| slot.clone()))
            .collect::<Vec<_>>();
        slots.sort_by(|left, right| left.key.cmp(&right.key));
        slots
    }

    /// Subscribe to aggregate tool updates.
    #[must_use]
    pub fn subscribe_tool_updates(&self) -> broadcast::Receiver<ToolUpdate> {
        self.aggregate.tool_updates_tx.subscribe()
    }

    /// Subscribe to aggregate provider events.
    #[must_use]
    pub fn subscribe_provider_events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.aggregate.provider_events_tx.subscribe()
    }

    /// Subscribe to aggregate extension errors.
    #[must_use]
    pub fn subscribe_errors(&self) -> broadcast::Receiver<ExtensionErrorEvent> {
        self.aggregate.errors_tx.subscribe()
    }

    /// Whether any active endpoint handles terminal input.
    #[must_use]
    pub fn has_terminal_input_handlers(&self) -> bool {
        self.endpoints().iter().any(|endpoint| {
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
        let endpoints = self.endpoints();
        let mut pending = FuturesUnordered::new();
        for endpoint in endpoints.iter() {
            if endpoint.runner.is_active() && endpoint.runner.has_terminal_input_handlers() {
                let index = endpoint.index;
                let runner = Arc::clone(&endpoint.runner);
                let data = data.to_owned();
                pending.push(async move { (index, runner.terminal_input(&data).await) });
            }
        }
        if pending.is_empty() {
            return Ok(protocol::TerminalInputResult::default());
        }
        let deadline = tokio::time::Instant::now() + TERMINAL_INPUT_DEADLINE;
        let mut replies = vec![None; endpoints.len()];
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
        self.aggregate.ui_tx.subscribe()
    }

    /// Highest endpoint theme generation.
    #[must_use]
    pub fn theme_generation(&self) -> u64 {
        self.endpoints()
            .iter()
            .map(|endpoint| endpoint.runner.theme_generation())
            .max()
            .unwrap_or(0)
    }

    /// Broadcast a theme update to all active endpoints.
    pub async fn push_theme_update(&self, update: &ThemeUpdate) {
        for endpoint in self.endpoints().iter() {
            endpoint.runner.push_theme_update(update).await;
        }
    }

    /// Claim the persistent facade UI-request receiver once.
    #[must_use]
    pub fn take_ui_requests(&self) -> Option<mpsc::Receiver<HostUiRequest>> {
        self.aggregate
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
        let route = self
            .aggregate
            .take_route(ui_response_id(&response))
            .filter(|route| route.generation == self.reload_generation())
            .ok_or(HostClientError::NotRunning)?;
        let endpoint = self
            .endpoints()
            .get(route.endpoint)
            .cloned()
            .ok_or(HostClientError::NotRunning)?;
        endpoint
            .runner
            .respond_ui(map_ui_response_id(response, route.local))
            .await
    }

    /// Whether this facade has at least one active endpoint.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.aggregate.stale.load(Ordering::Relaxed)
            && self
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.is_active())
    }

    /// Claim the persistent facade session bridge once.
    #[must_use]
    pub fn take_session_bridge(&self) -> Option<mpsc::Receiver<SessionBridgeEvent>> {
        self.aggregate
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
        let route = self
            .aggregate
            .take_route(id)
            .filter(|route| route.generation == self.reload_generation())
            .ok_or(HostClientError::NotRunning)?;
        let endpoint = self
            .endpoints()
            .get(route.endpoint)
            .cloned()
            .ok_or(HostClientError::NotRunning)?;
        endpoint
            .runner
            .respond_set_model(route.local, success)
            .await
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
        let route = self
            .aggregate
            .take_route(id)
            .filter(|route| route.generation == self.reload_generation())
            .ok_or(HostClientError::NotRunning)?;
        let endpoint = self
            .endpoints()
            .get(route.endpoint)
            .cloned()
            .ok_or(HostClientError::NotRunning)?;
        endpoint.runner.respond_compact(route.local, outcome).await
    }

    /// Broadcast mirrored session state.
    pub async fn push_session_state(&self, state: &SessionStateWire) {
        for endpoint in self.endpoints().iter() {
            endpoint.runner.push_session_state(state).await;
        }
    }

    /// Broadcast mirrored UI state.
    pub async fn push_ui_state(&self, state: &UiStateWire) {
        for endpoint in self.endpoints().iter() {
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
        for endpoint in self.endpoints().iter() {
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
        if self.aggregate.shutdown_done.load(Ordering::Relaxed)
            || self.aggregate.stale.load(Ordering::Relaxed)
        {
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
        let (next, pending, diagnostics) =
            build_generation(next_id, plans, &self.load_cwd, self.project_trusted, true).await;
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

        let old = self
            .generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        unregister_generation_providers(&old, runtime);
        let registrations = register_generation_providers(&next, runtime);
        if let Some((path, Err(error))) = registrations
            .iter()
            .find(|(_path, outcome)| outcome.is_err())
        {
            unregister_generation_providers(&next, runtime);
            let _ = register_generation_providers(&old, runtime);
            stop_generation(&next).await;
            return Err(HostStartError::Load(format!("{path}: {error}")));
        }

        self.aggregate.dispose_all_slots();
        self.aggregate.clear_routes();
        for endpoint in old.endpoints.iter() {
            endpoint.runner.invalidate();
        }

        {
            let mut generation = self
                .generation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *generation = Arc::new(next);
        }
        self.current_generation.store(next_id, Ordering::Release);
        // Close the quarantine race: an old bridge may have drained an already
        // queued item after the pre-swap cleanup but before observing this tag.
        self.aggregate.dispose_all_slots();
        self.aggregate.clear_routes();
        self.start_bridges(pending);
        abort_bridges(&old);
        stop_generation(&old).await;
        Ok(())
    }

    /// Invalidate all endpoints and synchronously dispose product-visible slots.
    pub fn invalidate(&self) {
        if self.aggregate.stale.swap(true, Ordering::Relaxed) {
            return;
        }
        for endpoint in self.endpoints().iter() {
            endpoint.runner.invalidate();
        }
        self.aggregate.dispose_all_slots();
        self.aggregate.clear_routes();
    }

    /// Gracefully stop every endpoint exactly once.
    pub async fn shutdown_once(&self) {
        let _reload = self.reload_lock.lock().await;
        let _shutdown = self.aggregate.shutdown_lock.lock().await;
        if self.aggregate.shutdown_done.load(Ordering::Relaxed) {
            return;
        }
        self.aggregate.stale.store(true, Ordering::Relaxed);
        let generation = self
            .generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        stop_generation(&generation).await;
        self.aggregate.dispose_all_slots();
        self.aggregate.clear_routes();
        abort_bridges(&generation);
        self.aggregate.shutdown_done.store(true, Ordering::Relaxed);
    }
}

impl ExtensionRunner for ExtensionRuntimeSet {
    fn has_handlers(&self, event: &str) -> bool {
        self.is_active()
            && self
                .endpoints()
                .iter()
                .any(|endpoint| endpoint.runner.has_handlers(event))
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>> {
        let endpoints = self.endpoints();
        Box::pin(async move {
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            let mut current = message;
            let mut changed = false;
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            let mut current_content = content;
            let mut current_details = details;
            let mut current_error = is_error;
            let mut output = AfterToolCallResult::default();
            let mut changed = false;
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            let mut current_text = text.to_owned();
            let mut current_images = images;
            let mut text_changed = false;
            let mut images_changed = false;
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            let mut messages = Vec::new();
            let mut system_prompt = None;
            let mut changed = false;
            for endpoint in endpoints.iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            let mut aggregate = ResourceExtensionPaths::default();
            for endpoint in endpoints.iter() {
                let paths = endpoint.runner.emit_resources_discover(cwd, reason).await?;
                aggregate.skill_paths.extend(paths.skill_paths);
                aggregate.prompt_paths.extend(paths.prompt_paths);
                aggregate.theme_paths.extend(paths.theme_paths);
            }
            Ok(aggregate)
        })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut commands = Vec::new();
        for endpoint in self.endpoints().iter() {
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
        let endpoints = self.endpoints();
        Box::pin(async move {
            for endpoint in endpoints.iter() {
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
        let mut tools = HashMap::new();
        for endpoint in self.endpoints().iter() {
            for (name, tool) in endpoint.runner.get_all_registered_tools() {
                tools.entry(name).or_insert(tool);
            }
        }
        tools
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        let mut flags = HashMap::new();
        for endpoint in self.endpoints().iter() {
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
        self.aggregate
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
            index: 0,
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
                index: 0,
                kind: EndpointKind::TsCompat,
                entries: Vec::new(),
                diagnostic_paths: Vec::new(),
                builtins: true,
                label: "<builtins>".to_owned(),
            },
        );
    }
    for (index, plan) in plans.iter_mut().enumerate() {
        plan.index = index;
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
            let index = plan.index;
            let result = match spec {
                Ok(spec) => start_endpoint(plan.clone(), spec, cwd, project_trusted).await,
                Err(message) => Err(message),
            };
            (index, plan, result)
        });
    }

    let mut results = Vec::new();
    while let Some(result) = starts.next().await {
        results.push(result);
    }
    results.sort_by_key(|(index, _, _)| *index);

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
                endpoints.push(Endpoint {
                    index: plan.index,
                    label: plan.label,
                    runner,
                });
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
        for endpoint in &endpoints {
            endpoint.runner.shutdown_once().await;
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
    mut endpoints: Vec<Endpoint>,
) -> (Generation, PendingBridges) {
    for (index, endpoint) in endpoints.iter_mut().enumerate() {
        endpoint.index = index;
    }
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
        },
        pending,
    )
}

fn spawn_broadcast_relay<T, F>(
    generation: u64,
    current_generation: Arc<AtomicU64>,
    aggregate: Arc<Aggregate>,
    label: String,
    mut receiver: broadcast::Receiver<T>,
    publish: F,
) -> tokio::task::JoinHandle<()>
where
    T: Clone + Send + 'static,
    F: Fn(&Aggregate, T) + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(item) => {
                    if current_generation.load(Ordering::Acquire) == generation {
                        publish(&aggregate, item);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    if current_generation.load(Ordering::Acquire) == generation {
                        aggregate.publish_error(
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
    let generation_id = context.generation_id;
    let current_generation = &context.current_generation;
    let aggregate = &context.aggregate;
    let label = &context.endpoint.label;
    let index = context.endpoint.index;
    let error_runner = Arc::clone(&context.endpoint.runner);
    vec![
        spawn_broadcast_relay(
            generation_id,
            Arc::clone(current_generation),
            Arc::clone(aggregate),
            label.clone(),
            tool_updates,
            |aggregate, item| {
                let _ = aggregate.tool_updates_tx.send(item);
            },
        ),
        spawn_broadcast_relay(
            generation_id,
            Arc::clone(current_generation),
            Arc::clone(aggregate),
            label.clone(),
            provider_events,
            |aggregate, item| {
                let _ = aggregate.provider_events_tx.send(item);
            },
        ),
        spawn_broadcast_relay(
            generation_id,
            Arc::clone(current_generation),
            Arc::clone(aggregate),
            label.clone(),
            errors,
            move |aggregate, item| {
                if !error_runner.is_active() {
                    aggregate.remove_routes_for(generation_id, index);
                }
                let _ = aggregate.errors_tx.send(item);
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
    let generation_id = context.generation_id;
    let ui_generation = Arc::clone(&context.current_generation);
    let ui_aggregate = Arc::clone(&context.aggregate);
    let ui_label = context.endpoint.label.clone();
    let index = context.endpoint.index;
    let mut handles = vec![tokio::spawn(async move {
        loop {
            match ui.recv().await {
                Ok(event) => {
                    if ui_generation.load(Ordering::Acquire) != generation_id {
                        continue;
                    }
                    match event {
                        ExtensionUiEvent::Slot(slot) => ui_aggregate.slot(index, slot),
                        ExtensionUiEvent::Dispose { key } => ui_aggregate.dispose(index, key),
                        other => {
                            let _ = ui_aggregate.ui_tx.send(other);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    if ui_generation.load(Ordering::Acquire) == generation_id {
                        ui_aggregate.publish_error(
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
        let request_generation = Arc::clone(&context.current_generation);
        let request_aggregate = Arc::clone(&context.aggregate);
        let runner = Arc::clone(&context.endpoint.runner);
        handles.push(tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let fallback = request.clone();
                let local_id = request.id();
                if request_generation.load(Ordering::Acquire) != generation_id {
                    let _ = runner.respond_ui(default_ui_response(&request)).await;
                    continue;
                }
                let routed_id = request_aggregate.route(generation_id, index, local_id);
                let routed = map_ui_request_id(request, routed_id);
                if request_aggregate.ui_requests_tx.try_send(routed).is_err() {
                    request_aggregate.take_route(routed_id);
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
    let generation_id = context.generation_id;
    let session_generation = Arc::clone(&context.current_generation);
    let session_aggregate = Arc::clone(&context.aggregate);
    let runner = Arc::clone(&context.endpoint.runner);
    let index = context.endpoint.index;
    Some(tokio::spawn(async move {
        while let Some(event) = session.recv().await {
            if session_generation.load(Ordering::Acquire) != generation_id {
                answer_unclaimed_session(&runner, event).await;
                continue;
            }
            let fallback = event.clone();
            let (routed, route_id) = match event {
                SessionBridgeEvent::SetModel { id, request } => {
                    let routed_id = session_aggregate.route(generation_id, index, id);
                    (
                        SessionBridgeEvent::SetModel {
                            id: routed_id,
                            request,
                        },
                        Some(routed_id),
                    )
                }
                SessionBridgeEvent::Compact { id, request } => {
                    let routed_id = session_aggregate.route(generation_id, index, id);
                    (
                        SessionBridgeEvent::Compact {
                            id: routed_id,
                            request,
                        },
                        Some(routed_id),
                    )
                }
                SessionBridgeEvent::Command(command) => {
                    (SessionBridgeEvent::Command(command), None)
                }
            };
            if session_aggregate
                .session_bridge_tx
                .try_send(routed)
                .is_err()
            {
                if let Some(route_id) = route_id {
                    session_aggregate.take_route(route_id);
                }
                answer_unclaimed_session(&runner, fallback).await;
            }
        }
    }))
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

fn register_generation_providers(
    generation: &Generation,
    runtime: &ModelRuntime,
) -> Vec<(String, Result<(), ModelRuntimeError>)> {
    register_endpoint_providers(&generation.endpoints, runtime)
}

fn unregister_generation_providers(generation: &Generation, runtime: &ModelRuntime) {
    unregister_endpoint_providers(&generation.endpoints, runtime);
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
            Duration::from_millis(200),
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

    async fn wait_for_slot_text(set: &ExtensionRuntimeSet, expected: &str) -> TestResult {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if set.current_slots().first().is_some_and(|slot| {
                    slot.lines
                        .first()
                        .and_then(|line| line.first())
                        .is_some_and(|run| run.text == expected)
                }) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| format!("slot did not become {expected}"))?;
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
                index: 0,
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
        set.start_bridges(pending);

        let endpoints = set.endpoints();
        let endpoint_count = endpoints.len();
        let command_registered = endpoints.first().is_some_and(|endpoint| {
            endpoint
                .runner
                .registry()
                .commands()
                .iter()
                .any(|command| command.name == COMMAND)
        });
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

    #[test]
    fn correlation_routes_preserve_full_local_ids_and_are_claimed_once() -> Result<(), &'static str>
    {
        let aggregate = Aggregate::new();
        let id = aggregate.route(7, 2, FrameId::MAX);
        let route = aggregate.take_route(id).ok_or("route")?;
        assert_eq!(route.generation, 7);
        assert_eq!(route.endpoint, 2);
        assert_eq!(route.local, FrameId::MAX);
        assert!(aggregate.take_route(id).is_none());
        Ok(())
    }

    #[test]
    fn slot_owner_falls_back_in_endpoint_order() {
        let aggregate = Aggregate::new();
        let slot0 = SanitizedSlot {
            key: "shared".to_owned(),
            ..SanitizedSlot::default()
        };
        let slot1 = slot0.clone();
        aggregate.slot(0, slot0.clone());
        aggregate.slot(1, slot1);
        aggregate.dispose(1, "shared".to_owned());
        assert_eq!(
            aggregate
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get("shared")
                .and_then(|owners| owners.last_key_value())
                .map(|(index, _)| *index),
            Some(0)
        );
        aggregate.dispose(0, "shared".to_owned());
        assert!(
            aggregate
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
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
    async fn runtime_set_keeps_claimed_bridges_across_generation_swap() -> TestResult {
        let (first, _first_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, first)]);
        let mut ui_requests = set.take_ui_requests().ok_or("ui bridge missing")?;
        let mut session_bridge = set.take_session_bridge().ok_or("session bridge missing")?;
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (next, pending) = generation_from_endpoints(
            2,
            vec![Endpoint {
                index: 0,
                label: "<replacement>".to_owned(),
                runner: replacement,
            }],
        );
        let old = {
            let mut generation = set
                .generation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *generation, Arc::new(next))
        };
        set.current_generation.store(2, Ordering::Release);
        set.start_bridges(pending);
        abort_bridges(&old);
        stop_generation(&old).await;

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
            vec![Endpoint {
                index: 0,
                label: "<old>".to_owned(),
                runner,
            }],
        );
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec![directory.path().to_string_lossy().into_owned()],
            String::new(),
            false,
        ));
        set.start_bridges(pending);
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
            vec![Endpoint {
                index: 0,
                label: "<old>".to_owned(),
                runner,
            }],
        );
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec![directory.path().to_string_lossy().into_owned()],
            String::new(),
            false,
        ));
        set.start_bridges(pending);
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
}
