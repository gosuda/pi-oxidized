//! Stable product facade over an ordered set of extension host endpoints.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
    self, ExtensionErrorEvent, FlagValueWire, FrameId, ProviderEvent, SessionSetupEntriesResponse,
    SessionStateWire, ShortcutExecuteResponse, ThemeUpdate, ToolUpdate, UiEventRequest,
    UiEventResponse, UiStateWire,
};
use pi_ext::sanitize::SanitizedSlot;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::agent_session::AgentSession;
use super::agent_session::events::{AgentSessionEvent, SessionShutdownReason};
use super::agent_session::extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, ExtensionRunnerError,
    InputTransformResult,
};
use super::agent_session_runtime::{CreateAgentSessionRuntimeResult, spawn_runtime_safe};
use super::agent_session_services::ExtensionFlagType;
use super::extension_host::{
    EVENT_CHANNEL_CAPACITY, ExtensionUiEvent, HOOK_TIMEOUT, HostExtensionRunner, HostStartError,
    SessionBridgeEvent, ToolRenderPhase, default_ui_response,
};
use super::extension_manifest::{ClassifiedExtension, ExtensionRuntime, classify};
use super::model_runtime::{ModelRuntime, ModelRuntimeError};
use super::resources::{ResourceExtensionPaths, SourceInfo};

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

#[derive(Clone, Copy)]
enum GenerationBuildPolicy {
    BestEffortStart,
    #[allow(dead_code)]
    RequireAllEndpointStarts,
}

struct GenerationBuild {
    generation: Option<Generation>,
    pending: PendingBridges,
    diagnostics: Vec<ExtensionSetDiagnostic>,
    endpoint_start_failure: Option<ExtensionSetDiagnostic>,
}

type EndpointStartOutcome = (
    usize,
    EndpointPlan,
    Result<Arc<HostExtensionRunner>, String>,
);

struct PreparedEndpoint {
    position: usize,
    kind: EndpointKind,
    label: String,
    runner: Arc<HostExtensionRunner>,
    plan: EndpointPlan,
}

struct GenerationStarts {
    endpoints: Vec<PreparedEndpoint>,
    diagnostics: Vec<ExtensionSetDiagnostic>,
    endpoint_start_failure: Option<ExtensionSetDiagnostic>,
    failed_builtins_owner: Option<EndpointPlan>,
}

/// One path-scoped startup or load failure. Other paths may remain active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionSetDiagnostic {
    /// Discovery path, resolved manifest entry, or `<builtins>`.
    pub path: String,
    /// Typed classification, spawn, handshake, or load failure text.
    pub message: String,
}

impl std::fmt::Display for ExtensionSetDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Extension \"{}\" error: {}", self.path, self.message)
    }
}

/// Prepared replacement held between prepare and commit of a reload.
pub(crate) struct PreparedReload {
    generation: Option<Generation>,
    pending: PendingBridges,
    diagnostics: Vec<ExtensionSetDiagnostic>,
}

#[cfg(test)]
impl PreparedReload {
    pub(crate) fn empty_for_test() -> Self {
        Self {
            generation: None,
            pending: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl Drop for PreparedReload {
    fn drop(&mut self) {
        if let Some(generation) = self.generation.take() {
            abort_bridges(&generation);
            for endpoint in generation.endpoints.iter() {
                let runner = Arc::clone(&endpoint.runner);
                spawn_runtime_safe("prepared-reload-shutdown", async move {
                    runner.shutdown_once().await;
                });
            }
        }
    }
}

/// Prepared resources owned by the facade while a replacement waits for host readiness.
pub(crate) enum PendingReadyOp {
    /// A fully-created session runtime waiting to replace the current runtime.
    Replacement {
        /// New runtime state. It is neither applied nor disposed before the ready decision.
        result: CreateAgentSessionRuntimeResult,
        /// Shutdown reason emitted by the old session during finalization.
        reason: SessionShutdownReason,
        /// Session file that will replace the old session file, when known.
        target_session_file: Option<String>,
    },
    /// A prepared extension generation waiting to replace the published generation.
    Reload {
        /// Unpublished extension generation.
        prepared: PreparedReload,
        /// Provider registry receiving the committed generation.
        model_runtime: Arc<ModelRuntime>,
    },
}

impl PendingReadyOp {
    fn replacement_target(&self) -> Option<Arc<AgentSession>> {
        match self {
            Self::Replacement { result, .. } => Some(Arc::clone(&result.session)),
            Self::Reload { .. } => None,
        }
    }
}

enum PendingReadyState {
    None,
    Pending {
        op: PendingReadyOp,
        token: String,
        ready_tx: oneshot::Sender<()>,
        owner: Option<EndpointId>,
    },
    Finalizing {
        op: Option<PendingReadyOp>,
        token: String,
        owner: Option<EndpointId>,
        replacement_target: Option<Arc<AgentSession>>,
    },
}

/// Authority carried by the current committed session target. This tag lets
/// a publisher prove it was bound before a subsequent rebind changed target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionTargetBinding(u64);

/// Centralized routing result so bridge consumers do not duplicate state
/// matching. Created by `ExtensionRuntimeSet::route_session_bridge`.
pub(crate) enum SessionBridgeRoute {
    /// Route to the committed session and retain its publication authority.
    Active {
        target: Arc<AgentSession>,
        binding: SessionTargetBinding,
    },
    /// Route to the exact token-and-owner candidate without publishing its mirror.
    Candidate(Arc<AgentSession>),
    /// Apply an exact token-and-owner readiness operation.
    Operation,
    /// Drop or answer an event that does not carry current authority.
    Rejected,
}

#[derive(Clone, Copy, Debug)]
enum TaggedBridgeKind {
    Candidate,
    Operation,
}

enum BridgeScope<'a> {
    Untagged,
    Tagged {
        token: &'a str,
        origin: Option<EndpointId>,
        kind: TaggedBridgeKind,
    },
}

/// Releases the finalizing slot when the finalizer completes or unwinds.
///
/// Returned from [`ExtensionRuntimeSet::take_finalizing`] alongside the
/// transferred operation. If the finalizer task panics or returns early
/// without calling [`ExtensionRuntimeSet::finish_finalize`], this guard's
/// `Drop` impl clears the slot so future replacements are not wedged.
pub(crate) struct FinalizeGuard {
    set: Weak<ExtensionRuntimeSet>,
    token: String,
}

impl Drop for FinalizeGuard {
    fn drop(&mut self) {
        if let Some(set) = self.set.upgrade() {
            // Idempotent: a normal finish already cleared the slot.
            let _ = set.finish_finalize(&self.token);
        }
    }
}

/// Outcome of a committed extension-runtime reload.
pub(crate) struct ReloadResult {
    /// Classification, load, flag, and provider diagnostics collected across prepare/commit.
    pub diagnostics: Vec<ExtensionSetDiagnostic>,
    /// Whether the prepared generation was published.
    pub(crate) committed: bool,
}

/// Result of best-effort cold startup.
pub struct ExtensionSetStart {
    /// Stable facade, absent only when no endpoint became ready.
    pub set: Option<Arc<ExtensionRuntimeSet>>,
    /// Ordered path-scoped failures.
    pub diagnostics: Vec<ExtensionSetDiagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EndpointId {
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

pub(crate) struct Generation {
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
}

struct GenerationLease {
    generation: Arc<Generation>,
    counted: bool,
}

impl GenerationLease {
    fn endpoints(&self) -> &[Endpoint] {
        if self.counted {
            &self.generation.endpoints
        } else {
            &[]
        }
    }

    fn live_endpoints(&self) -> impl DoubleEndedIterator<Item = &Endpoint> {
        self.endpoints()
            .iter()
            .filter(|endpoint| endpoint.runner.is_active())
    }

    fn is_active(&self) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.runner.is_active())
    }

    fn is_running(&self) -> bool {
        self.endpoints()
            .iter()
            .any(|endpoint| endpoint.runner.is_running())
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        if self.counted && self.generation.leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.generation.drained.notify_one();
        }
    }
}

pub(crate) struct PendingEndpointBridges {
    endpoint: Endpoint,
    tool_updates: broadcast::Receiver<ToolUpdate>,
    provider_events: broadcast::Receiver<ProviderEvent>,
    errors: broadcast::Receiver<ExtensionErrorEvent>,
    ui: broadcast::Receiver<ExtensionUiEvent>,
    ui_requests: Option<mpsc::Receiver<HostUiRequest>>,
    session_bridge: Option<mpsc::Receiver<SessionBridgeEvent>>,
    providers_update: Option<watch::Receiver<pi_ext::protocol::ProvidersUpdate>>,
    slots: Vec<SanitizedSlot>,
}

type PendingBridges = Vec<PendingEndpointBridges>;

/// Every relay for one endpoint shares this stable routing identity.
struct EndpointRelayContext {
    state: Weak<StdMutex<PublishedRuntimeState>>,
    channels: Arc<FacadeChannels>,
    endpoint: Endpoint,
    replacement_ready_drop: Weak<StdMutex<PendingReadyState>>,
}

#[derive(Clone, Copy)]
struct CorrelationRoute {
    endpoint: EndpointId,
    local: FrameId,
}

struct PublishedRuntimeState {
    generation: Arc<Generation>,
    slots: HashMap<String, BTreeMap<EndpointId, SanitizedSlot>>,
    routes: HashMap<FrameId, CorrelationRoute>,
    retired: BTreeSet<EndpointId>,
    provider_runtime: Option<ModelRuntime>,
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
            retired: BTreeSet::new(),
            provider_runtime: None,
            next_route_id: 1,
            stale: false,
            shutdown_done: false,
        }
    }

    fn lease(&self) -> GenerationLease {
        let counted = !self.stale && !self.shutdown_done;
        if counted {
            self.generation.leases.fetch_add(1, Ordering::Relaxed);
        }
        GenerationLease {
            generation: Arc::clone(&self.generation),
            counted,
        }
    }

    fn is_current_generation_endpoint(&self, endpoint: EndpointId) -> bool {
        self.generation.endpoint(endpoint).is_some()
    }

    fn accepts_relay(&self, endpoint: EndpointId) -> bool {
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
    fn apply_providers_update(
        &mut self,
        endpoint: &Endpoint,
        update: &pi_ext::protocol::ProvidersUpdate,
    ) {
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

    fn retire_endpoint(&mut self, endpoint: EndpointId, channels: &FacadeChannels) -> bool {
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

    fn reloadable(&self) -> bool {
        !self.stale && !self.shutdown_done && self.generation.has_one_active_compat_endpoint()
    }

    fn allocate_route(&mut self, endpoint: EndpointId, local: FrameId) -> Option<FrameId> {
        if !self.accepts_relay(endpoint) {
            return None;
        }
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
        if !self.accepts_relay(endpoint) {
            return;
        }
        let owners = self.slots.entry(slot.key.clone()).or_default();
        owners.insert(endpoint, slot.clone());
        if owners.last_key_value().map(|(owner, _)| *owner) == Some(endpoint) {
            let _ = channels.ui_tx.send(ExtensionUiEvent::Slot(slot));
        }
    }

    fn dispose_slot(&mut self, endpoint: EndpointId, key: String, channels: &FacadeChannels) {
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

    fn slot_owner(&self, key: &str) -> Option<(GenerationLease, Endpoint)> {
        let endpoint = *self.slots.get(key)?.last_key_value()?.0;
        if !self.accepts_relay(endpoint) {
            return None;
        }
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

    fn quiesce(&mut self, channels: &FacadeChannels) -> Arc<Generation> {
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
/// State that owns the bound session target and its mirror publication authority.
struct SessionTargetState {
    target: Weak<AgentSession>,
    binding: Option<SessionTargetBinding>,
    published: bool,
    next_binding: u64,
}

impl SessionTargetState {
    fn new() -> Self {
        Self {
            target: Weak::new(),
            binding: None,
            published: false,
            next_binding: 1,
        }
    }

    fn bind(&mut self, target: Weak<AgentSession>) -> SessionTargetBinding {
        if self.target.ptr_eq(&target)
            && let Some(binding) = self.binding
        {
            return binding;
        }
        let binding = SessionTargetBinding(self.next_binding);
        self.next_binding = self.next_binding.wrapping_add(1).max(1);
        self.target = target;
        self.binding = Some(binding);
        self.published = false;
        binding
    }

    fn route(&self) -> Option<(Arc<AgentSession>, SessionTargetBinding)> {
        Some((self.target.upgrade()?, self.binding?))
    }

    fn binding_for(&self, target: &Arc<AgentSession>) -> Option<SessionTargetBinding> {
        let (current, binding) = self.route()?;
        (self.published && Arc::ptr_eq(&current, target)).then_some(binding)
    }

    fn is_current(&self, binding: SessionTargetBinding) -> bool {
        self.binding == Some(binding) && self.target.strong_count() != 0
    }

    fn is_published(&self, binding: SessionTargetBinding) -> bool {
        self.published && self.is_current(binding)
    }

    fn publish(&mut self, binding: SessionTargetBinding) -> bool {
        if !self.is_current(binding) {
            return false;
        }
        self.published = true;
        true
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
    session_target: StdMutex<SessionTargetState>,
    registry_revision_tx: watch::Sender<u64>,
}

impl FacadeChannels {
    fn new() -> Self {
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
            session_target: StdMutex::new(SessionTargetState::new()),
            registry_revision_tx,
        }
    }

    fn publish_registry_change(&self) {
        self.registry_revision_tx
            .send_modify(|revision| *revision = revision.wrapping_add(1));
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
    command_source_infos: StdMutex<HashMap<String, SourceInfo>>,
    load_cwd: String,
    project_trusted: bool,
    reload_lock: tokio::sync::Mutex<()>,
    session_publish_lock: tokio::sync::Mutex<()>,
    pending_ready: Arc<StdMutex<PendingReadyState>>,
    next_replacement_token_id: AtomicU64,
    #[cfg(test)]
    test_prepared_reload: StdMutex<Option<TestPreparedReload>>,
    #[cfg(test)]
    test_abort_prepare_after_flags: StdMutex<bool>,
}

#[cfg(test)]
enum TestPreparedReload {
    Replacement {
        generation: Generation,
        pending: PendingBridges,
    },
    ReplacementWithDiagnostics {
        generation: Generation,
        pending: PendingBridges,
        diagnostics: Vec<ExtensionSetDiagnostic>,
    },
    ReplacementThenInvalidation {
        generation: Generation,
        pending: PendingBridges,
    },
    ReplacementThenFatalPreparationFailure {
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
        let GenerationBuild {
            generation,
            pending,
            diagnostics: mut build_diagnostics,
            endpoint_start_failure: _,
        } = build_generation(
            1,
            plans,
            &load_cwd,
            project_trusted,
            GenerationBuildPolicy::BestEffortStart,
        )
        .await;
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
            command_source_infos: StdMutex::new(HashMap::new()),
            load_cwd,
            project_trusted,
            reload_lock: tokio::sync::Mutex::new(()),
            session_publish_lock: tokio::sync::Mutex::new(()),
            pending_ready: Arc::new(StdMutex::new(PendingReadyState::None)),
            next_replacement_token_id: AtomicU64::new(1),
            #[cfg(test)]
            test_prepared_reload: StdMutex::new(None),
            #[cfg(test)]
            test_abort_prepare_after_flags: StdMutex::new(false),
        }
    }

    /// Install resource-loader provenance for command-owning extension paths.
    pub fn set_command_source_infos(&self, infos: impl IntoIterator<Item = (String, SourceInfo)>) {
        let mut current = self
            .command_source_infos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.clear();
        current.extend(infos);
    }

    /// Resolve resource-loader provenance for one command-owning path.
    #[must_use]
    pub fn command_source_info(&self, path: &str) -> Option<SourceInfo> {
        self.command_source_infos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
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

    fn claim_route_and_bind_owner(
        &self,
        id: FrameId,
        bind_token: Option<&str>,
    ) -> Option<(GenerationLease, Endpoint, FrameId)> {
        // Keep route claim and owner binding atomic against endpoint retirement.
        let mut state = self.state();
        let claimed = state.claim_route(id)?;
        if let Some(bind_token) = bind_token {
            let mut pending = self.pending_ready();
            if let PendingReadyState::Pending { token, owner, .. } = &mut *pending
                && token == bind_token
            {
                *owner = Some(claimed.1.id);
            }
        }
        Some(claimed)
    }
    fn pending_ready(&self) -> std::sync::MutexGuard<'_, PendingReadyState> {
        self.pending_ready
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mint a facade-scoped replacement token safe to serialize across JavaScript.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub(crate) fn next_replacement_token(&self) -> String {
        self.next_replacement_token_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("replacement token space exhausted")
            .to_string()
    }

    /// Install the sole ready-gated operation before its success response is written.
    ///
    /// On conflict, ownership of `op` is returned to the caller for explicit cleanup.
    #[allow(clippy::result_large_err)]
    pub(crate) fn install_pending(
        &self,
        token: String,
        op: PendingReadyOp,
    ) -> Result<oneshot::Receiver<()>, PendingReadyOp> {
        let mut state = self.pending_ready();
        if !matches!(*state, PendingReadyState::None) {
            return Err(op);
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        *state = PendingReadyState::Pending {
            op,
            token,
            ready_tx,
            owner: None,
        };
        Ok(ready_rx)
    }

    /// Correlate a host-ready event and advance its operation into finalization.
    ///
    /// Returns false for stale/mismatched tokens and for a waiter that was already dropped.
    pub(crate) fn complete_ready(&self, token: &str) -> bool {
        let mut state = self.pending_ready();
        let current = std::mem::replace(&mut *state, PendingReadyState::None);
        let (op, pending_token, ready_tx, owner) = match current {
            PendingReadyState::Pending {
                op,
                token,
                ready_tx,
                owner,
            } => (op, token, ready_tx, owner),
            other => {
                *state = other;
                return false;
            }
        };
        if pending_token != token {
            *state = PendingReadyState::Pending {
                op,
                token: pending_token,
                ready_tx,
                owner,
            };
            return false;
        }
        let replacement_target = op.replacement_target();
        *state = PendingReadyState::Finalizing {
            op: Some(op),
            token: pending_token,
            owner,
            replacement_target,
        };
        if ready_tx.send(()).is_ok() {
            return true;
        }
        let drained = match std::mem::replace(&mut *state, PendingReadyState::None) {
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None | PendingReadyState::Pending { .. } => None,
        };
        drop(state);
        if let Some(op) = drained {
            discard_pending_ready_op(op);
        }
        false
    }

    /// Transfer a token-correlated operation to its finalizer while retaining slot ownership.
    ///
    /// Returns the operation and a [`FinalizeGuard`] that releases the
    /// finalizing slot on drop. If the finalizer completes normally it
    /// should call [`ExtensionRuntimeSet::finish_finalize`] to clear
    /// the slot explicitly; if it panics or returns early, the guard's
    /// `Drop` impl clears the slot so future replacements are not wedged.
    pub(crate) fn take_finalizing(
        self: &Arc<Self>,
        token: &str,
    ) -> Option<(PendingReadyOp, FinalizeGuard)> {
        let mut state = self.pending_ready();
        match &mut *state {
            PendingReadyState::Finalizing {
                op,
                token: pending_token,
                ..
            } if pending_token == token => op.take().map(|op| {
                let guard = FinalizeGuard {
                    set: Arc::downgrade(self),
                    token: token.to_owned(),
                };
                (op, guard)
            }),
            PendingReadyState::None
            | PendingReadyState::Pending { .. }
            | PendingReadyState::Finalizing { .. } => None,
        }
    }

    /// Release the finalizing slot after the transferred operation was applied.
    ///
    /// Tolerates an already-cleared slot: if the slot was cleared by a
    /// [`FinalizeGuard`] drop or a prior call, this returns `true` so
    /// callers that race a guard drop with an explicit finish do not
    /// observe a spurious failure.
    pub(crate) fn finish_finalize(&self, token: &str) -> bool {
        let mut state = self.pending_ready();
        match &*state {
            PendingReadyState::Finalizing {
                op: None,
                token: pending_token,
                ..
            } if pending_token == token => {
                *state = PendingReadyState::None;
                true
            }
            // Slot already cleared (e.g. by a guard drop racing this call).
            PendingReadyState::None => true,
            _ => false,
        }
    }

    /// Abort an operation that has not been transferred to a finalizer.
    #[must_use]
    pub(crate) fn abort_pending(&self, token: &str) -> Option<PendingReadyOp> {
        let mut state = self.pending_ready();
        let matches = match &*state {
            PendingReadyState::Pending {
                token: pending_token,
                ..
            }
            | PendingReadyState::Finalizing {
                token: pending_token,
                op: Some(_),
                ..
            } => pending_token == token,
            PendingReadyState::None | PendingReadyState::Finalizing { op: None, .. } => false,
        };
        if !matches {
            return None;
        }
        match std::mem::replace(&mut *state, PendingReadyState::None) {
            PendingReadyState::Pending { op, .. } => Some(op),
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None => None,
        }
    }

    /// Abort an exact token that is still waiting for host readiness.
    ///
    /// Accepted readiness is irreversible: `Finalizing` states are never removed.
    pub(crate) fn abort_waiting_ready(&self, token: &str, owner: Option<EndpointId>) -> bool {
        let Some(op) = abort_pending_ready_drop(&self.pending_ready, token, owner) else {
            return false;
        };
        discard_pending_ready_op(op);
        true
    }

    /// Weak handle to the pending-ready slot for relay drop callbacks.
    #[must_use]
    fn pending_ready_weak(&self) -> Weak<StdMutex<PendingReadyState>> {
        Arc::downgrade(&self.pending_ready)
    }

    /// Drain any facade-owned prepared resources and wake a pending waiter.
    #[must_use]
    pub(crate) fn drain_pending(&self) -> Option<PendingReadyOp> {
        match std::mem::replace(&mut *self.pending_ready(), PendingReadyState::None) {
            PendingReadyState::Pending { op, .. } => Some(op),
            PendingReadyState::Finalizing { op, .. } => op,
            PendingReadyState::None => None,
        }
    }

    /// Whether a ready-gated operation is pending or finalizing.
    #[must_use]
    pub(crate) fn is_pending_busy(&self) -> bool {
        !matches!(*self.pending_ready(), PendingReadyState::None)
    }

    #[cfg(test)]
    pub(crate) fn inject_prepared_replacement_for_reload(
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
    fn inject_prepared_replacement_with_diagnostics_for_reload(
        &self,
        generation: Generation,
        pending: PendingBridges,
        diagnostics: Vec<ExtensionSetDiagnostic>,
    ) {
        let mut prepared = self
            .test_prepared_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            prepared.is_none(),
            "reload test preparation was already injected"
        );
        *prepared = Some(TestPreparedReload::ReplacementWithDiagnostics {
            generation,
            pending,
            diagnostics,
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

    #[cfg(test)]
    fn inject_prepared_replacement_then_fatal_preparation_failure(
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
        *prepared = Some(TestPreparedReload::ReplacementThenFatalPreparationFailure {
            generation,
            pending,
        });
    }

    async fn build_reload_generation(&self, id: u64, plans: Vec<EndpointPlan>) -> GenerationBuild {
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
                    } => GenerationBuild {
                        generation: Some(generation),
                        pending,
                        diagnostics: Vec::new(),
                        endpoint_start_failure: None,
                    },
                    TestPreparedReload::ReplacementWithDiagnostics {
                        generation,
                        pending,
                        diagnostics,
                    } => GenerationBuild {
                        generation: Some(generation),
                        pending,
                        diagnostics,
                        endpoint_start_failure: None,
                    },
                    TestPreparedReload::ReplacementThenInvalidation {
                        generation,
                        pending,
                    } => {
                        self.invalidate();
                        GenerationBuild {
                            generation: Some(generation),
                            pending,
                            diagnostics: Vec::new(),
                            endpoint_start_failure: None,
                        }
                    }
                    TestPreparedReload::ReplacementThenFatalPreparationFailure {
                        generation,
                        pending,
                    } => {
                        *self
                            .test_abort_prepare_after_flags
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                        GenerationBuild {
                            generation: Some(generation),
                            pending,
                            diagnostics: Vec::new(),
                            endpoint_start_failure: None,
                        }
                    }
                };
            }
        }
        build_generation(
            id,
            plans,
            &self.load_cwd,
            self.project_trusted,
            GenerationBuildPolicy::BestEffortStart,
        )
        .await
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
                providers_update,
                slots: _,
            } = pending_endpoint;
            endpoint.runner.set_endpoint_id(endpoint.id);
            let pending_ready_weak = self.pending_ready_weak();
            // Rejecting any token-bearing control at the first bounded hop
            // aborts only the matching operation from this endpoint.
            let drop_pending = Weak::clone(&pending_ready_weak);
            endpoint.runner.set_replacement_drop_handler(Arc::new(
                move |token: &str, origin: Option<EndpointId>| {
                    abort_dropped_pending_ready(&drop_pending, token, origin);
                },
            ));
            let context = EndpointRelayContext {
                state: Arc::downgrade(&self.state),
                channels: Arc::clone(&self.channels),
                endpoint,
                replacement_ready_drop: Weak::clone(&pending_ready_weak),
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
            if let Some(handle) = spawn_providers_update_relay(&context, providers_update) {
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
        for endpoint in lease.live_endpoints() {
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
        for endpoint in lease.live_endpoints() {
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
        let mut state = self.state();
        let results = register_endpoint_providers(
            state.generation.endpoints.iter().filter(|endpoint| {
                !state.retired.contains(&endpoint.id) && endpoint.runner.is_active()
            }),
            runtime,
        );
        state.provider_runtime = Some(runtime.clone());
        results
    }

    /// Remove each first-owned provider once.
    pub fn unregister_providers_from(&self, runtime: &ModelRuntime) {
        let mut state = self.state();
        unregister_endpoint_providers(&state.generation.endpoints, runtime);
        state.provider_runtime = None;
    }

    /// Aggregate registry with existing first-wins semantics.
    #[must_use]
    pub fn registry(&self) -> Registry {
        let lease = self.lease();
        let mut aggregate = Registry::new();
        for endpoint in lease.live_endpoints() {
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
            .live_endpoints()
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
    /// Returns path-qualified diagnostics for per-endpoint failures without aborting siblings.
    /// An empty overlay returns `Ok(vec![])` without contacting endpoints.
    ///
    /// # Errors
    ///
    /// Returns a runner-global failure only. Per-endpoint errors are collected into the Ok
    /// diagnostics list.
    pub async fn apply_flag_values(
        &self,
        values: &BTreeMap<String, FlagValueWire>,
    ) -> Result<Vec<ExtensionSetDiagnostic>, HostClientError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let lease = self.lease();
        let mut diagnostics = Vec::new();
        for endpoint in lease.live_endpoints() {
            if let Err(error) = endpoint.runner.apply_flag_values(values).await {
                diagnostics.push(ExtensionSetDiagnostic {
                    path: endpoint_diagnostic_path(endpoint),
                    message: error.to_string(),
                });
            }
        }
        Ok(diagnostics)
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
        for endpoint in lease.live_endpoints().rev() {
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

    /// Subscribe to aggregate extension registry changes.
    #[must_use]
    pub(crate) fn subscribe_registry_changes(&self) -> watch::Receiver<u64> {
        self.channels.registry_revision_tx.subscribe()
    }

    /// Whether any active endpoint handles terminal input.
    #[must_use]
    pub fn has_terminal_input_handlers(&self) -> bool {
        let lease = self.lease();
        lease.live_endpoints().any(|endpoint| {
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
        self.terminal_input_within(data, TERMINAL_INPUT_DEADLINE)
            .await
    }

    /// Deadline-injectable terminal-input fan-out for tests.
    ///
    /// Fans the original data to every participant under one shared deadline,
    /// forwarding that deadline to each endpoint request so a generous test
    /// deadline exercises the full routing path instead of racing the host
    /// transport under the production 4 ms budget. Production callers use
    /// [`Self::terminal_input`] (4 ms).
    ///
    /// # Errors
    ///
    /// Never returns `Err`; unavailable and late endpoint responses are represented in the result.
    pub(crate) async fn terminal_input_within(
        &self,
        data: &str,
        deadline: Duration,
    ) -> Result<protocol::TerminalInputResult, HostClientError> {
        let lease = self.lease();
        let mut pending = FuturesUnordered::new();
        for (index, endpoint) in lease.live_endpoints().enumerate() {
            if endpoint.runner.has_terminal_input_handlers() {
                let runner = Arc::clone(&endpoint.runner);
                let data = data.to_owned();
                pending.push(async move {
                    (index, runner.terminal_input_within(&data, deadline).await)
                });
            }
        }
        if pending.is_empty() {
            return Ok(protocol::TerminalInputResult::default());
        }
        let cutoff = tokio::time::Instant::now() + deadline;
        let mut replies = vec![None; lease.endpoints().len()];
        while !pending.is_empty() {
            match tokio::time::timeout_at(cutoff, pending.next()).await {
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
            .live_endpoints()
            .map(|endpoint| endpoint.runner.theme_generation())
            .max()
            .unwrap_or(0)
    }

    /// Broadcast a theme update to all active endpoints.
    pub async fn push_theme_update(&self, update: &ThemeUpdate) {
        let lease = self.lease();
        let mut sends = FuturesUnordered::new();
        for endpoint in lease.live_endpoints() {
            sends.push(endpoint.runner.push_theme_update(update));
        }
        while sends.next().await.is_some() {}
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
    pub(crate) fn take_session_bridge(&self) -> Option<mpsc::Receiver<SessionBridgeEvent>> {
        self.channels
            .session_bridge_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Bind the session that receives facade commands after ready finalization.
    pub(crate) async fn bind_session_target(
        &self,
        session: Weak<AgentSession>,
    ) -> SessionTargetBinding {
        let _publish = self.session_publish_lock.lock().await;
        self.channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bind(session)
    }

    /// Return the binding only when `session` owns the published mirror.
    #[must_use]
    pub(crate) fn session_binding_for(
        &self,
        session: &Arc<AgentSession>,
    ) -> Option<SessionTargetBinding> {
        self.channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .binding_for(session)
    }

    /// Commit an already-bound replacement and release its exact token.
    pub(crate) async fn commit_session_replacement(
        &self,
        token: &str,
    ) -> Option<(Arc<AgentSession>, SessionTargetBinding)> {
        let _publish = self.session_publish_lock.lock().await;
        let mut pending = self.pending_ready();
        let expected = match &*pending {
            PendingReadyState::Finalizing {
                op: None,
                token: pending_token,
                replacement_target: Some(target),
                ..
            } if pending_token == token => Arc::clone(target),
            _ => return None,
        };
        let (target, binding) = self
            .channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .route()?;
        if !Arc::ptr_eq(&target, &expected) {
            return None;
        }
        *pending = PendingReadyState::None;
        Some((target, binding))
    }

    /// Whether a mirror publisher still owns the committed session binding.
    #[must_use]
    pub(crate) fn is_session_target_current(&self, binding: SessionTargetBinding) -> bool {
        self.channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_current(binding)
    }

    /// Route one bridge item from the sole token, owner, and session authority.
    #[must_use]
    pub(crate) fn route_session_bridge(&self, event: &SessionBridgeEvent) -> SessionBridgeRoute {
        let scope = match event {
            SessionBridgeEvent::Command { envelope, origin } => {
                match envelope.replacement_token.as_deref() {
                    Some(token) => BridgeScope::Tagged {
                        token,
                        origin: *origin,
                        kind: TaggedBridgeKind::Candidate,
                    },
                    None => BridgeScope::Untagged,
                }
            }
            SessionBridgeEvent::SetupEntries {
                request, origin, ..
            } => BridgeScope::Tagged {
                token: &request.replacement_token,
                origin: *origin,
                kind: TaggedBridgeKind::Candidate,
            },
            SessionBridgeEvent::ReplacementReady { token, origin }
            | SessionBridgeEvent::ReplacementAbort { token, origin } => BridgeScope::Tagged {
                token,
                origin: *origin,
                kind: TaggedBridgeKind::Operation,
            },
            SessionBridgeEvent::SetModel { .. }
            | SessionBridgeEvent::Compact { .. }
            | SessionBridgeEvent::NewSession { .. }
            | SessionBridgeEvent::Fork { .. }
            | SessionBridgeEvent::NavigateTree { .. }
            | SessionBridgeEvent::SwitchSession { .. }
            | SessionBridgeEvent::Reload { .. } => BridgeScope::Untagged,
        };

        if let BridgeScope::Tagged {
            token,
            origin,
            kind,
        } = scope
        {
            let state = self.pending_ready();
            let owner_matches = |owner: Option<EndpointId>| owner == origin;
            return match (&*state, kind) {
                (
                    PendingReadyState::Pending {
                        op,
                        token: current,
                        owner,
                        ..
                    },
                    TaggedBridgeKind::Candidate,
                ) if current == token && owner_matches(*owner) => op
                    .replacement_target()
                    .map_or(SessionBridgeRoute::Rejected, SessionBridgeRoute::Candidate),
                (
                    PendingReadyState::Finalizing {
                        token: current,
                        owner,
                        replacement_target,
                        ..
                    },
                    TaggedBridgeKind::Candidate,
                ) if current == token && owner_matches(*owner) => replacement_target
                    .clone()
                    .map_or(SessionBridgeRoute::Rejected, SessionBridgeRoute::Candidate),
                (
                    PendingReadyState::Pending {
                        token: current,
                        owner,
                        ..
                    },
                    TaggedBridgeKind::Operation,
                ) if current == token && owner_matches(*owner) => SessionBridgeRoute::Operation,
                _ => SessionBridgeRoute::Rejected,
            };
        }

        let target = self
            .channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .route();
        match target {
            Some((target, binding)) => SessionBridgeRoute::Active { target, binding },
            None => SessionBridgeRoute::Rejected,
        }
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

    /// Route a correlated new-session response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_new_session(
        &self,
        id: FrameId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), HostClientError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_new_session(local, cancelled, token)
            .await
    }

    /// Route a correlated fork response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_fork(
        &self,
        id: FrameId,
        cancelled: bool,
        selected_text: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), HostClientError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_fork(local, cancelled, selected_text, token)
            .await
    }

    /// Route a correlated tree-navigation response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_navigate_tree(
        &self,
        id: FrameId,
        outcome: Result<protocol::SessionNavigateTreeResponse, String>,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint.runner.respond_navigate_tree(local, outcome).await
    }

    /// Route a correlated switch-session response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_switch_session(
        &self,
        id: FrameId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), HostClientError> {
        let bind_token = (!cancelled).then_some(token).flatten();
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_switch_session(local, cancelled, token)
            .await
    }

    /// Route a correlated reload response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_reload(
        &self,
        id: FrameId,
        outcome: Result<Option<&str>, String>,
    ) -> Result<(), HostClientError> {
        let bind_token = outcome.as_ref().ok().and_then(|token| *token);
        let Some((_lease, endpoint, local)) = self.claim_route_and_bind_owner(id, bind_token)
        else {
            return Err(HostClientError::NotRunning);
        };
        endpoint.runner.respond_reload(local, outcome).await
    }

    /// Route a correlated setup-entries response to its originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or missing, or its host rejects it.
    pub async fn respond_setup_entries(
        &self,
        id: FrameId,
        outcome: Result<SessionSetupEntriesResponse, String>,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint.runner.respond_setup_entries(local, outcome).await
    }

    /// Validate a replacement token and return the pending replacement target
    /// session. Returns `None` for stale or missing tokens (fail closed).
    #[must_use]
    pub(crate) fn validate_setup_token(&self, token: &str) -> Option<Arc<AgentSession>> {
        let state = self.pending_ready();
        match &*state {
            PendingReadyState::Pending {
                op,
                token: pending_token,
                ..
            } if pending_token == token => op.replacement_target(),
            PendingReadyState::Finalizing {
                replacement_target,
                token: pending_token,
                ..
            } if pending_token == token => replacement_target.clone(),
            _ => None,
        }
    }

    /// Route a deterministic busy rejection to the request's originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_replacement_busy(
        &self,
        id: FrameId,
        method: &str,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_replacement_busy(local, method)
            .await
    }

    /// Route an `extension_error` response to the request's originating endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the response route is stale or its host rejects it.
    pub async fn respond_session_error(
        &self,
        id: FrameId,
        method: &str,
        message: &str,
    ) -> Result<(), HostClientError> {
        let Some((_lease, endpoint, local)) = self.state().claim_route(id) else {
            return Err(HostClientError::NotRunning);
        };
        endpoint
            .runner
            .respond_session_error(local, method, message)
            .await
    }

    /// Publish the first mirror snapshot and grant this binding publication authority.
    pub(crate) async fn activate_session_state(
        &self,
        binding: SessionTargetBinding,
        state: &SessionStateWire,
    ) -> bool {
        let _publish = self.session_publish_lock.lock().await;
        if !self
            .channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_current(binding)
        {
            return false;
        }
        self.broadcast_session_state(state).await;
        self.channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .publish(binding)
    }

    /// Broadcast mirrored session state only for the published binding.
    pub(crate) async fn push_session_state_for_binding(
        &self,
        binding: SessionTargetBinding,
        state: &SessionStateWire,
    ) -> bool {
        let _publish = self.session_publish_lock.lock().await;
        if !self
            .channels
            .session_target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_published(binding)
        {
            return false;
        }
        self.broadcast_session_state(state).await;
        true
    }

    async fn broadcast_session_state(&self, state: &SessionStateWire) {
        let lease = self.lease();
        let mut sends = FuturesUnordered::new();
        for endpoint in lease.live_endpoints() {
            sends.push(endpoint.runner.push_session_state(state));
        }
        while sends.next().await.is_some() {}
    }

    /// Broadcast mirrored UI state.
    pub async fn push_ui_state(&self, state: &UiStateWire) {
        let lease = self.lease();
        let mut sends = FuturesUnordered::new();
        for endpoint in lease.live_endpoints() {
            sends.push(endpoint.runner.push_ui_state(state));
        }
        while sends.next().await.is_some() {}
    }

    /// Render with the first endpoint owning the tool renderer.
    pub async fn render_extension_tool_html(
        &self,
        phase: ToolRenderPhase,
        tool_name: &str,
        payload: &Value,
    ) -> Option<String> {
        let lease = self.lease();
        for endpoint in lease.live_endpoints() {
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
    /// Short-lived serialization lock for reload prepare or commit phases.
    ///
    /// Callers must not hold this lock while awaiting a host callback.
    #[must_use]
    pub(crate) fn reload_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.reload_lock
    }

    /// Prepare a reload replacement without mutating published providers or bridges.
    ///
    /// The caller must hold [`Self::reload_lock`] for this preparation phase.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime is no longer reloadable, no replacement endpoint starts,
    /// flag encoding/sync fails globally, replacement provider validation fails, or the facade is
    /// invalidated after flags are applied.
    pub(crate) async fn prepare_reload(
        &self,
        preserved_flags: HashMap<String, Value>,
    ) -> Result<PreparedReload, HostStartError> {
        if !self.state().reloadable() {
            return Err(HostStartError::Load(
                "extension runtime is not reloadable".to_owned(),
            ));
        }
        let flags = encode_flags(preserved_flags)?;
        let (classified, mut diagnostics) = classify_paths(&self.discovered_paths);
        let plans = plan_endpoints(&classified);
        let next_id = self
            .reload_generation()
            .checked_add(1)
            .ok_or_else(|| HostStartError::Load("extension generation exhausted".to_owned()))?;
        let GenerationBuild {
            generation: next,
            pending,
            diagnostics: mut load_diagnostics,
            endpoint_start_failure,
        } = self.build_reload_generation(next_id, plans).await;
        diagnostics.append(&mut load_diagnostics);
        let Some(next) = next else {
            let message = endpoint_start_failure.map_or_else(
                || "no extension endpoint started".to_owned(),
                |diagnostic| diagnostic.to_string(),
            );
            return Err(HostStartError::Load(message));
        };
        match apply_flags_to_generation(&next, &flags).await {
            Ok(mut flag_diagnostics) => diagnostics.append(&mut flag_diagnostics),
            Err(error) => {
                stop_generation(&next).await;
                return Err(HostStartError::FlagSync(error.to_string()));
            }
        }
        #[cfg(test)]
        {
            let abort = std::mem::replace(
                &mut *self
                    .test_abort_prepare_after_flags
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                false,
            );
            if abort {
                stop_generation(&next).await;
                return Err(HostStartError::FlagSync(
                    "injected post-start preparation failure".to_owned(),
                ));
            }
        }
        // Validate replacement providers before returning success so a failed
        // validation never reaches commit_reload after session_shutdown{reload}
        // has been emitted to the old generation.
        if let Err((path, error)) = validate_generation_providers(&next) {
            diagnostics.push(ExtensionSetDiagnostic {
                path,
                message: error.to_string(),
            });
            stop_generation(&next).await;
            return Err(HostStartError::Load(summarize_diagnostics(&diagnostics)));
        }
        if !self.state().reloadable() {
            stop_generation(&next).await;
            return Err(HostStartError::Load(
                "extension runtime is not reloadable".to_owned(),
            ));
        }
        Ok(PreparedReload {
            generation: Some(next),
            pending,
            diagnostics,
        })
    }

    /// Commit a prepared reload. Provider validation already ran in `prepare_reload`;
    /// the check here is defense-in-depth for direct commit callers.
    pub(crate) async fn commit_reload(
        &self,
        runtime: &ModelRuntime,
        mut prepared: PreparedReload,
    ) -> ReloadResult {
        let mut diagnostics = std::mem::take(&mut prepared.diagnostics);
        let Some(next) = prepared.generation.take() else {
            diagnostics.push(ExtensionSetDiagnostic {
                path: "<reload>".to_owned(),
                message: "prepared reload carried no replacement generation".to_owned(),
            });
            return ReloadResult {
                diagnostics,
                committed: false,
            };
        };
        let pending = std::mem::take(&mut prepared.pending);

        if let Err((path, error)) = validate_generation_providers(&next) {
            diagnostics.push(ExtensionSetDiagnostic {
                path,
                message: error.to_string(),
            });
            stop_generation(&next).await;
            return ReloadResult {
                diagnostics,
                committed: false,
            };
        }

        let next = Arc::new(next);
        let old = {
            let mut state = self.state();
            if state.stale || state.shutdown_done {
                None
            } else {
                let old = Arc::clone(&state.generation);
                unregister_endpoint_providers(&old.endpoints, runtime);
                let registrations = register_endpoint_providers(
                    next.endpoints
                        .iter()
                        .filter(|endpoint| endpoint.runner.is_active()),
                    runtime,
                );
                for (path, outcome) in registrations {
                    if let Err(error) = outcome {
                        diagnostics.push(ExtensionSetDiagnostic {
                            path,
                            message: error.to_string(),
                        });
                    }
                }
                state.provider_runtime = Some(runtime.clone());
                Some(state.replace_generation(Arc::clone(&next), &pending, &self.channels))
            }
        };
        let Some(old) = old else {
            stop_generation(&next).await;
            return ReloadResult {
                diagnostics,
                committed: false,
            };
        };
        self.start_bridges(&next, pending);
        drain_leases(&old).await;
        for endpoint in old.endpoints.iter() {
            endpoint.runner.invalidate();
        }
        abort_bridges(&old);
        stop_generation(&old).await;
        ReloadResult {
            diagnostics,
            committed: true,
        }
    }

    /// Build a complete replacement, preserve the facade, then reap the old generation.
    ///
    /// # Errors
    ///
    /// Returns an error when prepare fails. Commit itself always returns diagnostics.
    #[cfg(test)]
    pub(crate) async fn restart_and_rewire(
        &self,
        runtime: &ModelRuntime,
        preserved_flags: HashMap<String, Value>,
    ) -> Result<ReloadResult, HostStartError> {
        let _reload = self.reload_lock().lock().await;
        let prepared = self.prepare_reload(preserved_flags).await?;
        Ok(self.commit_reload(runtime, prepared).await)
    }

    /// Invalidate all endpoints and synchronously dispose product-visible slots.
    pub fn invalidate(&self) {
        if let Some(op) = self.drain_pending() {
            discard_pending_ready_op(op);
        }
        let Some(generation) = self.state().invalidate(&self.channels) else {
            return;
        };
        for endpoint in generation.endpoints.iter() {
            endpoint.runner.invalidate();
        }
    }

    /// Gracefully stop every endpoint exactly once.
    pub async fn shutdown_once(&self) {
        let _reload = self.reload_lock().lock().await;
        if let Some(op) = self.drain_pending() {
            discard_pending_ready_op(op);
        }
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
                .live_endpoints()
                .any(|endpoint| endpoint.runner.has_handlers(event))
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>> {
        let lease = self.lease();
        Box::pin(async move {
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
                        output.terminate = Some(output.terminate.unwrap_or(false) || terminate);
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
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
        for endpoint in lease.live_endpoints() {
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
            for endpoint in lease.live_endpoints() {
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
        for endpoint in lease.live_endpoints() {
            for (name, tool) in endpoint.runner.get_all_registered_tools() {
                tools.entry(name).or_insert(tool);
            }
        }
        tools
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        let lease = self.lease();
        let mut flags = HashMap::new();
        for endpoint in lease.live_endpoints() {
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
        plans.push(EndpointPlan {
            position: 0,
            kind: EndpointKind::TsCompat,
            entries: Vec::new(),
            diagnostic_paths: Vec::new(),
            builtins: true,
            label: "<builtins>".to_owned(),
        });
    } else if let Some(compat) = plans
        .iter_mut()
        .find(|plan| plan.kind == EndpointKind::TsCompat)
    {
        compat.builtins = true;
    }
    for (position, plan) in plans.iter_mut().enumerate() {
        plan.position = position;
    }
    plans
}

fn endpoint_host_spec(
    plan: &EndpointPlan,
    ts_spec: Option<&Result<HostSpec, host::HostError>>,
) -> Result<HostSpec, String> {
    match plan.kind {
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
        EndpointKind::TsCompat => match ts_spec {
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
    }
}

fn resolve_typescript_host(plans: &[EndpointPlan]) -> Option<Result<HostSpec, host::HostError>> {
    plans
        .iter()
        .any(|plan| plan.kind != EndpointKind::Native)
        .then(host::resolve_host)
}

async fn build_generation(
    id: u64,
    plans: Vec<EndpointPlan>,
    load_cwd: &str,
    project_trusted: bool,
    policy: GenerationBuildPolicy,
) -> GenerationBuild {
    let ts_spec = resolve_typescript_host(&plans);
    build_generation_with_starter(
        id,
        plans,
        load_cwd,
        project_trusted,
        policy,
        ts_spec,
        start_endpoint,
    )
    .await
}

fn collect_generation_starts(results: Vec<EndpointStartOutcome>) -> GenerationStarts {
    let mut endpoints = Vec::new();
    let mut diagnostics = Vec::new();
    let mut endpoint_start_failure = None;
    let mut failed_builtins_owner = None;
    for (position, plan, result) in results {
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
                endpoints.push(PreparedEndpoint {
                    position,
                    kind: plan.kind,
                    label: plan.label.clone(),
                    runner,
                    plan,
                });
            }
            Err(message) => {
                if plan.builtins {
                    failed_builtins_owner = Some(plan.clone());
                }
                let paths = if plan.diagnostic_paths.is_empty() {
                    vec![plan.label]
                } else {
                    plan.diagnostic_paths
                };
                for path in paths {
                    let diagnostic = ExtensionSetDiagnostic {
                        path,
                        message: message.clone(),
                    };
                    if endpoint_start_failure.is_none() {
                        endpoint_start_failure = Some(diagnostic.clone());
                    }
                    diagnostics.push(diagnostic);
                }
            }
        }
    }
    GenerationStarts {
        endpoints,
        diagnostics,
        endpoint_start_failure,
        failed_builtins_owner,
    }
}

#[allow(clippy::too_many_lines)]
async fn build_generation_with_starter<Starter, StartFuture>(
    id: u64,
    plans: Vec<EndpointPlan>,
    load_cwd: &str,
    project_trusted: bool,
    policy: GenerationBuildPolicy,
    ts_spec: Option<Result<HostSpec, host::HostError>>,
    starter: Starter,
) -> GenerationBuild
where
    Starter: Fn(EndpointPlan, HostSpec, String, bool) -> StartFuture + Clone,
    StartFuture: Future<Output = Result<Arc<HostExtensionRunner>, String>>,
{
    let mut starts = FuturesUnordered::new();
    for plan in plans {
        let spec = endpoint_host_spec(&plan, ts_spec.as_ref());
        let cwd = load_cwd.to_owned();
        let starter = starter.clone();
        starts.push(async move {
            let position = plan.position;
            let result = match spec {
                Ok(spec) => starter(plan.clone(), spec, cwd, project_trusted).await,
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
    let GenerationStarts {
        mut endpoints,
        mut diagnostics,
        endpoint_start_failure,
        failed_builtins_owner,
    } = collect_generation_starts(results);

    if matches!(policy, GenerationBuildPolicy::RequireAllEndpointStarts)
        && endpoint_start_failure.is_some()
    {
        let mut stops = endpoints
            .iter()
            .map(|endpoint| endpoint.runner.shutdown_once())
            .collect::<FuturesUnordered<_>>();
        while stops.next().await.is_some() {}
        return GenerationBuild {
            generation: None,
            pending: Vec::new(),
            diagnostics,
            endpoint_start_failure,
        };
    }

    if matches!(policy, GenerationBuildPolicy::BestEffortStart)
        && !endpoints.is_empty()
        && failed_builtins_owner.is_some()
        && let Some(index) = endpoints
            .iter()
            .position(|endpoint| endpoint.kind == EndpointKind::TsCompat && !endpoint.plan.builtins)
    {
        let mut promotion_plan = endpoints[index].plan.clone();
        promotion_plan.builtins = true;
        let result = match endpoint_host_spec(&promotion_plan, ts_spec.as_ref()) {
            Ok(spec) => {
                starter(
                    promotion_plan.clone(),
                    spec,
                    load_cwd.to_owned(),
                    project_trusted,
                )
                .await
            }
            Err(message) => Err(message),
        };
        match result {
            Ok(runner) => {
                for (path, message) in runner.load_errors() {
                    let path = promotion_plan
                        .entries
                        .iter()
                        .position(|entry| entry == &path)
                        .and_then(|entry_index| promotion_plan.diagnostic_paths.get(entry_index))
                        .cloned()
                        .unwrap_or(path);
                    diagnostics.push(ExtensionSetDiagnostic { path, message });
                }
                let position = endpoints[index].position;
                let old = std::mem::replace(
                    &mut endpoints[index],
                    PreparedEndpoint {
                        position,
                        kind: promotion_plan.kind,
                        label: promotion_plan.label.clone(),
                        runner,
                        plan: promotion_plan,
                    },
                );
                old.runner.shutdown_once().await;
            }
            Err(message) => {
                let label = endpoints[index].label.clone();
                diagnostics.push(ExtensionSetDiagnostic {
                    path: label.clone(),
                    message: format!("builtins promotion failed for {label}: {message}"),
                });
            }
        }
    }

    if endpoints.is_empty() {
        return GenerationBuild {
            generation: None,
            pending: Vec::new(),
            diagnostics,
            endpoint_start_failure,
        };
    }
    endpoints.sort_by_key(|endpoint| endpoint.position);
    let endpoints = endpoints
        .into_iter()
        .map(|endpoint| (endpoint.kind, endpoint.label, endpoint.runner))
        .collect();
    let (generation, pending) = generation_from_endpoints(id, endpoints);
    GenerationBuild {
        generation: Some(generation),
        pending,
        diagnostics,
        endpoint_start_failure,
    }
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

pub(crate) fn generation_from_endpoints(
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
            providers_update: endpoint.runner.take_providers_updates(),
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
                    if state.accepts_relay(endpoint) {
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
                    if state.accepts_relay(endpoint) {
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
        spawn_fatal_error_relay(
            Weak::clone(state),
            endpoint,
            error_runner,
            Arc::clone(channels),
            label.clone(),
            errors,
            Weak::clone(&context.replacement_ready_drop),
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
                        other if state.accepts_relay(endpoint) => {
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
                    if state.accepts_relay(endpoint) {
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
// Keep the exhaustive bridge-variant routing in one flat dispatch.
#[allow(clippy::too_many_lines)]
fn spawn_session_relay(
    context: &EndpointRelayContext,
    session_bridge: Option<mpsc::Receiver<SessionBridgeEvent>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut session = session_bridge?;
    let state = Weak::clone(&context.state);
    let channels = Arc::clone(&context.channels);
    let runner = Arc::clone(&context.endpoint.runner);
    let replacement_ready_drop = Weak::clone(&context.replacement_ready_drop);
    let endpoint = context.endpoint.id;
    Some(tokio::spawn(async move {
        while let Some(event) = session.recv().await {
            let fallback = event.clone();
            let Some(state) = state.upgrade() else {
                break;
            };
            let mut dropped_replacement: Option<(String, Option<EndpointId>)> = None;
            let send_failed = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.accepts_relay(endpoint) {
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
                        SessionBridgeEvent::NewSession { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::NewSession { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::Fork { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::Fork { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::NavigateTree { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id
                                    .map(|id| SessionBridgeEvent::NavigateTree { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::SwitchSession { id, request } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id
                                    .map(|id| SessionBridgeEvent::SwitchSession { id, request }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::Reload { id } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::Reload { id }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::SetupEntries { id, request, .. } => {
                            let routed_id = state.allocate_route(endpoint, id);
                            (
                                routed_id.map(|id| SessionBridgeEvent::SetupEntries {
                                    id,
                                    request,
                                    origin: Some(endpoint),
                                }),
                                routed_id,
                            )
                        }
                        SessionBridgeEvent::Command { envelope, .. } => (
                            Some(SessionBridgeEvent::Command {
                                envelope,
                                origin: Some(endpoint),
                            }),
                            None,
                        ),
                        SessionBridgeEvent::ReplacementReady { token, .. } => (
                            Some(SessionBridgeEvent::ReplacementReady {
                                token,
                                origin: Some(endpoint),
                            }),
                            None,
                        ),
                        SessionBridgeEvent::ReplacementAbort { token, .. } => (
                            Some(SessionBridgeEvent::ReplacementAbort {
                                token,
                                origin: Some(endpoint),
                            }),
                            None,
                        ),
                    };

                    routed.map(|routed| {
                        let failed = channels.session_bridge_tx.try_send(routed).is_err();
                        if failed && let Some(route_id) = route_id {
                            state.release_route(route_id);
                        }
                        failed
                    })
                } else {
                    None
                }
            };
            if send_failed.unwrap_or(true) {
                match &fallback {
                    SessionBridgeEvent::Command { envelope, origin } => {
                        if let Some(token) = &envelope.replacement_token {
                            dropped_replacement = Some((token.clone(), *origin));
                        }
                    }
                    SessionBridgeEvent::ReplacementReady { token, origin }
                    | SessionBridgeEvent::ReplacementAbort { token, origin } => {
                        dropped_replacement = Some((token.clone(), *origin));
                    }
                    _ => {}
                }
                answer_unclaimed_session(&runner, fallback).await;
            }
            if let Some((token, owner)) = dropped_replacement {
                abort_dropped_pending_ready(&replacement_ready_drop, &token, owner);
            }
        }
    }))
}
/// Spawn the live `providers.update` relay for one endpoint.
///
/// Each update replaces only this endpoint's provider snapshot, then the
/// aggregate is rebuilt in live endpoint order and `ModelRuntime` is
/// rewired. Stale or retired endpoints are ignored.
fn spawn_providers_update_relay(
    context: &EndpointRelayContext,
    rx: Option<watch::Receiver<pi_ext::protocol::ProvidersUpdate>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut rx = rx?;
    let state = context.state.clone();
    let channels = Arc::clone(&context.channels);
    let endpoint_id = context.endpoint.id;
    Some(tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let update = rx.borrow_and_update().clone();
            let publish = if let Some(state) = state.upgrade() {
                let mut set = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if set.stale || set.shutdown_done || set.retired.contains(&endpoint_id) {
                    false
                } else {
                    match set.generation.endpoint(endpoint_id).cloned() {
                        Some(endpoint) => {
                            set.apply_providers_update(&endpoint, &update);
                            true
                        }
                        None => false,
                    }
                }
                // `set` is dropped here, releasing PublishedRuntimeState
            } else {
                false
            };
            if publish {
                channels.publish_registry_change();
            }
        }
    }))
}
fn discard_pending_ready_op(op: PendingReadyOp) {
    match op {
        PendingReadyOp::Replacement { result, .. } => {
            spawn_runtime_safe("prepared-replacement-discard", async move {
                result.session.dispose().await;
            });
        }
        PendingReadyOp::Reload { .. } => {}
    }
}

/// Drop-specific token removal: removes ONLY a matching `Pending` state,
/// never `Finalizing`. A duplicate dropped readiness frame must not revoke an
/// already accepted `complete_ready` that won the race. Returns the removed
/// operation so the caller can discard it after releasing the mutex guard.
fn abort_pending_ready_drop(
    pending: &StdMutex<PendingReadyState>,
    token: &str,
    owner: Option<EndpointId>,
) -> Option<PendingReadyOp> {
    let mut state = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*state {
        PendingReadyState::Pending {
            token: pending_token,
            owner: pending_owner,
            ..
        } if pending_token == token && *pending_owner == owner => {}
        _ => return None,
    }
    match std::mem::replace(&mut *state, PendingReadyState::None) {
        PendingReadyState::Pending { op, ready_tx, .. } => {
            drop(ready_tx);
            Some(op)
        }
        _ => None,
    }
}

fn abort_dropped_pending_ready(
    pending: &Weak<StdMutex<PendingReadyState>>,
    token: &str,
    owner: Option<EndpointId>,
) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    if let Some(op) = abort_pending_ready_drop(&pending, token, owner) {
        discard_pending_ready_op(op);
    }
}

fn abort_pending_ready_owner(
    pending: &StdMutex<PendingReadyState>,
    owner: EndpointId,
) -> Option<PendingReadyOp> {
    let mut state = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*state {
        PendingReadyState::Pending {
            owner: Some(pending_owner),
            ..
        } if *pending_owner == owner => {}
        _ => return None,
    }
    match std::mem::replace(&mut *state, PendingReadyState::None) {
        PendingReadyState::Pending { op, ready_tx, .. } => {
            drop(ready_tx);
            Some(op)
        }
        _ => None,
    }
}

fn abort_owned_pending_ready(pending: &Weak<StdMutex<PendingReadyState>>, owner: EndpointId) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    if let Some(op) = abort_pending_ready_owner(&pending, owner) {
        discard_pending_ready_op(op);
    }
}

async fn drain_leases(generation: &Generation) {
    while generation.leases.load(Ordering::Acquire) != 0 {
        generation.drained.notified().await;
    }
}

async fn stop_generation(generation: &Generation) {
    let mut stops = generation
        .endpoints
        .iter()
        .map(|endpoint| endpoint.runner.shutdown_once())
        .collect::<FuturesUnordered<_>>();
    while stops.next().await.is_some() {}
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

fn register_endpoint_provider(
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

fn register_endpoint_providers<'a>(
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

fn endpoint_diagnostic_path(endpoint: &Endpoint) -> String {
    endpoint.label.clone()
}

/// Render every reload diagnostic in collection order so a failed prepare
/// surfaces the full path-scoped failure history, not just the terminal error.
fn summarize_diagnostics(diagnostics: &[ExtensionSetDiagnostic]) -> String {
    diagnostics
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

async fn apply_flags_to_generation(
    generation: &Generation,
    flags: &BTreeMap<String, FlagValueWire>,
) -> Result<Vec<ExtensionSetDiagnostic>, HostClientError> {
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let mut diagnostics = Vec::new();
    for endpoint in generation.endpoints.iter() {
        if let Err(error) = endpoint.runner.apply_flag_values(flags).await {
            diagnostics.push(ExtensionSetDiagnostic {
                path: endpoint_diagnostic_path(endpoint),
                message: error.to_string(),
            });
        }
    }
    Ok(diagnostics)
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
        SessionBridgeEvent::NewSession { id, .. } => {
            let _ = runner
                .respond_session_error(
                    id,
                    protocol::SESSION_NEW_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        SessionBridgeEvent::Fork { id, .. } => {
            let _ = runner
                .respond_session_error(id, protocol::SESSION_FORK_METHOD, "no active session")
                .await;
        }
        SessionBridgeEvent::NavigateTree { id, .. } => {
            let _ = runner
                .respond_navigate_tree(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::SwitchSession { id, .. } => {
            let _ = runner
                .respond_session_error(
                    id,
                    protocol::SESSION_SWITCH_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        SessionBridgeEvent::Reload { id } => {
            let _ = runner
                .respond_reload(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::SetupEntries { id, .. } => {
            let _ = runner
                .respond_setup_entries(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::Command { .. }
        | SessionBridgeEvent::ReplacementReady { .. }
        | SessionBridgeEvent::ReplacementAbort { .. } => {}
    }
}

/// Keep retirement and pending ownership cleanup atomic to observers.
fn retire_endpoint_and_abort_pending(
    state: &mut PublishedRuntimeState,
    endpoint: EndpointId,
    channels: &FacadeChannels,
    pending_ready: &Weak<StdMutex<PendingReadyState>>,
) -> bool {
    let retired = state.retire_endpoint(endpoint, channels);
    if retired {
        abort_owned_pending_ready(pending_ready, endpoint);
    }
    retired
}

fn spawn_fatal_error_relay(
    state: Weak<StdMutex<PublishedRuntimeState>>,
    endpoint: EndpointId,
    runner: Arc<HostExtensionRunner>,
    channels: Arc<FacadeChannels>,
    label: String,
    mut receiver: broadcast::Receiver<ExtensionErrorEvent>,
    pending_ready: Weak<StdMutex<PendingReadyState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut forward_first_fatal = false;
        if !runner.is_active()
            && let Some(state) = state.upgrade()
        {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            forward_first_fatal =
                retire_endpoint_and_abort_pending(&mut state, endpoint, &channels, &pending_ready);
        }
        loop {
            match receiver.recv().await {
                Ok(item) => {
                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    let forward = {
                        let mut state = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let retired_now = !runner.is_active()
                            && retire_endpoint_and_abort_pending(
                                &mut state,
                                endpoint,
                                &channels,
                                &pending_ready,
                            );
                        forward_first_fatal || retired_now || state.accepts_relay(endpoint)
                    };
                    if forward {
                        let _ = channels.errors_tx.send(item);
                        forward_first_fatal = false;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    let Some(state) = state.upgrade() else {
                        break;
                    };
                    let (retired_now, accepts_relay) = {
                        let mut state = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if runner.is_active() {
                            (false, state.accepts_relay(endpoint))
                        } else {
                            (
                                retire_endpoint_and_abort_pending(
                                    &mut state,
                                    endpoint,
                                    &channels,
                                    &pending_ready,
                                ),
                                false,
                            )
                        }
                    };
                    if !retired_now && accepts_relay {
                        channels.publish_error(
                            "extension_event_lagged",
                            format!("extension {label:?} relay lagged by {count} events"),
                            None,
                        );
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    if !runner.is_active()
                        && let Some(state) = state.upgrade()
                    {
                        let mut state = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        retire_endpoint_and_abort_pending(
                            &mut state,
                            endpoint,
                            &channels,
                            &pending_ready,
                        );
                    }
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::error::Error;
    use std::io::{BufRead, Write};
    use std::sync::atomic::AtomicUsize;

    use pi_ext::protocol::{Frame, FrameKind, HelloAck, SessionCommand, SessionCommandEnvelope};
    use pi_ext::sanitize::{SanitizedRun, SanitizedSlot};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    #[tokio::test]
    async fn pending_ready_slot_correlates_one_operation_through_finalizing() -> TestResult {
        let (generation, _) = generation_from_endpoints(1, Vec::new());
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload {
                        generation: None,
                        pending: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                    model_runtime: Arc::clone(&runtime),
                },
            )
            .map_err(|_| "initial pending install was rejected")?;

        assert!(set.is_pending_busy());
        assert!(
            set.install_pending(
                set.next_replacement_token(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload {
                        generation: None,
                        pending: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                    model_runtime: runtime,
                },
            )
            .is_err()
        );
        assert!(!set.complete_ready("stale-token"));
        assert!(set.complete_ready(&token));
        ready.await?;
        let (finalizing, _guard) = set
            .take_finalizing(&token)
            .ok_or("ready operation was not finalizing")?;
        assert!(matches!(finalizing, PendingReadyOp::Reload { .. }));
        assert!(set.is_pending_busy());
        // Explicit finish clears the slot; the guard is dropped after.
        assert!(set.finish_finalize(&token));
        assert!(!set.is_pending_busy());
        // A second finish is idempotent (slot already cleared).
        assert!(set.finish_finalize(&token));
        assert!(!set.is_pending_busy());
        Ok(())
    }

    #[tokio::test]
    async fn finalize_guard_releases_slot_when_dropped_without_finish() -> TestResult {
        let (generation, _) = generation_from_endpoints(1, Vec::new());
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload {
                        generation: None,
                        pending: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                    model_runtime: Arc::clone(&runtime),
                },
            )
            .map_err(|_| "initial pending install was rejected")?;

        assert!(set.complete_ready(&token));
        ready.await?;
        {
            let (finalizing, _guard) = set
                .take_finalizing(&token)
                .ok_or("ready operation was not finalizing")?;
            assert!(matches!(finalizing, PendingReadyOp::Reload { .. }));
            assert!(set.is_pending_busy());
            // Drop the guard without calling finish — the slot must be
            // released so future replacements are not wedged.
        }
        assert!(
            !set.is_pending_busy(),
            "dropped finalize guard must release the finalizing slot"
        );
        Ok(())
    }

    #[tokio::test]
    async fn second_hop_full_aborts_only_matching_pending_token() -> TestResult {
        let (runner, host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);

        // Install a PendingReadyOp::Reload and keep ready_rx.
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload {
                        generation: None,
                        pending: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                    model_runtime: Arc::clone(&runtime),
                },
            )
            .map_err(|_| "initial pending install was rejected")?;
        let owner = set.state().generation.endpoints[0].id;
        let route = set
            .state()
            .allocate_route(owner, 80)
            .ok_or("owner route allocation failed")?;
        set.respond_reload(route, Ok(Some(&token))).await?;
        host.wait_for_response(protocol::SESSION_RELOAD_METHOD, 80)
            .await?;

        // Send a stale dropped token — the pending slot must remain.
        let stale_token = "stale-tok-999".to_owned();
        abort_pending_ready_drop(&set.pending_ready, &stale_token, Some(owner));
        assert!(
            set.is_pending_busy(),
            "stale dropped token must not clear the active operation"
        );
        let wrong_owner = EndpointId {
            generation: owner.generation,
            position: owner.position + 1,
        };
        assert!(
            abort_pending_ready_drop(&set.pending_ready, &token, Some(wrong_owner)).is_none(),
            "a dropped frame from another endpoint aborted the pending operation"
        );
        assert!(set.is_pending_busy());

        // Fill the facade session_bridge_tx to EVENT_CHANNEL_CAPACITY while
        // its receiver is held and not drained.
        let _bridge_rx = set
            .take_session_bridge()
            .ok_or("session bridge receiver missing")?;
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            set.channels
                .session_bridge_tx
                .try_send(SessionBridgeEvent::Command {
                    envelope: SessionCommandEnvelope {
                        replacement_token: None,
                        command: SessionCommand::SetSessionName {
                            name: "filler".to_owned(),
                        },
                    },
                    origin: None,
                })
                .map_err(|_| "failed to fill session bridge")?;
        }

        // Emit a real session.replacementReady from the fake host for the
        // installed token. The relay's try_send to the facade will fail
        // (channel full), so the drop handler must abort the matching token.
        host.emit(Frame {
            id: 0,
            kind: FrameKind::Event,
            method: "session.replacementReady".to_owned(),
            payload: json!({"token": token}),
        })
        .await;

        // Assert within a bounded test-only timeout that ready_rx returns Err,
        // pending busy is false, and no finalizing operation exists.
        let result = tokio::time::timeout(Duration::from_secs(5), ready_rx).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "ready_rx must return Err after the matching token is aborted"
        );
        assert!(
            !set.is_pending_busy(),
            "pending slot must be cleared after the matching token is aborted"
        );
        assert!(
            set.take_finalizing(&token).is_none(),
            "no finalizing operation must exist after a dropped ready"
        );

        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn second_hop_full_scoped_command_aborts_pending_token() -> TestResult {
        let (runner, host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload::empty_for_test(),
                    model_runtime: runtime,
                },
            )
            .map_err(|_| "initial pending install was rejected")?;
        let owner = set.state().generation.endpoints[0].id;
        let route = set
            .state()
            .allocate_route(owner, 81)
            .ok_or("owner route allocation failed")?;
        set.respond_reload(route, Ok(Some(&token))).await?;
        host.wait_for_response(protocol::SESSION_RELOAD_METHOD, 81)
            .await?;
        let _bridge_rx = set
            .take_session_bridge()
            .ok_or("session bridge receiver missing")?;
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            set.channels
                .session_bridge_tx
                .try_send(SessionBridgeEvent::Command {
                    envelope: SessionCommandEnvelope {
                        replacement_token: None,
                        command: SessionCommand::SetSessionName {
                            name: "filler".to_owned(),
                        },
                    },
                    origin: None,
                })
                .map_err(|_| "failed to fill session bridge")?;
        }

        host.emit(Frame {
            id: 0,
            kind: FrameKind::Event,
            method: protocol::SESSION_COMMAND_METHOD.to_owned(),
            payload: json!({
                "replacementToken": token,
                "action": "setSessionName",
                "name": "candidate"
            }),
        })
        .await;

        let result = tokio::time::timeout(TEST_TIMEOUT, ready_rx).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "dropping a scoped command must wake the pending waiter"
        );
        assert!(
            !set.is_pending_busy(),
            "dropping a scoped command must clear the matching pending slot"
        );
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn dropped_duplicate_ready_preserves_finalizing_operation() -> TestResult {
        let (generation, _) = generation_from_endpoints(1, Vec::new());
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload::empty_for_test(),
                    model_runtime: runtime,
                },
            )
            .map_err(|_| "initial pending install was rejected")?;

        assert!(set.complete_ready(&token));
        assert!(
            abort_pending_ready_drop(&set.pending_ready, &token, None).is_none(),
            "a duplicate dropped ready must not revoke a finalizing operation"
        );
        ready_rx.await?;
        let (op, _guard) = set
            .take_finalizing(&token)
            .ok_or("duplicate dropped ready removed the finalizing operation")?;
        assert!(matches!(&op, PendingReadyOp::Reload { .. }));
        drop(op);
        assert!(set.finish_finalize(&token));
        Ok(())
    }

    #[tokio::test]
    async fn commit_reload_without_generation_returns_uncommitted_diagnostic() -> TestResult {
        let (generation, _) = generation_from_endpoints(1, Vec::new());
        let set =
            ExtensionRuntimeSet::from_generation(generation, Vec::new(), String::new(), false);
        let runtime = ModelRuntime::create_in_memory().await?;
        let prepared = PreparedReload {
            generation: None,
            pending: Vec::new(),
            diagnostics: Vec::new(),
        };
        let result = set.commit_reload(&runtime, prepared).await;
        assert!(
            !result.committed,
            "a reload without a generation must not commit"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("no replacement generation")),
            "diagnostics must explain the missing generation: {:?}",
            result.diagnostics
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_drains_pending_ready_and_wakes_waiter() -> TestResult {
        let (generation, _) = generation_from_endpoints(1, Vec::new());
        let set =
            ExtensionRuntimeSet::from_generation(generation, Vec::new(), String::new(), false);
        let token = set.next_replacement_token();
        let ready = set
            .install_pending(
                token,
                PendingReadyOp::Reload {
                    prepared: PreparedReload {
                        generation: None,
                        pending: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                    model_runtime: Arc::new(ModelRuntime::create_in_memory().await?),
                },
            )
            .map_err(|_| "pending install was rejected")?;

        set.shutdown_once().await;

        assert!(ready.await.is_err());
        assert!(!set.is_pending_busy());
        Ok(())
    }

    enum FakeCommand {
        Emit(Frame),
        PauseReads {
            paused: tokio::sync::oneshot::Sender<()>,
            release: tokio::sync::oneshot::Receiver<()>,
        },
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
        pub(crate) fn set_response(&self, method: &str, payload: Value) {
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

        pub(crate) async fn emit(&self, frame: Frame) {
            let _ = self.commands.send(FakeCommand::Emit(frame)).await;
        }

        pub(crate) async fn wait_for_frame(&self, method: &str) -> TestResult {
            tokio::time::timeout(TEST_TIMEOUT, async {
                loop {
                    if self
                        .state
                        .frames
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .any(|frame| frame.method == method)
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

        async fn pause_reads(&self) -> TestResult<tokio::sync::oneshot::Sender<()>> {
            let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            self.commands
                .send(FakeCommand::PauseReads {
                    paused: paused_tx,
                    release: release_rx,
                })
                .await
                .map_err(|_| "fake host command channel closed")?;
            tokio::time::timeout(TEST_TIMEOUT, paused_rx)
                .await
                .map_err(|_| "fake host did not pause reads")??;
            Ok(release_tx)
        }

        pub(crate) async fn close(&self) {
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
        pub(crate) fn frame_count(&self, method: &str) -> usize {
            self.state
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|frame| frame.method == method)
                .count()
        }

        pub(crate) fn first_payload(&self, method: &str) -> Option<Value> {
            self.state
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|frame| frame.method == method)
                .map(|frame| frame.payload.clone())
        }

        pub(crate) fn observed_methods(&self) -> Vec<String> {
            self.state
                .frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|frame| frame.method.clone())
                .collect()
        }

        pub(crate) async fn wait_for_exit(&self) -> TestResult {
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

    async fn make_blocking_shutdown_runner() -> TestResult<Arc<HostExtensionRunner>> {
        let test_name = format!(
            "{}::blocking_shutdown_child",
            module_path!()
                .split_once("::")
                .map_or(module_path!(), |(_, path)| path)
        );
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(test_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env("PI_BLOCKING_SHUTDOWN_CHILD", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or("blocking child stdin missing")?;
        let stdout = child.stdout.take().ok_or("blocking child stdout missing")?;
        let stderr = child.stderr.take().ok_or("blocking child stderr missing")?;
        // The child writes JSONL to stderr because libtest owns its stdout.
        let client = Arc::new(HostClient::connect_boxed(
            Box::new(stdin),
            Box::new(stderr),
            Box::new(stdout),
            Some(child),
        ));
        Ok(HostExtensionRunner::connect_with_cwd_and_trust(
            client,
            Vec::new(),
            "/workspace",
            false,
            Duration::from_millis(200),
        )
        .await?)
    }

    #[test]
    #[ignore = "spawned as a child process by the concurrent shutdown test"]
    fn blocking_shutdown_child() -> TestResult {
        if std::env::var_os("PI_BLOCKING_SHUTDOWN_CHILD").is_none() {
            return Ok(());
        }
        let mut input = std::io::BufReader::new(std::io::stdin().lock());
        let mut output = std::io::stderr().lock();
        let mut line = String::new();

        input.read_line(&mut line)?;
        let hello = pi_ext::protocol::decode_frame_str(&line)?;
        output.write_all(&pi_ext::protocol::encode_frame(&Frame {
            id: hello.id,
            kind: FrameKind::Res,
            method: hello.method,
            payload: serde_json::to_value(HelloAck::local())?,
        })?)?;
        output.flush()?;

        line.clear();
        input.read_line(&mut line)?;
        let load = pi_ext::protocol::decode_frame_str(&line)?;
        output.write_all(&pi_ext::protocol::encode_frame(&Frame {
            id: load.id,
            kind: FrameKind::Res,
            method: load.method,
            payload: snapshot(&[]),
        })?)?;
        output.flush()?;
        std::thread::sleep(Duration::from_mins(1));
        Ok(())
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
                    Some(FakeCommand::PauseReads { paused, release }) => {
                        let _ = paused.send(());
                        let _ = release.await;
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
    fn provider_update_frame(name: &str, base_url: &str) -> Frame {
        Frame {
            id: 0,
            kind: FrameKind::Event,
            method: pi_ext::protocol::PROVIDERS_UPDATE_METHOD.to_owned(),
            payload: json!({
                "providers": [{
                    "name": name,
                    "baseUrl": base_url,
                    "api": "openai-completions"
                }]
            }),
        }
    }

    async fn wait_for_provider_snapshot(
        runtime: &ModelRuntime,
        name: &str,
        base_url: &str,
        absent: &[&str],
    ) -> TestResult {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let current = runtime.get_registered_provider_config(name);
                if current
                    .as_ref()
                    .and_then(|config| config.base_url.as_deref())
                    == Some(base_url)
                    && absent
                        .iter()
                        .all(|name| runtime.get_registered_provider_config(name).is_none())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| format!("provider runtime did not publish {name}"))?;
        Ok(())
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

    fn probe_preserved_flags() -> HashMap<String, Value> {
        HashMap::from([("probe".to_owned(), Value::Bool(true))])
    }

    fn test_endpoint_plan(
        position: usize,
        kind: EndpointKind,
        label: &str,
        builtins: bool,
    ) -> EndpointPlan {
        EndpointPlan {
            position,
            kind,
            entries: vec![format!("{label}.entry")],
            diagnostic_paths: vec![label.to_owned()],
            builtins,
            label: label.to_owned(),
        }
    }

    fn test_typescript_host_spec() -> HostSpec {
        HostSpec {
            source: HostSource::Env(PathBuf::from("/test/bun")),
            program: PathBuf::from("/test/bun"),
            args: Vec::new(),
        }
    }

    async fn shutdown_generation(generation: &Generation) {
        let mut stops = generation
            .endpoints
            .iter()
            .map(|endpoint| endpoint.runner.shutdown_once())
            .collect::<FuturesUnordered<_>>();
        while stops.next().await.is_some() {}
    }

    #[tokio::test]
    async fn best_effort_replaces_failed_builtins_owner_with_builtins_only_fallback() -> TestResult
    {
        let starts = Arc::new(StdMutex::new(Vec::new()));
        let starter = {
            let starts = Arc::clone(&starts);
            move |plan: EndpointPlan, _: HostSpec, _: String, _: bool| {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((
                            plan.position,
                            plan.label.clone(),
                            plan.entries.clone(),
                            plan.builtins,
                        ));
                    if plan.label == "owner" {
                        return Err("owner failed".to_owned());
                    }
                    let registry = if plan.builtins && plan.label == "secondary" {
                        let mut registry = snapshot(&[]);
                        registry["errors"] = json!([{
                            "path": "<builtin:test>",
                            "error": "builtin load failed"
                        }]);
                        registry
                    } else {
                        snapshot(&[])
                    };
                    make_runner(registry)
                        .await
                        .map(|(runner, _)| runner)
                        .map_err(|error| error.to_string())
                }
            }
        };
        let build = build_generation_with_starter(
            1,
            vec![
                test_endpoint_plan(0, EndpointKind::Native, "native", false),
                test_endpoint_plan(1, EndpointKind::TsCompat, "owner", true),
                test_endpoint_plan(2, EndpointKind::TsCompat, "secondary", false),
            ],
            "/workspace",
            false,
            GenerationBuildPolicy::BestEffortStart,
            Some(Ok(test_typescript_host_spec())),
            starter,
        )
        .await;

        assert_eq!(
            build.diagnostics,
            [
                ExtensionSetDiagnostic {
                    path: "owner".to_owned(),
                    message: "owner failed".to_owned(),
                },
                ExtensionSetDiagnostic {
                    path: "<builtin:test>".to_owned(),
                    message: "builtin load failed".to_owned(),
                },
            ]
        );
        let generation = build.generation.ok_or("fallback generation missing")?;
        assert_eq!(
            generation
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.as_str())
                .collect::<Vec<_>>(),
            ["native", "secondary"]
        );
        let starts = starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(starts.len(), 4, "a healthy sibling was restarted");
        assert_eq!(
            starts
                .iter()
                .filter(|(_, _, entries, _)| entries == &["owner.entry"])
                .count(),
            1,
            "the failed owner was restarted"
        );
        assert!(starts.iter().any(|(_, label, entries, builtins)| {
            label == "secondary" && entries == &["secondary.entry"] && !*builtins
        }));
        assert!(starts.iter().any(|(_, label, entries, builtins)| {
            label == "secondary" && entries == &["secondary.entry"] && *builtins
        }));
        drop(starts);
        shutdown_generation(&generation).await;
        Ok(())
    }

    #[tokio::test]
    async fn best_effort_keeps_survivors_when_builtins_fallback_fails() -> TestResult {
        let starts = Arc::new(StdMutex::new(Vec::new()));
        let starter = {
            let starts = Arc::clone(&starts);
            move |plan: EndpointPlan, _: HostSpec, _: String, _: bool| {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((plan.label.clone(), plan.entries.clone(), plan.builtins));
                    if plan.label == "owner" {
                        Err("owner failed".to_owned())
                    } else if plan.label == "secondary" && plan.builtins {
                        Err("promotion failed".to_owned())
                    } else {
                        make_runner(snapshot(&[]))
                            .await
                            .map(|(runner, _)| runner)
                            .map_err(|error| error.to_string())
                    }
                }
            }
        };
        let build = build_generation_with_starter(
            1,
            vec![
                test_endpoint_plan(0, EndpointKind::Native, "native", false),
                test_endpoint_plan(1, EndpointKind::TsCompat, "owner", true),
                test_endpoint_plan(2, EndpointKind::TsCompat, "secondary", false),
            ],
            "/workspace",
            false,
            GenerationBuildPolicy::BestEffortStart,
            Some(Ok(test_typescript_host_spec())),
            starter,
        )
        .await;

        assert_eq!(
            build.diagnostics,
            [
                ExtensionSetDiagnostic {
                    path: "owner".to_owned(),
                    message: "owner failed".to_owned(),
                },
                ExtensionSetDiagnostic {
                    path: "secondary".to_owned(),
                    message: "builtins promotion failed for secondary: promotion failed".to_owned(),
                },
            ]
        );
        let generation = build.generation.ok_or("survivor generation missing")?;
        assert_eq!(
            generation
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.as_str())
                .collect::<Vec<_>>(),
            ["native", "secondary"]
        );
        let starts = starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(starts.len(), 4, "a healthy sibling was restarted");
        assert_eq!(
            starts
                .iter()
                .filter(|(_, entries, _)| entries == &["owner.entry"])
                .count(),
            1,
            "the failed owner was restarted"
        );
        assert!(starts.iter().any(|(label, entries, builtins)| {
            label == "secondary" && entries == &["secondary.entry"] && !*builtins
        }));
        assert!(starts.iter().any(|(label, entries, builtins)| {
            label == "secondary" && entries == &["secondary.entry"] && *builtins
        }));
        drop(starts);
        shutdown_generation(&generation).await;
        Ok(())
    }

    #[tokio::test]
    async fn builtins_owner_failure_promotes_first_surviving_compat() -> TestResult {
        let starts = Arc::new(StdMutex::new(Vec::new()));
        let starter = {
            let starts = Arc::clone(&starts);
            move |plan: EndpointPlan, _: HostSpec, _: String, _: bool| {
                let starts = Arc::clone(&starts);
                async move {
                    starts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((plan.label.clone(), plan.entries.clone(), plan.builtins));
                    if plan.label == "owner" {
                        return Err("owner failed".to_owned());
                    }
                    make_runner(snapshot(&[]))
                        .await
                        .map(|(runner, _)| runner)
                        .map_err(|error| error.to_string())
                }
            }
        };
        let build = build_generation_with_starter(
            1,
            vec![
                test_endpoint_plan(0, EndpointKind::Native, "native", false),
                test_endpoint_plan(1, EndpointKind::TsCompat, "owner", true),
                test_endpoint_plan(2, EndpointKind::TsCompat, "secondary", false),
            ],
            "/workspace",
            false,
            GenerationBuildPolicy::BestEffortStart,
            Some(Ok(test_typescript_host_spec())),
            starter,
        )
        .await;
        let generation = build.generation.ok_or("promoted generation missing")?;
        assert_eq!(
            generation
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.as_str())
                .collect::<Vec<_>>(),
            ["native", "secondary"]
        );
        let starts = starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(starts.iter().any(|(label, entries, builtins)| {
            label == "secondary" && entries == &["secondary.entry"] && *builtins
        }));
        assert!(build.diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "owner" && diagnostic.message == "owner failed"
        }));
        drop(starts);
        shutdown_generation(&generation).await;
        Ok(())
    }

    #[tokio::test]
    async fn builtins_promotion_failure_keeps_surviving_endpoint_and_reports_path() -> TestResult {
        let starter = move |plan: EndpointPlan, _: HostSpec, _: String, _: bool| async move {
            if plan.label == "owner" {
                Err("owner failed".to_owned())
            } else if plan.label == "secondary" && plan.builtins {
                Err("promotion failed".to_owned())
            } else {
                make_runner(snapshot(&[]))
                    .await
                    .map(|(runner, _)| runner)
                    .map_err(|error| error.to_string())
            }
        };
        let build = build_generation_with_starter(
            1,
            vec![
                test_endpoint_plan(0, EndpointKind::Native, "native", false),
                test_endpoint_plan(1, EndpointKind::TsCompat, "owner", true),
                test_endpoint_plan(2, EndpointKind::TsCompat, "secondary", false),
            ],
            "/workspace",
            false,
            GenerationBuildPolicy::BestEffortStart,
            Some(Ok(test_typescript_host_spec())),
            starter,
        )
        .await;
        let generation = build.generation.ok_or("survivor generation missing")?;
        assert_eq!(
            generation
                .endpoints
                .iter()
                .map(|endpoint| endpoint.label.as_str())
                .collect::<Vec<_>>(),
            ["native", "secondary"]
        );
        assert!(build.diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "secondary" && diagnostic.message.contains("promotion")
        }));
        shutdown_generation(&generation).await;
        Ok(())
    }

    #[tokio::test]
    async fn multi_endpoint_flag_failures_are_path_qualified_and_do_not_abort_siblings()
    -> TestResult {
        let (first, first_host) = make_runner(snapshot(&[])).await?;
        let (second, second_host) = make_runner(snapshot(&[])).await?;
        first_host.set_response("flags.set", json!({"ok": false}));
        second_host.set_response("flags.set", json!({"ok": true}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::TsCompat, second),
        ]);
        let values = BTreeMap::from([("demo".to_owned(), FlagValueWire::Boolean(true))]);
        let diagnostics = set.apply_flag_values(&values).await?;
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.contains("test:")),
            "expected path-qualified flag diagnostics: {diagnostics:?}"
        );
        assert!(
            first_host.request_count("flags.set") + second_host.request_count("flags.set") >= 2,
            "sibling endpoints must both be attempted"
        );
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn prepare_reload_failure_keeps_old_runner_provider_and_transport_live() -> TestResult {
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
        let (old, old_host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
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
        set.invalidate();
        assert!(
            set.prepare_reload(HashMap::new()).await.is_err(),
            "invalidated facade must fail prepare early"
        );
        assert_eq!(set.reload_generation(), 1);
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        assert_eq!(old_host.exit_count(), 0);
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn post_start_fatal_prepare_failure_reaps_replacement_and_keeps_old_live() -> TestResult {
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
        let (old, old_host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
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
        set.inject_prepared_replacement_then_fatal_preparation_failure(replacement, pending);
        replacement_host.set_response("flags.set", json!({"ok": true}));
        assert!(
            set.prepare_reload(probe_preserved_flags()).await.is_err(),
            "post-start fatal prepare must fail after replacement start"
        );
        assert_eq!(replacement_host.request_count("flags.set"), 1);
        replacement_host.wait_for_exit().await?;
        assert_eq!(replacement_host.exit_count(), 1);
        assert_eq!(set.reload_generation(), 1);
        assert!(set.is_active());
        assert!(set.can_reload());
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        assert_eq!(old_host.exit_count(), 0);
        let result = set.emit_input("still-live", None, "user", None).await?;
        assert!(!result.handled);
        old_host.wait_for_request("input").await?;
        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn commit_replaces_prepared_generation_after_old_endpoint_inactivates() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&[])).await?;
        let old_to_invalidate = Arc::clone(&old);
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_for_reload(replacement, pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        let prepared = set.prepare_reload(HashMap::new()).await?;

        old_to_invalidate.invalidate();
        let result = set.commit_reload(&runtime, prepared).await;

        assert!(result.diagnostics.is_empty());
        assert_eq!(set.reload_generation(), 2);
        assert!(set.is_active());
        old_host.wait_for_exit().await?;
        set.shutdown_once().await;
        replacement_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn commit_rejects_stale_facade_without_provider_mutation() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_for_reload(replacement, pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        let prepared = set.prepare_reload(HashMap::new()).await?;
        let provider_epoch = runtime.provider_mutation_epoch();

        set.invalidate();
        let result = set.commit_reload(&runtime, prepared).await;

        assert!(!result.committed);
        assert_eq!(set.reload_generation(), 1);
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        replacement_host.wait_for_exit().await?;
        assert_eq!(replacement_host.exit_count(), 1);
        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn reload_returns_load_flag_and_provider_diagnostics() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&["input"])).await?;
        let (replacement, replacement_host) = make_runner(json!({
            "providers": [{
                "name": "invalid-provider",
                "models": [{"id": "invalid-model", "reasoning": false}]
            }],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        replacement_host.set_response("flags.set", json!({"ok": false}));
        let (generation, pending) =
            generation_from_endpoints(1, vec![(EndpointKind::TsCompat, "<old>".to_owned(), old)]);
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.install(pending);
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_with_diagnostics_for_reload(
            replacement,
            pending,
            vec![ExtensionSetDiagnostic {
                path: "broken-sibling.ts".to_owned(),
                message: "load failed".to_owned(),
            }],
        );
        let runtime = ModelRuntime::create_in_memory().await?;
        // Provider validation now runs in prepare_reload before commit, so
        // restart_and_rewire returns an error when the replacement has an
        // invalid provider. The old generation stays live.
        let result = set
            .restart_and_rewire(
                &runtime,
                HashMap::from([("demo".to_owned(), Value::Bool(true))]),
            )
            .await;
        assert!(
            result.is_err(),
            "prepare_reload must reject invalid providers"
        );
        assert!(set.is_active());
        assert_eq!(set.reload_generation(), 1);
        replacement_host.wait_for_exit().await?;
        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn prepare_reload_preserves_all_diagnostics_on_provider_validation_failure() -> TestResult
    {
        // Old generation carries a valid provider that must stay published.
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
        let (old, old_host) = make_runner(json!({
            "providers": [old_provider],
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        // Replacement carries an invalid provider (no api/baseUrl) so
        // validate_generation_providers fails during prepare_reload.
        let (replacement, replacement_host) = make_runner(json!({
            "providers": [{
                "name": "invalid-provider",
                "models": [{"id": "invalid-model", "reasoning": false}]
            }],
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

        // Inject the invalid replacement together with a load diagnostic so the
        // candidate generation accumulates diagnostics before validation fails.
        let (replacement_generation, replacement_pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_with_diagnostics_for_reload(
            replacement_generation,
            replacement_pending,
            vec![ExtensionSetDiagnostic {
                path: "broken-sibling.ts".to_owned(),
                message: "load failed".to_owned(),
            }],
        );

        // No preserved flags, so flag sync succeeds and prepare_reload reaches
        // provider validation. Every collected diagnostic must survive the error.
        let Err(error) = set.prepare_reload(HashMap::new()).await else {
            return Err("prepare_reload unexpectedly succeeded with an invalid provider".into());
        };
        let HostStartError::Load(message) = error else {
            return Err(format!("expected HostStartError::Load, got {error}").into());
        };
        assert!(
            message.contains("broken-sibling.ts"),
            "collected load diagnostic path was lost: {message}"
        );
        assert!(
            message.contains("load failed"),
            "collected load diagnostic message was lost: {message}"
        );
        assert!(
            message.contains("invalid-provider"),
            "provider validation path was lost: {message}"
        );
        assert!(
            message.contains("no \"api\" specified"),
            "provider validation error was lost: {message}"
        );

        // The live generation is unchanged: no new generation, old provider
        // still published, replacement never registered.
        assert!(set.is_active());
        assert_eq!(set.reload_generation(), 1);
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert!(
            runtime
                .get_registered_provider_config("old-provider")
                .is_some(),
            "old provider must remain published"
        );
        assert!(
            runtime
                .get_registered_provider_config("invalid-provider")
                .is_none(),
            "invalid replacement provider must not publish"
        );
        replacement_host.wait_for_exit().await?;
        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
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
    fn native_first_attaches_builtins_to_first_compat_group() {
        let plans = plan_endpoints(&[
            classified(ExtensionRuntime::Native, "native"),
            classified(ExtensionRuntime::TsCompat, "plugin.ts"),
        ]);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].entries, ["native"]);
        assert!(!plans[0].builtins);
        assert_eq!(plans[1].entries, ["plugin.ts"]);
        assert!(plans[1].builtins);
    }

    #[test]
    fn native_only_plan_does_not_start_bun_for_builtins() {
        let plans = plan_endpoints(&[classified(ExtensionRuntime::Native, "native")]);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, EndpointKind::Native);
        assert!(!plans[0].builtins);
        assert!(
            resolve_typescript_host(&plans).is_none(),
            "native-only plans must not resolve the Bun compatibility host"
        );
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
        let GenerationBuild {
            generation,
            pending,
            diagnostics,
            endpoint_start_failure: _,
        } = build_generation(
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
            GenerationBuildPolicy::BestEffortStart,
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

    /// Retiring an endpoint that is still active (transport never closed) must
    /// centrally quarantine it: the runner is invalidated so every `is_active`
    /// consumer — registry, shortcuts, and dispatch — excludes it without
    /// waiting for a fatal relay.
    #[tokio::test]
    async fn retiring_an_active_endpoint_excludes_it_from_registry_shortcuts_and_dispatch()
    -> TestResult {
        let first_snapshot = json!({
            "tools": [{"name": "sharedTool", "label": "doomed", "description": "", "parameters": {}}],
            "commands": [{"name": "sharedCommand"}],
            "renderers": [{"type": "tool", "name": "sharedTool"}],
            "shortcuts": [{"key": "ctrl+a"}],
        });
        let second_snapshot = json!({
            "tools": [{"name": "sharedTool", "label": "survivor", "description": "", "parameters": {}}],
            "commands": [{"name": "sharedCommand"}],
            "renderers": [{"type": "tool", "name": "sharedTool"}],
            "shortcuts": [{"key": "ctrl+b"}],
        });
        let (first, first_host) = make_runner(first_snapshot).await?;
        let (second, second_host) = make_runner(second_snapshot).await?;
        let first_runner = Arc::clone(&first);
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::TsCompat, second),
        ]);

        // First-wins: the still-active doomed endpoint owns the published surfaces.
        assert_eq!(set.registry().tools()[0].label, "doomed");
        assert_eq!(set.raw_shortcuts().len(), 2);
        assert!(first_runner.is_active());

        // Manually retire the still-active first endpoint; its transport stays open.
        {
            let mut state = set.state();
            let doomed = state.generation.endpoints[0].id;
            assert!(state.retire_endpoint(doomed, &set.channels));
        }

        // Retirement centrally invalidates the runner even without transport shutdown.
        assert!(
            !first_runner.is_active(),
            "retirement must quarantine the runner, not only the published state"
        );
        // Registry and shortcuts now reflect only the surviving endpoint.
        assert_eq!(set.registry().tools()[0].label, "survivor");
        assert_eq!(set.raw_shortcuts().len(), 1);
        // Dispatch routes to the survivor and never reaches the retired endpoint.
        second_host.set_response("command.execute", json!({"ok": true}));
        first_host.set_response("command.execute", json!({"ok": false}));
        assert!(set.execute_command("sharedCommand", "").await?);
        second_host.wait_for_request("command.execute").await?;
        assert_eq!(first_host.request_count("command.execute"), 0);
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn provider_update_relay_keeps_latest_complete_snapshot() -> TestResult {
        let (runner, host) = make_runner(json!({
            "providers": [shared_provider("https://old.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (generation, pending) = generation_from_endpoints(
            1,
            vec![(
                EndpointKind::TsCompat,
                "<provider-update>".to_owned(),
                runner,
            )],
        );

        host.emit(provider_update_frame("pre-spawn", "https://pre.example/v1"))
            .await;
        tokio::time::timeout(TEST_TIMEOUT, async {
            while !pending[0]
                .providers_update
                .as_ref()
                .is_some_and(|receiver| receiver.has_changed().unwrap_or(false))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "pre-spawn provider update did not reach the watch receiver")?;

        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        let mut registry_changes = set.subscribe_registry_changes();
        let revision = *registry_changes.borrow_and_update();
        set.install(pending);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );
        wait_for_provider_snapshot(
            &runtime,
            "pre-spawn",
            "https://pre.example/v1",
            &["shared-provider"],
        )
        .await?;
        tokio::time::timeout(TEST_TIMEOUT, registry_changes.changed())
            .await
            .map_err(|_| "provider update did not publish a registry revision")??;
        assert_ne!(*registry_changes.borrow_and_update(), revision);

        for (name, base_url) in [
            ("transient", "https://transient.example/v1"),
            ("final-provider", "https://final.example/v1"),
        ] {
            host.emit(provider_update_frame(name, base_url)).await;
        }

        wait_for_provider_snapshot(
            &runtime,
            "final-provider",
            "https://final.example/v1",
            &["shared-provider", "pre-spawn", "transient"],
        )
        .await?;

        set.shutdown_once().await;
        host.wait_for_exit().await?;
        Ok(())
    }

    /// When a crashed-but-not-retired endpoint sits ahead of the retired owner
    /// in iteration order, the provider-owner filter must still recognize the
    /// retiring owner and rewire to the surviving active duplicate instead of
    /// leaving the provider orphaned on the crashed-first endpoint.
    #[tokio::test]
    async fn retiring_an_owner_rewires_provider_to_a_surviving_active_duplicate() -> TestResult {
        let (crashed, _crashed_host) = make_runner(json!({
            "providers": [shared_provider("https://crashed.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (owner, _owner_host) = make_runner(json!({
            "providers": [shared_provider("https://owner.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (duplicate, _duplicate_host) = make_runner(json!({
            "providers": [shared_provider("https://dup.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let crashed_runner = Arc::clone(&crashed);
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, crashed),
            (EndpointKind::TsCompat, owner),
            (EndpointKind::TsCompat, duplicate),
        ]);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, result)| result.is_ok())
        );
        // First-wins registration: the crashed-first endpoint holds the provider.
        assert_eq!(
            runtime
                .get_registered_provider_config("shared-provider")
                .ok_or("shared-provider not registered")?
                .base_url
                .as_deref(),
            Some("https://crashed.example/v1")
        );

        // The crashed-first endpoint goes inactive WITHOUT being retired (no relay raced).
        crashed_runner.invalidate();
        assert!(!crashed_runner.is_active());

        let owner_id = EndpointId {
            generation: 1,
            position: 1,
        };
        let epoch = runtime.provider_mutation_epoch();
        {
            let mut state = set.state();
            assert!(state.retire_endpoint(owner_id, &set.channels));
        }

        // The surviving active duplicate is now the registered owner (rewired off
        // the retired owner), not left unregistered or orphaned on the crashed endpoint.
        let config = runtime
            .get_registered_provider_config("shared-provider")
            .ok_or("surviving active duplicate was left unregistered")?;
        assert_eq!(config.base_url.as_deref(), Some("https://dup.example/v1"));
        assert_eq!(
            runtime.provider_mutation_epoch(),
            epoch + 2,
            "retirement must unregister then rewire the provider"
        );
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
        let before = set.reload_generation();

        let result = set
            .restart_and_rewire(&runtime, HashMap::new())
            .await
            .expect("best-effort native replacement should prepare/commit");
        assert!(
            result.committed,
            "reload did not commit: {:?}",
            result.diagnostics
        );
        assert_eq!(
            set.reload_generation(),
            before + 1,
            "a committed reload must publish exactly one new generation"
        );
        assert!(set.is_active(), "facade lost every endpoint after reload");
        old_host.wait_for_exit().await?;
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

        // Provider validation now runs in prepare_reload, so restart_and_rewire
        // returns an error (not a ReloadResult) when the replacement has an
        // invalid provider. The old generation stays live and untouched.
        let result = set.restart_and_rewire(&runtime, HashMap::new()).await;
        assert!(
            result.is_err(),
            "prepare_reload must reject invalid providers"
        );
        assert_eq!(set.reload_generation(), 1);
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert!(
            runtime
                .get_registered_provider_config("old-provider")
                .is_some(),
            "old provider must remain published"
        );
        assert!(
            runtime
                .get_registered_provider_config("replacement-provider")
                .is_none(),
            "replacement provider must not publish"
        );
        assert!(
            runtime
                .get_registered_provider_config("invalid-provider")
                .is_none(),
            "invalid provider must not publish"
        );
        set.shutdown_once().await;
        host.wait_for_exit().await?;
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
    #[tokio::test(flavor = "current_thread")]
    async fn invalidation_hides_a_buffered_relay_event() -> TestResult {
        let (runner, _host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let endpoint = set.state().generation.endpoints[0].clone();
        let (ui_tx, ui_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let context = EndpointRelayContext {
            state: Arc::downgrade(&set.state),
            channels: Arc::clone(&set.channels),
            endpoint,
            replacement_ready_drop: set.pending_ready_weak(),
        };
        let handles = spawn_ui_relays(&context, ui_rx, None);
        let mut published = set.subscribe_ui();

        assert!(
            ui_tx
                .send(ExtensionUiEvent::Slot(SanitizedSlot {
                    key: "buffered".to_owned(),
                    ..SanitizedSlot::default()
                }))
                .is_ok()
        );
        set.invalidate();
        tokio::task::yield_now().await;

        assert!(set.current_slots().is_empty());
        assert!(matches!(
            published.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        for handle in handles {
            handle.abort();
        }
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_refuses_new_leases_without_extending_the_draining_generation() -> TestResult {
        let (runner, host) = make_runner(snapshot(&["input"])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let generation = Arc::clone(&set.state().generation);
        let draining = set.lease();

        let shutdown_set = Arc::clone(&set);
        let shutdown = tokio::spawn(async move { shutdown_set.shutdown_once().await });
        while !set.state().shutdown_done {
            tokio::task::yield_now().await;
        }
        assert_eq!(generation.leases.load(Ordering::Acquire), 1);
        let rejected = set.lease();
        assert!(rejected.endpoints().is_empty());
        assert_eq!(generation.leases.load(Ordering::Acquire), 1);
        assert!(
            !set.emit_input("after shutdown", None, "user", None)
                .await?
                .handled
        );
        assert_eq!(host.request_count("input"), 0);
        drop(rejected);
        drop(draining);
        shutdown.await?;
        assert_eq!(generation.leases.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_starts_every_endpoint_before_waiting_for_any() -> TestResult {
        let first = make_blocking_shutdown_runner().await?;
        let second = make_blocking_shutdown_runner().await?;
        let first_runner = Arc::clone(&first);
        let second_runner = Arc::clone(&second);
        let (generation, _) = generation_from_endpoints(
            1,
            vec![
                (EndpointKind::TsCompat, "<first>".to_owned(), first),
                (EndpointKind::Native, "<second>".to_owned(), second),
            ],
        );

        let stop = tokio::spawn(async move { stop_generation(&generation).await });
        tokio::time::timeout(TEST_TIMEOUT, async {
            while first_runner.is_running() || second_runner.is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "not every endpoint began shutdown before the first child blocked reaping")?;
        stop.await?;
        Ok(())
    }

    #[tokio::test]
    async fn current_reload_admission_covers_every_active_endpoint_class() -> TestResult {
        let (compat, _compat_host) = make_runner(snapshot(&["input"])).await?;
        let compat_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, compat)]);
        assert!(compat_set.can_reload(), "active compat");

        let (stale_compat, _stale_compat_host) = make_runner(snapshot(&["input"])).await?;
        let stale_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, stale_compat)]);
        stale_set.invalidate();
        assert!(!stale_set.can_reload(), "stale facade");

        let (inactive_compat, _inactive_compat_host) = make_runner(snapshot(&["input"])).await?;
        inactive_compat.invalidate();
        let inactive_set =
            ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, inactive_compat)]);
        assert!(!inactive_set.can_reload(), "inactive compat");

        let (native, _native_host) = make_runner(snapshot(&["input"])).await?;
        let native_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::Native, native)]);
        assert!(!native_set.can_reload(), "native-only");

        let (first, _first_host) = make_runner(snapshot(&["input"])).await?;
        let (second, _second_host) = make_runner(snapshot(&["input"])).await?;
        let multi_set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        assert!(!multi_set.can_reload(), "multi-endpoint");

        let (active_compat, _active_compat_host) = make_runner(snapshot(&["input"])).await?;
        let (inactive_native, _inactive_native_host) = make_runner(snapshot(&["input"])).await?;
        inactive_native.invalidate();
        let compat_with_inactive_sibling = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, active_compat),
            (EndpointKind::Native, inactive_native),
        ]);
        assert!(
            compat_with_inactive_sibling.can_reload(),
            "active compat + inactive sibling"
        );

        compat_set.shutdown_once().await;
        stale_set.shutdown_once().await;
        inactive_set.shutdown_once().await;
        native_set.shutdown_once().await;
        multi_set.shutdown_once().await;
        compat_with_inactive_sibling.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn reload_rejects_every_invalid_current_runtime_class() -> TestResult {
        let (stale_compat, _stale_host) = make_runner(snapshot(&["input"])).await?;
        let stale_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, stale_compat)]);
        stale_set.invalidate();

        let (inactive_compat, _inactive_host) = make_runner(snapshot(&["input"])).await?;
        inactive_compat.invalidate();
        let inactive_set =
            ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, inactive_compat)]);

        let (native, _native_host) = make_runner(snapshot(&["input"])).await?;
        let native_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::Native, native)]);

        let (first, _first_host) = make_runner(snapshot(&["input"])).await?;
        let (second, _second_host) = make_runner(snapshot(&["input"])).await?;
        let multi_set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);

        let runtime = ModelRuntime::create_in_memory().await?;
        let provider_epoch = runtime.provider_mutation_epoch();
        for (scenario, set) in [
            ("stale facade", &stale_set),
            ("inactive compat", &inactive_set),
            ("native-only", &native_set),
            ("multi-endpoint", &multi_set),
        ] {
            assert!(
                set.restart_and_rewire(&runtime, HashMap::new())
                    .await
                    .is_err(),
                "{scenario}"
            );
            assert_eq!(set.reload_generation(), 1, "{scenario}");
            assert_eq!(
                runtime.provider_mutation_epoch(),
                provider_epoch,
                "{scenario}"
            );
        }

        stale_set.shutdown_once().await;
        inactive_set.shutdown_once().await;
        native_set.shutdown_once().await;
        multi_set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn reload_rejects_invalid_prepared_runtime_classes() -> TestResult {
        // Native prepared replacements now commit; cover invalidation on a fresh
        // reloadable compat facade so flags.set==1 and exit_count==1 still hold.
        let (active_compat, _active_host) = make_runner(snapshot(&["input"])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, active_compat)]);
        let runtime = ModelRuntime::create_in_memory().await?;
        let provider_epoch = runtime.provider_mutation_epoch();

        let (replacement, replacement_host) = make_runner(snapshot(&[])).await?;
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_then_invalidation_for_reload(replacement, pending);
        replacement_host.set_response("flags.set", json!({"ok": true}));
        assert!(
            set.restart_and_rewire(&runtime, probe_preserved_flags())
                .await
                .is_err(),
            "prepared then invalidated"
        );
        assert_eq!(replacement_host.request_count("flags.set"), 1);
        assert_eq!(set.reload_generation(), 1, "prepared then invalidated");
        assert!(!set.is_active(), "prepared then invalidated");
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert!(runtime.get_registered_provider_ids().is_empty());
        replacement_host.wait_for_exit().await?;
        assert_eq!(replacement_host.exit_count(), 1);

        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn prepared_reload_admission_requires_one_compat_endpoint() -> TestResult {
        let (compat, _compat_host) = make_runner(snapshot(&[])).await?;
        let compat_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, compat)]);
        assert!(compat_set.can_reload(), "live compat facade");
        assert!(
            compat_set
                .state()
                .generation
                .has_one_active_compat_endpoint()
        );

        let (native, _native_host) = make_runner(snapshot(&[])).await?;
        let native_set = ExtensionRuntimeSet::bind(vec![(EndpointKind::Native, native)]);
        assert!(!native_set.can_reload(), "native-only facade");

        let (first, _first_host) = make_runner(snapshot(&[])).await?;
        let (second, _second_host) = make_runner(snapshot(&[])).await?;
        let multi_set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        assert!(!multi_set.can_reload(), "multi-endpoint live facade");

        // Best-effort prepare accepts multi-endpoint injected replacements.
        let (live, _live_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, live)]);
        let (a, _a_host) = make_runner(snapshot(&[])).await?;
        let (b, _b_host) = make_runner(snapshot(&[])).await?;
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![
                (EndpointKind::TsCompat, "<compat>".to_owned(), a),
                (EndpointKind::Native, "<native>".to_owned(), b),
            ],
        );
        set.inject_prepared_replacement_for_reload(replacement, pending);
        let prepared = set.prepare_reload(HashMap::new()).await?;
        assert!(prepared.generation.is_some());
        drop(prepared);

        compat_set.shutdown_once().await;
        native_set.shutdown_once().await;
        multi_set.shutdown_once().await;
        set.shutdown_once().await;
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
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::Native,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_for_reload(replacement, pending);

        replacement_host.set_response("flags.set", json!({"ok": true}));
        set.restart_and_rewire(&runtime, probe_preserved_flags())
            .await
            .expect("native prepared replacements are accepted");
        assert_eq!(replacement_host.request_count("flags.set"), 1);
        assert_eq!(set.reload_generation(), 2);
        assert!(set.is_active());
        assert!(
            runtime
                .get_registered_provider_config("replacement-provider")
                .is_some()
        );
        assert!(runtime.get_model("old-provider", "old-model").is_none());
        old_host.wait_for_exit().await?;
        assert_eq!(old_host.exit_count(), 1);

        set.shutdown_once().await;
        replacement_host.wait_for_exit().await?;
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
        replacement_host.set_response("flags.set", json!({"ok": true}));

        assert!(
            set.restart_and_rewire(&runtime, probe_preserved_flags())
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
    #[tokio::test]
    async fn runtime_feedback_empty_discovery_stays_hostless() {
        let started = ExtensionRuntimeSet::start(Vec::new(), String::new(), false).await;
        assert!(
            started.set.is_none(),
            "empty discovery started an extension host"
        );
        assert!(started.diagnostics.is_empty());
    }

    #[test]
    fn runtime_feedback_all_failed_classifications_plan_builtins() {
        let missing = "/definitely/missing/runtime-feedback-extension";
        let (classified, diagnostics) = classify_paths(&[missing.to_owned()]);
        assert!(classified.is_empty());
        assert_eq!(diagnostics.len(), 1);
        let plans = plan_endpoints(&classified);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].kind, EndpointKind::TsCompat);
        assert!(plans[0].builtins);
        assert!(plans[0].entries.is_empty());
        assert_eq!(plans[0].label, "<builtins>");
    }

    #[test]
    fn runtime_feedback_secondary_compat_argv_orders_no_builtins_after_script() -> TestResult {
        let plans = plan_endpoints(&[
            classified(ExtensionRuntime::TsCompat, "first.ts"),
            classified(ExtensionRuntime::Native, "/native"),
            classified(ExtensionRuntime::TsCompat, "second.ts"),
        ]);
        let resolved = Ok(HostSpec {
            source: HostSource::Env(PathBuf::from("/bun")),
            program: PathBuf::from("/bun"),
            args: vec!["/bundle/pi-extension-host.js".to_owned()],
        });
        let specs = plans
            .iter()
            .map(|plan| endpoint_host_spec(plan, Some(&resolved)))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            specs[0].args,
            ["/bundle/pi-extension-host.js"],
            "built-ins compatibility host argv changed"
        );
        assert!(
            specs[1].args.is_empty(),
            "native host gained compatibility argv"
        );
        assert_eq!(
            specs[2].args,
            ["/bundle/pi-extension-host.js", "--no-builtins"],
            "secondary compatibility flag must follow the Bun script argument"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runtime_feedback_reload_skips_classification_and_load_diagnostics() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&["input"])).await?;
        let (replacement, replacement_host) = make_runner(snapshot(&["input"])).await?;
        replacement_host.set_response("flags.set", json!({"ok": true}));
        let (generation, pending) =
            generation_from_endpoints(1, vec![(EndpointKind::TsCompat, "<old>".to_owned(), old)]);
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            vec!["/definitely/missing/reload-diagnostic".to_owned()],
            String::new(),
            false,
        ));
        set.install(pending);
        let (replacement, pending) = generation_from_endpoints(
            2,
            vec![(
                EndpointKind::TsCompat,
                "<replacement>".to_owned(),
                replacement,
            )],
        );
        set.inject_prepared_replacement_with_diagnostics_for_reload(
            replacement,
            pending,
            vec![ExtensionSetDiagnostic {
                path: "broken-sibling.ts".to_owned(),
                message: "load failed".to_owned(),
            }],
        );
        let runtime = ModelRuntime::create_in_memory().await?;
        let result = set.restart_and_rewire(&runtime, HashMap::new()).await?;
        assert!(
            result.diagnostics.iter().any(|diagnostic| {
                diagnostic.path == "broken-sibling.ts" && diagnostic.message == "load failed"
            }),
            "reload should return injected load diagnostics: {:?}",
            result.diagnostics
        );
        assert_eq!(set.reload_generation(), 2);
        assert!(set.is_active());
        old_host.wait_for_exit().await?;
        set.shutdown_once().await;
        replacement_host.wait_for_exit().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_feedback_spawned_replacement_failure_is_transactional() -> TestResult {
        let (old, old_host) = make_runner(snapshot(&["input"])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, old)]);
        let old_generation = set.reload_generation();

        let directory = tempfile::tempdir()?;
        write_native_snapshot_host(directory.path(), snapshot(&[]))?;
        let healthy = directory.path().join("replacement");
        let failing = directory.path().join("failing");
        let marker = directory.path().join("spawned");
        std::fs::write(
            &failing,
            format!(
                "#!/bin/sh\nprintf spawned > '{}'\nexit 7\n",
                marker.display()
            ),
        )?;
        let mut permissions = std::fs::metadata(&failing)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&failing, permissions)?;
        let plans = vec![
            EndpointPlan {
                position: 0,
                kind: EndpointKind::Native,
                entries: vec![healthy.to_string_lossy().into_owned()],
                diagnostic_paths: vec!["healthy".to_owned()],
                builtins: false,
                label: "healthy".to_owned(),
            },
            EndpointPlan {
                position: 1,
                kind: EndpointKind::Native,
                entries: vec![failing.to_string_lossy().into_owned()],
                diagnostic_paths: vec!["failing".to_owned()],
                builtins: false,
                label: "failing".to_owned(),
            },
        ];
        let build = build_generation(
            2,
            plans,
            &directory.path().to_string_lossy(),
            false,
            GenerationBuildPolicy::RequireAllEndpointStarts,
        )
        .await;
        assert!(marker.exists(), "failing replacement was never spawned");
        assert!(build.generation.is_none());
        assert!(build.pending.is_empty());
        assert!(build.endpoint_start_failure.is_some());
        assert_eq!(set.reload_generation(), old_generation);
        assert!(set.is_active(), "published old generation was disturbed");
        set.shutdown_once().await;
        old_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_feedback_or_folds_tool_result_terminate() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&["tool_result"])).await?;
        let (second, second_host) = make_runner(snapshot(&["tool_result"])).await?;
        first_host.set_response("tool_result", json!({"terminate": true}));
        second_host.set_response("tool_result", json!({"terminate": false}));
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        let result = set
            .emit_tool_result(
                "tool",
                "call",
                Map::new(),
                Vec::<ToolResultContent>::new(),
                Value::Null,
                false,
            )
            .await?
            .ok_or("tool result fold returned no change")?;
        assert_eq!(
            result.terminate,
            Some(true),
            "later false overwrote an earlier true terminate"
        );
        set.shutdown_once().await;
        first_host.wait_for_exit().await?;
        second_host.wait_for_exit().await?;
        Ok(())
    }

    async fn wait_for_retirement(set: &ExtensionRuntimeSet, endpoint: EndpointId) -> TestResult {
        tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if set.state().retired.contains(&endpoint) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "endpoint was not retired")?;
        Ok(())
    }

    #[tokio::test]
    async fn fatal_retirement_aborts_only_owner_pending_ready() -> TestResult {
        let (owner_runner, owner_host) = make_runner(snapshot(&[])).await?;
        let (other_runner, other_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, owner_runner),
            (EndpointKind::Native, other_runner),
        ]);
        let (owner, other) = {
            let state = set.state();
            (
                state.generation.endpoints[0].id,
                state.generation.endpoints[1].id,
            )
        };
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let mut ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload::empty_for_test(),
                    model_runtime: runtime,
                },
            )
            .map_err(|_| "initial pending install was rejected")?;
        let route = set
            .state()
            .allocate_route(owner, 77)
            .ok_or("owner route allocation failed")?;
        set.respond_reload(route, Ok(Some(&token))).await?;
        owner_host
            .wait_for_response(protocol::SESSION_RELOAD_METHOD, 77)
            .await?;

        other_host.close().await;
        wait_for_retirement(&set, other).await?;
        assert!(
            set.is_pending_busy(),
            "retiring an unrelated endpoint aborted the pending operation"
        );
        assert!(matches!(
            ready_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        owner_host.close().await;
        wait_for_retirement(&set, owner).await?;
        let result = tokio::time::timeout(TEST_TIMEOUT, ready_rx).await;
        assert!(
            matches!(result, Ok(Err(_))),
            "retiring the owner must wake the pending ready waiter"
        );
        assert!(!set.is_pending_busy());
        assert!(set.take_finalizing(&token).is_none());
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn fatal_retirement_preserves_finalizing_ready() -> TestResult {
        let (runner, host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let owner = set.state().generation.endpoints[0].id;
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload::empty_for_test(),
                    model_runtime: runtime,
                },
            )
            .map_err(|_| "initial pending install was rejected")?;
        let route = set
            .state()
            .allocate_route(owner, 78)
            .ok_or("owner route allocation failed")?;
        set.respond_reload(route, Ok(Some(&token))).await?;
        host.wait_for_response(protocol::SESSION_RELOAD_METHOD, 78)
            .await?;
        assert!(set.complete_ready(&token));
        ready_rx.await?;

        host.close().await;
        wait_for_retirement(&set, owner).await?;
        let (op, _guard) = set
            .take_finalizing(&token)
            .ok_or("owner retirement removed the finalizing operation")?;
        assert!(matches!(&op, PendingReadyOp::Reload { .. }));
        drop(op);
        assert!(set.finish_finalize(&token));
        set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn session_bridge_operations_require_pending_token_owner() -> TestResult {
        let (owner_runner, owner_host) = make_runner(snapshot(&[])).await?;
        let (other_runner, other_host) = make_runner(snapshot(&[])).await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, owner_runner),
            (EndpointKind::Native, other_runner),
        ]);
        let (owner, other) = {
            let state = set.state();
            (
                state.generation.endpoints[0].id,
                state.generation.endpoints[1].id,
            )
        };
        let runtime = Arc::new(ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: PreparedReload::empty_for_test(),
                    model_runtime: runtime,
                },
            )
            .map_err(|_| "initial pending install was rejected")?;
        let route = set
            .state()
            .allocate_route(owner, 79)
            .ok_or("owner route allocation failed")?;
        set.respond_reload(route, Ok(Some(&token))).await?;
        owner_host
            .wait_for_response(protocol::SESSION_RELOAD_METHOD, 79)
            .await?;

        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementReady {
                token: token.clone(),
                origin: Some(owner),
            }),
            SessionBridgeRoute::Operation
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementReady {
                token: token.clone(),
                origin: Some(other),
            }),
            SessionBridgeRoute::Rejected
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementReady {
                token: token.clone(),
                origin: None,
            }),
            SessionBridgeRoute::Rejected
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementReady {
                token: "stale-token".to_owned(),
                origin: Some(owner),
            }),
            SessionBridgeRoute::Rejected
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::Command {
                envelope: protocol::SessionCommandEnvelope {
                    replacement_token: Some(token.clone()),
                    command: protocol::SessionCommand::SetSessionName {
                        name: "candidate".to_owned(),
                    },
                },
                origin: Some(owner),
            }),
            SessionBridgeRoute::Rejected
        ));

        assert!(set.complete_ready(&token));
        ready_rx.await?;
        assert!(matches!(
            &*set.pending_ready(),
            PendingReadyState::Finalizing {
                token: current,
                owner: Some(bound_owner),
                ..
            } if current == &token && *bound_owner == owner
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementAbort {
                token: token.clone(),
                origin: Some(owner),
            }),
            SessionBridgeRoute::Rejected
        ));

        let (op, guard) = set
            .take_finalizing(&token)
            .ok_or("ready operation was not finalizing")?;
        drop(op);
        assert!(set.finish_finalize(&token));
        drop(guard);
        set.shutdown_once().await;
        owner_host.wait_for_exit().await?;
        other_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_feedback_fatal_endpoint_retires_slots_routes_and_registry() -> TestResult {
        let (live, live_host) = make_runner(json!({
            "tools": [],
            "commands": [{"name": "live"}],
            "shortcuts": [],
            "renderers": [],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (dead, dead_host) = make_runner(json!({
            "tools": [],
            "commands": [{"name": "dead"}],
            "shortcuts": [],
            "renderers": [],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (generation, mut pending) = generation_from_endpoints(
            1,
            vec![
                (EndpointKind::TsCompat, "<live>".to_owned(), live),
                (EndpointKind::Native, "<dead>".to_owned(), dead),
            ],
        );
        pending[0].slots = vec![SanitizedSlot {
            key: "shared".to_owned(),
            lines: vec![vec![SanitizedRun {
                text: "live".to_owned(),
                ..SanitizedRun::default()
            }]],
            ..SanitizedSlot::default()
        }];
        pending[1].slots = vec![
            SanitizedSlot {
                key: "shared".to_owned(),
                lines: vec![vec![SanitizedRun {
                    text: "dead".to_owned(),
                    ..SanitizedRun::default()
                }]],
                ..SanitizedSlot::default()
            },
            SanitizedSlot {
                key: "dead-only".to_owned(),
                ..SanitizedSlot::default()
            },
        ];
        let dead_id = pending[1].endpoint.id;
        let set = Arc::new(ExtensionRuntimeSet::from_generation(
            generation,
            Vec::new(),
            String::new(),
            false,
        ));
        set.install(pending);
        let route = set
            .state()
            .allocate_route(dead_id, 77)
            .ok_or("route allocation failed")?;
        assert!(
            set.registry()
                .commands()
                .iter()
                .any(|item| item.name == "dead")
        );

        let mut registry_changes = set.subscribe_registry_changes();
        let revision = *registry_changes.borrow_and_update();
        dead_host.close().await;
        wait_for_retirement(&set, dead_id).await?;
        assert_ne!(*registry_changes.borrow_and_update(), revision);
        assert!(!set.state().routes.contains_key(&route));
        assert!(
            !set.registry()
                .commands()
                .iter()
                .any(|item| item.name == "dead"),
            "dead endpoint remained in aggregate registry"
        );
        assert!(
            set.registry()
                .commands()
                .iter()
                .any(|item| item.name == "live")
        );
        assert_eq!(set.slot_keys(), ["shared"]);
        assert_eq!(
            set.current_slots()[0].lines[0][0].text,
            "live",
            "live fallback slot was not promoted"
        );
        dead_host.close().await;
        tokio::task::yield_now().await;
        assert_eq!(
            set.slot_keys(),
            ["shared"],
            "duplicate fatal signal mutated slots"
        );
        set.shutdown_once().await;
        live_host.wait_for_exit().await?;
        Ok(())
    }

    fn shared_provider(base_url: &str) -> Value {
        json!({
            "name": "shared-provider",
            "baseUrl": base_url,
            "api": "openai-completions",
            "models": [{
                "id": "shared-model",
                "name": "Shared model",
                "api": "openai-completions",
                "baseUrl": base_url,
                "reasoning": false
            }]
        })
    }

    #[tokio::test]
    async fn runtime_feedback_fatal_endpoint_promotes_provider_owner_once() -> TestResult {
        let (owner, owner_host) = make_runner(json!({
            "providers": [shared_provider("https://owner.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (fallback, fallback_host) = make_runner(json!({
            "providers": [shared_provider("https://fallback.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, owner),
            (EndpointKind::Native, fallback),
        ]);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );
        let epoch = runtime.provider_mutation_epoch();
        let owner_id = EndpointId {
            generation: 1,
            position: 0,
        };
        owner_host.close().await;
        wait_for_retirement(&set, owner_id).await?;
        let config = runtime
            .get_registered_provider_config("shared-provider")
            .ok_or("fallback provider was not registered")?;
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://fallback.example/v1")
        );
        assert_eq!(runtime.provider_mutation_epoch(), epoch + 2);
        owner_host.close().await;
        tokio::task::yield_now().await;
        assert_eq!(
            runtime.provider_mutation_epoch(),
            epoch + 2,
            "duplicate fatal signal rewired provider twice"
        );
        set.shutdown_once().await;
        fallback_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_feedback_fatal_non_owner_preserves_provider_owner() -> TestResult {
        let (owner, owner_host) = make_runner(json!({
            "providers": [shared_provider("https://owner.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let (duplicate, duplicate_host) = make_runner(json!({
            "providers": [shared_provider("https://duplicate.example/v1")],
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, owner),
            (EndpointKind::Native, duplicate),
        ]);
        let runtime = ModelRuntime::create_in_memory().await?;
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_, result)| result.is_ok())
        );
        let epoch = runtime.provider_mutation_epoch();
        let duplicate_id = EndpointId {
            generation: 1,
            position: 1,
        };
        duplicate_host.close().await;
        wait_for_retirement(&set, duplicate_id).await?;
        let config = runtime
            .get_registered_provider_config("shared-provider")
            .ok_or("first provider owner was removed")?;
        assert_eq!(config.base_url.as_deref(), Some("https://owner.example/v1"));
        assert_eq!(
            runtime.provider_mutation_epoch(),
            epoch,
            "retiring a duplicate provider mutated effective ownership"
        );
        set.shutdown_once().await;
        owner_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn runtime_feedback_state_broadcasts_do_not_head_of_line_block() -> TestResult {
        let (first, first_host) = make_runner(snapshot(&[])).await?;
        let (second, second_host) = make_runner(snapshot(&[])).await?;
        let first_runner = Arc::clone(&first);
        let set = ExtensionRuntimeSet::bind(vec![
            (EndpointKind::TsCompat, first),
            (EndpointKind::Native, second),
        ]);
        let release = first_host.pause_reads().await?;

        first_runner
            .push_ui_state(&UiStateWire {
                editor_text: "x".repeat(1024 * 1024),
                tools_expanded: false,
            })
            .await;
        let filled = Arc::new(AtomicUsize::new(0));
        let fill_count = Arc::clone(&filled);
        let fill_runner = Arc::clone(&first_runner);
        let fill = tokio::spawn(async move {
            let queued = UiStateWire {
                editor_text: "queued".to_owned(),
                tools_expanded: false,
            };
            for _ in 0..=pi_ext::client::OUTBOUND_CAPACITY {
                fill_runner.push_ui_state(&queued).await;
                fill_count.fetch_add(1, Ordering::Release);
            }
        });
        tokio::time::timeout(TEST_TIMEOUT, async {
            while filled.load(Ordering::Acquire) < pi_ext::client::OUTBOUND_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "first endpoint outbound queue did not fill")?;
        assert!(
            !fill.is_finished(),
            "saturation send must wait for first endpoint capacity"
        );

        let broadcast_set = Arc::clone(&set);
        let broadcast = tokio::spawn(async move {
            broadcast_set
                .push_ui_state(&UiStateWire {
                    editor_text: "broadcast".to_owned(),
                    tools_expanded: false,
                })
                .await;
        });
        second_host.wait_for_frame("ui.state").await?;
        release
            .send(())
            .map_err(|()| "paused fake host already released")?;
        tokio::time::timeout(TEST_TIMEOUT, fill)
            .await
            .map_err(|_| "outbound saturation did not drain")??;
        tokio::time::timeout(TEST_TIMEOUT, broadcast)
            .await
            .map_err(|_| "state broadcast did not complete")??;
        first_host.wait_for_frame("ui.state").await?;
        set.shutdown_once().await;
        first_host.wait_for_exit().await?;
        second_host.wait_for_exit().await?;
        Ok(())
    }
}
