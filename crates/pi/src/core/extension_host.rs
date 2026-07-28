//! Product-side [`ExtensionRunner`] over the pi-ext [`HostClient`].
//!
//! The bundled TypeScript extension host owns the real `ExtensionRunner`
//! (the 15-hook merge table, mutable results, command dispatch, transform
//! chains). Rust is the validation boundary: it sends **one** event request
//! per hook and trusts only the validated typed response, converts the host's
//! registration snapshot into pi-ext tool/provider adapters, pumps unsolicited
//! tool/provider/uiSlot/error traffic into bounded typed subscribers, drops
//! stale generations, isolates every host failure as a single non-retryable
//! `extension_error`, and owns reload generation / slot invalidation /
//! exactly-once shutdown.
//!
//! See the authoritative `agent://ExtensionPlan` for the locked boundary
//! decisions. `AgentSession` never depends on `pi-ext` directly; it talks to
//! this runner through the [`ExtensionRunner`] trait seam defined in
//! [`super::agent_session::extension_runner`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_agent::{AfterToolCallResult, AgentMessage, AgentTool, BeforeToolCallResult};
use pi_ai::{AssistantMessage, AssistantMessageEvent, ToolResultContent};
use pi_ext::adapters::{
    self, CommandRegistration, ExtensionAgentTool, ExtensionProvider, FlagRegistration,
    ProviderRegistration, Registry, RendererKind, RendererRegistration, ShortcutRegistration,
    ToolRegistration,
};
use pi_ext::client::{
    HandshakePolicy, HostClient, HostClientError, HostEvent, HostUiRequest, HostUiResponse,
};
use pi_ext::host::{self, HostError, HostSource, HostSpec};
use pi_ext::protocol::{
    self, DisposeSlot, ExtensionErrorEvent, FlagSnapshotEntry, FlagValueWire, FlagsSetRequest,
    FlagsSetResponse, FrameId, NotifyRequest, ProviderEvent, ProviderSnapshotEntry,
    RegistrySnapshot as RegistrySnapshotWire, SessionCommand, SessionCompactRequest,
    SessionSetModelRequest, SessionStateWire, ShortcutExecuteRequest, ShortcutExecuteResponse,
    ThemeSet, ThemeUpdate, ToolUpdate, UiControl, UiEventRequest, UiEventResponse, UiSlot,
    UiStateWire,
};
use pi_ext::sanitize::{SanitizedSlot, sanitize_slot};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc, watch};

use super::agent_session::events::AgentSessionEvent;
use super::agent_session::extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, InputTransformResult,
};
use super::extension_manifest::{ClassifiedExtension, ExtensionMode, classify_extension};
use super::model_runtime::{
    ModelRuntime, ModelRuntimeError, ProviderConfigInput, ProviderModelDefinition,
};
use super::resources::{ExtensionResourcePath, ResourceExtensionPaths};

/// Lifecycle hook deadline (control RPC).
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for the `hello` handshake + extension load.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded capacity for the tool-update / provider-event / error broadcasts.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Open method string: client requests the host's registration snapshot.
pub const LOAD_METHOD: &str = "extensions.load";

/// Open method string: dispatch a registered slash command.
pub const COMMAND_EXECUTE_METHOD: &str = "command.execute";
/// Private compact streaming update request. Extensions still observe `message_update`.
pub const MESSAGE_UPDATE_DELTA_METHOD: &str = "message_update_delta";

/// Open method string: render an extension tool call/result as HTML (export).
pub const TOOL_RENDER_HTML_METHOD: &str = "tool.renderHtml";

/// The 33 lifecycle event `type` discriminants mirrored from the reference
/// `ExtensionAPI.on()` overloads. The host reports which of these have at
/// least one handler; Rust gates IPC on that set.
pub const ALL_EVENT_TYPES: &[&str] = &[
    "project_trust",
    "resources_discover",
    "session_start",
    "session_info_changed",
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_compact",
    "session_shutdown",
    "session_before_tree",
    "session_tree",
    "context",
    "before_provider_request",
    "before_provider_headers",
    "after_provider_response",
    "before_agent_start",
    "agent_start",
    "agent_end",
    "agent_settled",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "model_select",
    "thinking_level_select",
    "tool_call",
    "tool_result",
    "user_bash",
    "input",
];

/// Which phase of an extension tool to render as HTML.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRenderPhase {
    /// Render the tool-call invocation (`renderCall`).
    Call,
    /// Render the tool-result payload (`renderResult`).
    Result,
}

impl ToolRenderPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Result => "result",
        }
    }
}

/// Sanitized extension UI activity delivered to an active product mode.
#[derive(Debug, Clone)]
pub enum ExtensionUiEvent {
    /// Fire-and-forget notification.
    Notify(NotifyRequest),
    /// Sanitized keyed slot update.
    Slot(SanitizedSlot),
    /// Keyed slot disposal.
    Dispose {
        /// Stable extension widget key to remove.
        key: String,
    },
    /// Extension `setTheme` application request (string or object form).
    ThemeSet(ThemeSet),
    /// Extension fire-and-forget UI control (`ui.setStatus`, `ui.setEditorText`, …).
    UiControl(UiControl),
}

/// One item on the claimed session-action bridge.
///
/// Delivered by [`HostExtensionRunner::take_session_bridge`]; the claiming
/// session task applies each item against the live `AgentSession` and answers
/// `SetModel` via [`HostExtensionRunner::respond_set_model`].
#[derive(Debug, Clone)]
pub enum SessionBridgeEvent {
    /// Fire-and-forget extension session action.
    Command(SessionCommand),
    /// Correlated `pi.setModel` request.
    SetModel {
        /// Host correlation id (echo into `respond_set_model`).
        id: FrameId,
        /// Requested model payload.
        request: SessionSetModelRequest,
    },
    /// Correlated `ctx.compact` request.
    Compact {
        /// Host correlation id (echo into `respond_compact`).
        id: FrameId,
        /// Compact request payload.
        request: SessionCompactRequest,
    },
}

/// Failure while starting the extension host.
#[derive(Debug, thiserror::Error)]
pub enum HostStartError {
    /// No host executable could be resolved.
    #[error("extension host not available: {0}")]
    Resolve(#[from] HostError),
    /// The host process could not be spawned.
    #[error("extension host spawn failed: {0}")]
    Spawn(String),
    /// The `hello` handshake failed (version mismatch or transport).
    #[error("extension host handshake failed: {0}")]
    Handshake(String),
    /// The registration snapshot could not be loaded or decoded.
    #[error("extension host load failed: {0}")]
    Load(String),
    /// Validated flags could not be synchronized to the host.
    #[error("extension host flag synchronization failed: {0}")]
    FlagSync(String),
}

impl From<HostClientError> for HostStartError {
    fn from(value: HostClientError) -> Self {
        match value {
            HostClientError::Spawn { message } => Self::Spawn(message),
            HostClientError::Handshake { message } => Self::Handshake(message),
            other => Self::Load(other.to_string()),
        }
    }
}

/// Path-qualified nonfatal extension-host failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionHostDiagnostic {
    /// Discovery or extension path that owns the failed operation.
    pub path: String,
    /// Host, flag, or provider failure detail.
    pub message: String,
}

impl ExtensionHostDiagnostic {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ExtensionHostDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Extension \"{}\" error: {}",
            self.path, self.message
        )
    }
}

/// Replacement host prepared while the current host remains fully live.
pub struct PreparedHostRestart {
    runner: Arc<HostExtensionRunner>,
    diagnostics: Vec<ExtensionHostDiagnostic>,
}

/// Committed replacement and every nonfatal preparation/commit diagnostic.
pub struct HostRestartResult {
    /// Live replacement runner.
    pub runner: Arc<HostExtensionRunner>,
    /// Deterministically ordered load, flag, then provider diagnostics.
    pub diagnostics: Vec<ExtensionHostDiagnostic>,
}

// ---------------------------------------------------------------------------
// Registration snapshot wire types (host → Rust load response)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtensionsLoadRequest<'a> {
    extension_paths: &'a [String],
    cwd: &'a str,
    project_trusted: bool,
}

fn register_snapshot_flag(flag: FlagSnapshotEntry, snapshot: &mut RegistrySnapshot) {
    let kind = if flag.kind == "boolean" {
        adapters::FlagKind::Boolean
    } else {
        adapters::FlagKind::String
    };
    let default = flag.default.as_ref().map(|value| match value {
        FlagValueWire::Boolean(value) => value.to_string(),
        FlagValueWire::String(value) => value.clone(),
    });
    let description = (!flag.description.is_empty()).then_some(flag.description);
    let extension_path = (!flag.extension_path.is_empty()).then_some(flag.extension_path);
    if !snapshot.registry.register_flag(FlagRegistration {
        name: flag.name.clone(),
        description,
        kind,
        default,
        extension_path,
    }) {
        return;
    }

    let selected = flag.value.or(flag.default);
    let value = selected.map_or_else(
        || {
            if kind == adapters::FlagKind::Boolean {
                Value::Bool(false)
            } else {
                Value::String(String::new())
            }
        },
        |value| match value {
            FlagValueWire::Boolean(value) => Value::Bool(value),
            FlagValueWire::String(value) => Value::String(value),
        },
    );
    snapshot.flag_values.insert(flag.name, value);
}

fn decode_provider_models(
    models: Option<Value>,
) -> Result<Option<Vec<ProviderModelDefinition>>, String> {
    let Some(models) = models else {
        return Ok(None);
    };
    let Value::Array(models) = models else {
        return Err("models must be an array".to_owned());
    };
    serde_json::from_value(Value::Array(models))
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Built registry snapshot: pi-ext [`Registry`] plus ready tool/provider
/// adapters and the handler-presence set.
#[derive(Default)]
struct RegistrySnapshot {
    /// Aggregate registrations (first-wins dedup applied on build).
    registry: Registry,
    /// Ordered, undeduplicated shortcut registrations for product last-wins resolution.
    raw_shortcuts: Vec<ShortcutRegistration>,
    /// Extension tool adapters keyed by tool name.
    tools: HashMap<String, Arc<dyn AgentTool>>,
    /// Lifecycle event types with at least one handler.
    handlers: HashSet<String>,
    /// Whether terminal input must be offered to the host before native dispatch.
    terminal_input: bool,
    /// Resolved flag values (host value if present, else default).
    flag_values: HashMap<String, Value>,
    /// Provider config inputs keyed by provider id (for `ModelRuntime` registration).
    provider_configs: HashMap<String, ProviderConfigInput>,
    /// Provider ids that expose a host-side `streamSimple` handler.
    stream_provider_ids: HashSet<String>,
    /// Optional extension path per provider (diagnostics).
    provider_extension_paths: HashMap<String, String>,
    /// Host-reported per-path load errors.
    load_errors: Vec<(String, String)>,
}

fn register_snapshot_provider(provider: ProviderSnapshotEntry, snapshot: &mut RegistrySnapshot) {
    let ProviderSnapshotEntry {
        name,
        stream_simple,
        base_url,
        api,
        display_name,
        api_key,
        headers,
        auth_header,
        models,
        extension_path,
    } = provider;
    let models = match decode_provider_models(models) {
        Ok(models) => models,
        Err(error) => {
            let path = extension_path.clone().unwrap_or_else(|| name.clone());
            snapshot
                .load_errors
                .push((path, format!("provider {name:?} models: {error}")));
            return;
        }
    };
    let config = ProviderConfigInput {
        name: display_name,
        base_url,
        api_key,
        api,
        headers,
        auth_header,
        models,
        model_overrides: None,
        oauth: None,
    };
    if !snapshot.registry.register_provider(ProviderRegistration {
        name: name.clone(),
        base_url: config.base_url.clone(),
        api: config.api.clone(),
    }) {
        return;
    }
    snapshot.provider_configs.insert(name.clone(), config);
    if stream_simple {
        snapshot.stream_provider_ids.insert(name.clone());
    }
    if let Some(path) = extension_path {
        snapshot.provider_extension_paths.insert(name, path);
    }
}

fn build_snapshot(wire: RegistrySnapshotWire, client: &Arc<HostClient>) -> RegistrySnapshot {
    let mut snapshot = RegistrySnapshot {
        terminal_input: wire.terminal_input,
        ..RegistrySnapshot::default()
    };

    for tool in wire.tools {
        let meta = ToolRegistration {
            name: tool.name.clone(),
            label: tool.label,
            description: tool.description,
            parameters: tool.parameters,
            execution_mode: tool.execution_mode,
        };
        // First registration wins (host already dedups; this is the Rust-side
        // trust boundary for a duplicated name).
        if snapshot.registry.register_tool(meta.clone()) {
            let adapter = ExtensionAgentTool::new(meta, Arc::clone(client));
            snapshot
                .tools
                .insert(adapter.name().to_owned(), Arc::new(adapter));
        }
    }

    for command in wire.commands {
        let description = (!command.description.is_empty()).then_some(command.description);
        let source = (!command.source.is_empty()).then_some(command.source);
        let _ = snapshot.registry.register_command(CommandRegistration {
            name: command.name,
            description,
            source,
        });
    }

    for shortcut in wire.shortcuts {
        let description = (!shortcut.description.is_empty()).then_some(shortcut.description);
        let extension_path =
            (!shortcut.extension_path.is_empty()).then_some(shortcut.extension_path);
        let registration = ShortcutRegistration {
            key: shortcut.key,
            description,
            extension_path,
        };
        snapshot.raw_shortcuts.push(registration.clone());
        let _ = snapshot.registry.register_shortcut(registration);
    }

    for flag in wire.flags {
        register_snapshot_flag(flag, &mut snapshot);
    }

    for renderer in wire.renderers {
        let _ = snapshot.registry.register_renderer(RendererRegistration {
            kind: match renderer.kind.as_str() {
                "tool" => RendererKind::Tool,
                "widget" => RendererKind::Widget,
                _ => RendererKind::Message,
            },
            name: renderer.name,
        });
    }

    for provider in wire.providers {
        register_snapshot_provider(provider, &mut snapshot);
    }

    for error in wire.errors {
        let path = if error.path.is_empty() {
            "<unknown>".to_owned()
        } else {
            error.path
        };
        let message = if error.error.is_empty() {
            "extension load failed".to_owned()
        } else {
            error.error
        };
        snapshot.load_errors.push((path, message));
    }

    snapshot.handlers = wire.handlers.into_iter().collect();
    let _ = wire.extensions;
    snapshot
}

// ---------------------------------------------------------------------------
// Hook response wire types (validated typed responses trusted by Rust)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelWire {
    #[serde(default)]
    cancel: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeforeToolCallWire {
    #[serde(default)]
    block: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    input: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AfterToolCallWire {
    #[serde(default)]
    content: Option<Vec<ToolResultContent>>,
    #[serde(default)]
    details: Option<Value>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    terminate: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
enum InputTransformWire {
    Continue,
    Transform {
        text: String,
        #[serde(default)]
        images: Option<Value>,
    },
    Handled,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BeforeAgentStartWire {
    #[serde(default)]
    messages: Vec<AgentMessage>,
    #[serde(default)]
    system_prompt: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourcePathWire {
    path: String,
    extension_path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourcesDiscoverWire {
    #[serde(default, rename = "skillPaths")]
    skills: Option<Vec<ResourcePathWire>>,
    #[serde(default, rename = "promptPaths")]
    prompts: Option<Vec<ResourcePathWire>>,
    #[serde(default, rename = "themePaths")]
    themes: Option<Vec<ResourcePathWire>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MessageEndWire {
    message: Option<AgentMessage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandExecuteWire {
    #[serde(default)]
    ok: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolRenderHtmlWire {
    #[serde(default)]
    html: Option<String>,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Slot subscription state: latest sanitized slot (or `None` when disposed).
type SlotWatch = watch::Sender<Option<SanitizedSlot>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingKind {
    Ui,
    SetModel,
    Compact,
}

#[derive(Clone, Copy, Debug)]
struct PendingRoute {
    endpoint_id: u64,
    local_id: FrameId,
    kind: PendingKind,
}

#[derive(Clone, Debug)]
struct SlotRoute {
    endpoint_id: u64,
    local_key: String,
}

struct AggregateState {
    endpoint_count: usize,
    startup_errors: Vec<(String, String)>,
    slots: RwLock<HashMap<String, SlotWatch>>,
    slot_routes: RwLock<HashMap<String, SlotRoute>>,
    pending_routes: StdMutex<HashMap<FrameId, PendingRoute>>,
    next_frame_id: AtomicU64,
    tool_updates_tx: broadcast::Sender<ToolUpdate>,
    provider_events_tx: broadcast::Sender<ProviderEvent>,
    errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    ui_tx: broadcast::Sender<ExtensionUiEvent>,
    ui_requests_tx: mpsc::Sender<HostUiRequest>,
    ui_requests_rx: StdMutex<Option<mpsc::Receiver<HostUiRequest>>>,
    ui_requests_claimed: AtomicBool,
    session_bridge_tx: mpsc::Sender<SessionBridgeEvent>,
    session_bridge_rx: StdMutex<Option<mpsc::Receiver<SessionBridgeEvent>>>,
    session_bridge_claimed: AtomicBool,
    reload_generation: AtomicU64,
    theme_generation: AtomicU64,
}

impl AggregateState {
    fn new(endpoint_count: usize, startup_errors: Vec<(String, String)>) -> Self {
        let (tool_updates_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (provider_events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (errors_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_requests_tx, ui_requests_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (session_bridge_tx, session_bridge_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            endpoint_count,
            startup_errors,
            slots: RwLock::new(HashMap::new()),
            slot_routes: RwLock::new(HashMap::new()),
            pending_routes: StdMutex::new(HashMap::new()),
            next_frame_id: AtomicU64::new(1),
            tool_updates_tx,
            provider_events_tx,
            errors_tx,
            ui_tx,
            ui_requests_tx,
            ui_requests_rx: StdMutex::new(Some(ui_requests_rx)),
            ui_requests_claimed: AtomicBool::new(false),
            session_bridge_tx,
            session_bridge_rx: StdMutex::new(Some(session_bridge_rx)),
            session_bridge_claimed: AtomicBool::new(false),
            reload_generation: AtomicU64::new(1),
            theme_generation: AtomicU64::new(0),
        }
    }

    fn publish_error(&self, code: &str, message: &str, data: Option<Value>) {
        let _ = self.errors_tx.send(ExtensionErrorEvent {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable: false,
            data,
        });
    }

    fn next_route_id(&self) -> FrameId {
        loop {
            let id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    fn insert_route(&self, endpoint: &Endpoint, local_id: FrameId, kind: PendingKind) -> FrameId {
        let id = if self.endpoint_count == 1 {
            local_id
        } else {
            self.next_route_id()
        };
        let route = PendingRoute {
            endpoint_id: endpoint.id,
            local_id,
            kind,
        };
        self.pending_routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, route);
        id
    }

    fn take_route(&self, id: FrameId, kind: PendingKind) -> Option<PendingRoute> {
        let mut routes = self
            .pending_routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let route = routes.get(&id).copied()?;
        if route.kind != kind {
            return None;
        }
        routes.remove(&id)
    }

    fn remove_endpoint_routes(&self, endpoint_id: u64) {
        self.pending_routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, route| route.endpoint_id != endpoint_id);
    }

    fn external_slot_key(&self, endpoint_id: u64, local_key: &str) -> String {
        if self.endpoint_count == 1 {
            local_key.to_owned()
        } else {
            format!("endpoint-{endpoint_id}:{local_key}")
        }
    }

    fn slot_send(&self, endpoint: &Endpoint, mut slot: SanitizedSlot) {
        if !endpoint.active() {
            return;
        }
        let external_key = self.external_slot_key(endpoint.id, &slot.key);
        let local_key = std::mem::replace(&mut slot.key, external_key.clone());
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        if !endpoint.active() {
            return;
        }
        let sender = slots
            .entry(external_key.clone())
            .or_insert_with(|| watch::channel(None).0);
        sender.send_replace(Some(slot.clone()));
        if let Ok(mut routes) = self.slot_routes.write() {
            routes.insert(
                external_key,
                SlotRoute {
                    endpoint_id: endpoint.id,
                    local_key,
                },
            );
        }
        let _ = self.ui_tx.send(ExtensionUiEvent::Slot(slot));
    }

    fn slot_dispose(&self, endpoint: &Endpoint, local_key: &str) {
        if !endpoint.active() {
            return;
        }
        let external_key = self.external_slot_key(endpoint.id, local_key);
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        // Mirror slot_send: a dispose buffered before invalidate/reload must
        // not win the lock race against teardown and emit a duplicate
        // disposal (or kill a re-registered slot) afterwards.
        if !endpoint.active() {
            return;
        }
        if let Some(sender) = slots.remove(&external_key) {
            sender.send_replace(None);
        }
        if let Ok(mut routes) = self.slot_routes.write() {
            routes.remove(&external_key);
        }
        let _ = self
            .ui_tx
            .send(ExtensionUiEvent::Dispose { key: external_key });
    }

    fn dispose_endpoint_slots(&self, endpoint_id: u64) {
        // Match slot_send / slot_dispose / dispose_all_slots: slots then routes.
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        let Ok(mut routes) = self.slot_routes.write() else {
            return;
        };
        routes.retain(|key, route| {
            if route.endpoint_id == endpoint_id {
                if let Some(sender) = slots.remove(key) {
                    sender.send_replace(None);
                }
                let _ = self
                    .ui_tx
                    .send(ExtensionUiEvent::Dispose { key: key.clone() });
                false
            } else {
                true
            }
        });
    }

    fn dispose_all_slots(&self) {
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        for (key, sender) in slots.drain() {
            sender.send_replace(None);
            let _ = self.ui_tx.send(ExtensionUiEvent::Dispose { key });
        }
        if let Ok(mut routes) = self.slot_routes.write() {
            routes.clear();
        }
    }
}

/// Load-time parameters shared by every endpoint start path: the working
/// directory handed to `extensions.load`, the project-trust flag, and the
/// per-hook RPC timeout.
#[derive(Clone)]
struct StartContext {
    load_cwd: String,
    project_trusted: bool,
    hook_timeout: Duration,
}

struct Endpoint {
    id: u64,
    client: Arc<HostClient>,
    snapshot: RwLock<RegistrySnapshot>,
    flag_values: RwLock<HashMap<String, Value>>,
    /// Resolved load paths handed to this endpoint's `extensions.load`.
    extension_paths: Vec<String>,
    disabled: AtomicBool,
    stale: AtomicBool,
    shutdown_done: AtomicBool,
    shutdown_lock: tokio::sync::Mutex<()>,
    hook_timeout: Duration,
}

impl Endpoint {
    fn new(
        id: u64,
        client: Arc<HostClient>,
        snapshot: RegistrySnapshot,
        extension_paths: Vec<String>,
        context: &StartContext,
    ) -> Self {
        let flag_values = snapshot.flag_values.clone();
        Self {
            id,
            client,
            snapshot: RwLock::new(snapshot),
            flag_values: RwLock::new(flag_values),
            extension_paths,
            disabled: AtomicBool::new(false),
            stale: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
            shutdown_lock: tokio::sync::Mutex::new(()),
            hook_timeout: context.hook_timeout,
        }
    }

    fn has_handlers(&self, event: &str) -> bool {
        self.active()
            && self
                .snapshot
                .read()
                .is_ok_and(|guard| guard.handlers.contains(event))
    }

    fn active(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed) && !self.stale.load(Ordering::Relaxed)
    }

    async fn hook_request(
        &self,
        method: &str,
        payload: Value,
    ) -> Result<protocol::Frame, HostClientError> {
        if !self.active() {
            return Err(HostClientError::NotRunning);
        }
        self.client
            .request_raw(method, payload, self.hook_timeout)
            .await
    }

    fn report_host_error(&self, aggregate: &AggregateState, err: &HostClientError) {
        let fatal = matches!(
            err,
            HostClientError::Closed { .. }
                | HostClientError::Protocol { .. }
                | HostClientError::NotRunning
        );
        if fatal {
            self.disabled.store(true, Ordering::Relaxed);
        }
        aggregate.publish_error(error_code(err), &err.to_string(), None);
    }
}

fn error_code(err: &HostClientError) -> &'static str {
    match err {
        HostClientError::Handshake { .. } => "extension_handshake",
        HostClientError::Timeout { .. } => "extension_timeout",
        HostClientError::Cancelled { .. } => "extension_cancelled",
        HostClientError::Closed { .. } => "extension_closed",
        HostClientError::Protocol { .. } => "extension_protocol",
        HostClientError::Remote { .. } => "extension_remote",
        HostClientError::Spawn { .. } => "extension_spawn",
        HostClientError::NotRunning => "extension_not_running",
        HostClientError::Payload(_) => "extension_payload",
        HostClientError::StreamOverflow { .. } => "extension_stream_overflow",
        HostClientError::OutboundCancelFull { .. } => "extension_outbound_capacity",
    }
}

/// Product extension runner backed by a live pi-ext [`HostClient`].
///
/// Construct with [`HostExtensionRunner::start`] (resolve + spawn) or
/// [`HostExtensionRunner::connect`] (pre-built client, used by tests and the
/// reload restart closure). All [`ExtensionRunner`] hooks send a single event
/// request and trust only the validated typed response; the host owns the
/// 15-hook merge. Host failures are isolated as a single non-retryable
/// `extension_error` and never abort the session.
pub struct HostExtensionRunner {
    endpoints: Arc<[Arc<Endpoint>]>,
    aggregate: Arc<AggregateState>,
    /// Original discovery paths in input order, including entries that
    /// failed classification and never produced an endpoint.
    discovery_paths: Vec<String>,
    /// Load working directory accepted at runner construction.
    load_cwd: String,
    /// Project-trust flag accepted at runner construction.
    project_trusted: bool,
}

/// Spawn program selected by the classified endpoint mode. Native plans carry
/// their resolved executable, so native spec construction is total.
#[derive(Clone, Debug)]
enum EndpointProgram {
    /// Bun-hosted script endpoint (compat or lean).
    Bun,
    /// Self-contained native executable endpoint.
    Native(std::path::PathBuf),
}

#[derive(Clone, Debug)]
struct EndpointPlan {
    id: u64,
    mode: ExtensionMode,
    program: EndpointProgram,
    paths: Vec<String>,
    diagnostic_path: String,
    /// Whether this Bun host loads the compact built-in extensions. Exactly
    /// one compat plan — the first in original plan order — is initially
    /// designated the builtins owner. Startup promotes the first surviving
    /// compat plan when that owner fails.
    load_builtins: bool,
}

impl EndpointPlan {
    fn accepts(&self, classified: &ClassifiedExtension) -> bool {
        if self.mode != classified.mode {
            return false;
        }
        match &self.program {
            EndpointProgram::Bun => true,
            EndpointProgram::Native(executable) => executable == &classified.entry,
        }
    }
}

fn current_target_triple() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let environment = if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else if cfg!(target_os = "linux") {
        "gnu"
    } else {
        ""
    };
    match (arch, os, environment) {
        ("x86_64", "linux", env) => format!("x86_64-unknown-linux-{env}"),
        ("aarch64", "linux", env) => format!("aarch64-unknown-linux-{env}"),
        ("x86_64", "macos", _) => "x86_64-apple-darwin".to_owned(),
        ("aarch64", "macos", _) => "aarch64-apple-darwin".to_owned(),
        ("x86_64", "windows", "msvc") => "x86_64-pc-windows-msvc".to_owned(),
        ("aarch64", "windows", "msvc") => "aarch64-pc-windows-msvc".to_owned(),
        _ => format!("{arch}-unknown-{os}"),
    }
}

fn classify_endpoint_plans(
    extension_paths: &[String],
) -> (Vec<EndpointPlan>, Vec<(String, String)>) {
    let target = current_target_triple();
    let mut plans: Vec<EndpointPlan> = Vec::new();
    let mut errors = Vec::new();
    let mut contiguous = false;
    let mut builtins_owned = false;
    for (index, path) in extension_paths.iter().enumerate() {
        match classify_extension(Path::new(path), &target) {
            Ok(classified) => {
                let load_path =
                    if classified.mode == ExtensionMode::Compat && classified.manifest.is_none() {
                        path.clone()
                    } else if classified.mode == ExtensionMode::Native {
                        classified.root.to_string_lossy().into_owned()
                    } else {
                        classified.entry.to_string_lossy().into_owned()
                    };
                if contiguous
                    && let Some(plan) = plans.last_mut()
                    && plan.accepts(&classified)
                {
                    plan.paths.push(load_path);
                    continue;
                }
                let load_builtins = classified.mode == ExtensionMode::Compat && !builtins_owned;
                builtins_owned |= load_builtins;
                plans.push(EndpointPlan {
                    id: index as u64,
                    mode: classified.mode,
                    program: if classified.mode == ExtensionMode::Native {
                        EndpointProgram::Native(classified.entry)
                    } else {
                        EndpointProgram::Bun
                    },
                    paths: vec![load_path],
                    diagnostic_path: path.clone(),
                    load_builtins,
                });
                contiguous = true;
            }
            Err(error) => {
                errors.push((path.clone(), error.to_string()));
                contiguous = false;
            }
        }
    }
    (plans, errors)
}

impl HostExtensionRunner {
    /// Resolve, spawn, handshake, and load all classified extension endpoints.
    ///
    /// # Errors
    ///
    /// Per-endpoint failures are isolated and surfaced through
    /// [`HostExtensionRunner::load_errors`]; an `Err` is reserved for failures
    /// that prevent the aggregate runner itself from being constructed.
    pub async fn start(extension_paths: Vec<String>) -> Result<Arc<Self>, HostStartError> {
        let load_cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::start_with_cwd_and_trust(extension_paths, load_cwd, false).await
    }

    /// Spawn one explicit compatibility host, then bind.
    ///
    /// # Errors
    ///
    /// Returns `HostStartError::Spawn` when the host process cannot be
    /// spawned, and the handshake/load error when binding fails.
    pub async fn spawn_from(
        spec: &HostSpec,
        extension_paths: Vec<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        let client = Arc::new(
            HostClient::spawn(spec).map_err(|error| HostStartError::Spawn(error.to_string()))?,
        );
        let startup = Self::connect(Arc::clone(&client), extension_paths).await;
        Self::finish_startup(&client, startup).await
    }

    /// Bind one compatibility endpoint to a pre-built client.
    ///
    /// # Errors
    ///
    /// Returns the handshake or registry-load `HostStartError` when the
    /// endpoint cannot be bound.
    pub async fn connect(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::connect_with_timeout(client, extension_paths, HOOK_TIMEOUT).await
    }

    /// Bind one compatibility endpoint with a custom hook timeout.
    ///
    /// # Errors
    ///
    /// Returns the handshake or registry-load `HostStartError` when the
    /// endpoint cannot be bound.
    pub async fn connect_with_timeout(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        let load_cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::connect_with_cwd_and_trust(client, extension_paths, load_cwd, false, hook_timeout)
            .await
    }

    /// Bind one compatibility endpoint with an explicit load cwd.
    ///
    /// # Errors
    ///
    /// Returns the handshake or registry-load `HostStartError` when the
    /// endpoint cannot be bound.
    pub async fn connect_with_cwd(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::connect_with_cwd_and_trust(client, extension_paths, load_cwd, false, hook_timeout)
            .await
    }

    /// Bind one compatibility endpoint with explicit cwd and trust.
    ///
    /// # Errors
    ///
    /// Returns the handshake or registry-load `HostStartError` when the
    /// endpoint cannot be bound.
    pub async fn connect_with_cwd_and_trust(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        project_trusted: bool,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        let context = StartContext {
            load_cwd: load_cwd.into(),
            project_trusted,
            hook_timeout,
        };
        let endpoint = Self::connect_endpoint(
            0,
            client,
            extension_paths.clone(),
            &context,
            HandshakePolicy::Compat,
        )
        .await?;
        Ok(Self::from_endpoints(
            vec![endpoint],
            Vec::new(),
            extension_paths,
            &context,
        ))
    }

    async fn connect_endpoint(
        id: u64,
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        context: &StartContext,
        handshake_policy: HandshakePolicy,
    ) -> Result<Arc<Endpoint>, HostStartError> {
        client.handshake_with_policy(handshake_policy).await?;
        let snapshot = Self::load(
            &client,
            &extension_paths,
            &context.load_cwd,
            context.project_trusted,
        )
        .await?;
        Ok(Arc::new(Endpoint::new(
            id,
            client,
            snapshot,
            extension_paths,
            context,
        )))
    }

    fn from_endpoints(
        endpoints: Vec<Arc<Endpoint>>,
        startup_errors: Vec<(String, String)>,
        discovery_paths: Vec<String>,
        context: &StartContext,
    ) -> Arc<Self> {
        let aggregate = Arc::new(AggregateState::new(endpoints.len(), startup_errors));
        let runner = Arc::new(Self {
            endpoints: endpoints.into(),
            aggregate: Arc::clone(&aggregate),
            discovery_paths,
            load_cwd: context.load_cwd.clone(),
            project_trusted: context.project_trusted,
        });
        for endpoint in runner.endpoints.iter() {
            spawn_event_pump(Arc::clone(endpoint), Arc::clone(&aggregate));
        }
        runner
    }

    #[cfg(test)]
    async fn connect_test_endpoints(
        clients: Vec<(u64, Arc<HostClient>, Vec<String>)>,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        let load_cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let context = StartContext {
            load_cwd,
            project_trusted: false,
            hook_timeout,
        };
        let mut discovery_paths = Vec::new();
        let mut futures = Vec::with_capacity(clients.len());
        for (id, client, paths) in clients {
            discovery_paths.extend(paths.iter().cloned());
            futures.push(Self::connect_endpoint(
                id,
                client,
                paths,
                &context,
                HandshakePolicy::Compat,
            ));
        }
        let endpoints = futures::future::try_join_all(futures).await?;
        Ok(Self::from_endpoints(
            endpoints,
            Vec::new(),
            discovery_paths,
            &context,
        ))
    }

    async fn start_classified(
        extension_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::start_classified_with_resolver(
            extension_paths,
            load_cwd,
            project_trusted,
            host::resolve_host,
        )
        .await
    }

    async fn start_classified_with_resolver<F>(
        extension_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
        resolve_bun: F,
    ) -> Result<Arc<Self>, HostStartError>
    where
        F: FnOnce() -> Result<HostSpec, HostError>,
    {
        let (plans, errors) = classify_endpoint_plans(&extension_paths);
        let mut diagnostics = errors
            .into_iter()
            .map(|(path, message)| ExtensionHostDiagnostic::new(path, message))
            .collect::<Vec<_>>();
        let bun_spec = if plans.iter().any(|plan| plan.mode != ExtensionMode::Native) {
            resolve_bun().map(Some).map_err(|error| error.to_string())
        } else {
            Ok(None)
        };
        let context = StartContext {
            load_cwd,
            project_trusted,
            hook_timeout: HOOK_TIMEOUT,
        };
        let startups = plans
            .iter()
            .map(|plan| Self::start_endpoint_plan(plan, &bun_spec, &context));
        let mut live = Vec::new();
        let mut builtins_owner_failed = false;
        for (plan, startup) in plans
            .iter()
            .cloned()
            .zip(futures::future::join_all(startups).await)
        {
            match startup {
                Ok(endpoint) => live.push((plan, endpoint)),
                Err(diagnostic) => {
                    builtins_owner_failed |= plan.load_builtins;
                    diagnostics.push(diagnostic);
                }
            }
        }

        if builtins_owner_failed
            && let Some(index) = live
                .iter()
                .position(|(plan, _)| plan.mode == ExtensionMode::Compat)
        {
            let mut promoted_plan = live[index].0.clone();
            promoted_plan.load_builtins = true;
            match Self::start_endpoint_plan(&promoted_plan, &bun_spec, &context).await {
                Ok(promoted) => {
                    let previous = std::mem::replace(&mut live[index].1, promoted);
                    let _ = previous.client.shutdown().await;
                    previous.stale.store(true, Ordering::Relaxed);
                    live[index].0 = promoted_plan;
                }
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }

        let mut diagnostic_order = HashMap::with_capacity(extension_paths.len());
        for (index, path) in extension_paths.iter().enumerate() {
            diagnostic_order.entry(path.as_str()).or_insert(index);
        }
        diagnostics.sort_by_key(|diagnostic| {
            diagnostic_order
                .get(diagnostic.path.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });

        Ok(Self::from_endpoints(
            live.into_iter().map(|(_, endpoint)| endpoint).collect(),
            diagnostics
                .into_iter()
                .map(|diagnostic| (diagnostic.path, diagnostic.message))
                .collect(),
            extension_paths,
            &context,
        ))
    }

    async fn start_endpoint_plan(
        plan: &EndpointPlan,
        bun_spec: &Result<Option<HostSpec>, String>,
        context: &StartContext,
    ) -> Result<Arc<Endpoint>, ExtensionHostDiagnostic> {
        let spec = match &plan.program {
            EndpointProgram::Native(executable) => Ok(HostSpec {
                source: HostSource::InstalledAsset(executable.clone()),
                program: executable.clone(),
                args: Vec::new(),
            }),
            EndpointProgram::Bun => bun_spec
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|spec| {
                    spec.clone().ok_or_else(|| {
                        if plan.mode == ExtensionMode::Lean {
                            "lean host was not resolved".to_owned()
                        } else {
                            "compat host was not resolved".to_owned()
                        }
                    })
                })
                .map(|mut spec| {
                    if plan.mode == ExtensionMode::Lean {
                        spec.args.push("--lean".to_owned());
                    }
                    if plan.mode == ExtensionMode::Compat && !plan.load_builtins {
                        spec.args.push("--no-builtins".to_owned());
                    }
                    spec
                }),
        }
        .map_err(|error| {
            ExtensionHostDiagnostic::new(
                plan.diagnostic_path.clone(),
                format!("extension host resolution failed: {error}"),
            )
        })?;
        let client = Arc::new(HostClient::spawn(&spec).map_err(|error| {
            ExtensionHostDiagnostic::new(plan.diagnostic_path.clone(), error.to_string())
        })?);
        let policy = if plan.mode == ExtensionMode::Compat {
            HandshakePolicy::Compat
        } else {
            HandshakePolicy::ProtocolOnly
        };
        match Self::connect_endpoint(
            plan.id,
            Arc::clone(&client),
            plan.paths.clone(),
            context,
            policy,
        )
        .await
        {
            Ok(endpoint) => Ok(endpoint),
            Err(error) => {
                let _ = client.shutdown().await;
                Err(ExtensionHostDiagnostic::new(
                    plan.diagnostic_path.clone(),
                    error.to_string(),
                ))
            }
        }
    }

    async fn finish_startup(
        client: &HostClient,
        startup: Result<Arc<Self>, HostStartError>,
    ) -> Result<Arc<Self>, HostStartError> {
        if startup.is_err() {
            let _ = client.shutdown().await;
        }
        startup
    }

    async fn load(
        client: &Arc<HostClient>,
        extension_paths: &[String],
        cwd: &str,
        project_trusted: bool,
    ) -> Result<RegistrySnapshot, HostStartError> {
        let payload = serde_json::to_value(ExtensionsLoadRequest {
            extension_paths,
            cwd,
            project_trusted,
        })
        .map_err(|error| HostStartError::Load(error.to_string()))?;
        let frame = client
            .request_raw(LOAD_METHOD, payload, START_TIMEOUT)
            .await?;
        let wire: RegistrySnapshotWire = serde_json::from_value(frame.payload)
            .map_err(|e| HostStartError::Load(e.to_string()))?;
        wire.validate()
            .map_err(|error| HostStartError::Load(error.to_string()))?;
        Ok(build_snapshot(wire, client))
    }

    /// Borrowed primary client for the legacy one-endpoint API, or `None`
    /// when no endpoint loaded successfully.
    #[must_use]
    pub fn client(&self) -> Option<&Arc<HostClient>> {
        self.endpoints.first().map(|endpoint| &endpoint.client)
    }

    fn endpoint_for_route(&self, route: PendingRoute) -> Result<&Arc<Endpoint>, HostClientError> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.id == route.endpoint_id && endpoint.active())
            .ok_or(HostClientError::NotRunning)
    }

    fn first_owner(&self, predicate: impl Fn(&RegistrySnapshot) -> bool) -> Option<&Arc<Endpoint>> {
        self.endpoints.iter().find(|endpoint| {
            endpoint
                .snapshot
                .read()
                .is_ok_and(|snapshot| predicate(&snapshot))
        })
    }

    /// Original discovery paths in input order, including entries that
    /// failed classification and never produced an endpoint.
    #[must_use]
    pub fn extension_paths(&self) -> Vec<String> {
        self.discovery_paths.clone()
    }

    /// Per-path load, classification, startup, and cross-endpoint collision diagnostics.
    #[must_use]
    pub fn load_errors(&self) -> Vec<(String, String)> {
        let mut errors = self.aggregate.startup_errors.clone();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                errors.extend(snapshot.load_errors.clone());
            }
        }
        let mut seen = HashSet::<(String, String)>::new();
        let mut reported = HashSet::<(String, String)>::new();
        for endpoint in self.endpoints.iter() {
            let Ok(snapshot) = endpoint.snapshot.read() else {
                continue;
            };
            let path = endpoint
                .extension_paths
                .first()
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_owned());
            let registrations = [
                (
                    "tool",
                    snapshot
                        .registry
                        .tools()
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "command",
                    snapshot
                        .registry
                        .commands()
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "shortcut",
                    snapshot
                        .registry
                        .shortcuts()
                        .iter()
                        .map(|item| item.key.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "flag",
                    snapshot
                        .registry
                        .flags()
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "renderer",
                    snapshot
                        .registry
                        .renderers()
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                ),
                (
                    "provider",
                    snapshot
                        .registry
                        .providers()
                        .iter()
                        .map(|item| item.name.as_str())
                        .collect::<Vec<_>>(),
                ),
            ];
            for (kind, names) in registrations {
                for name in names {
                    let key = (kind.to_owned(), name.to_owned());
                    if !seen.insert(key.clone()) && reported.insert(key) {
                        // Shortcuts resolve last-wins: `execute_shortcut` reverse-scans
                        // the endpoints, and the interactive projection shows the same
                        // owner. Every other kind keeps first-registration-wins, so the
                        // diagnostic must not name the wrong owner for shortcuts.
                        let resolution = if kind == "shortcut" {
                            "later registration wins"
                        } else {
                            "first registration wins"
                        };
                        errors.push((
                            path.clone(),
                            format!("duplicate {kind} {name:?}; {resolution}"),
                        ));
                    }
                }
            }
        }
        errors
    }

    #[must_use]
    /// Return provider configs, resolving duplicates by endpoint order.
    pub fn provider_configs(&self) -> HashMap<String, ProviderConfigInput> {
        let mut configs = HashMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for (name, config) in &snapshot.provider_configs {
                    configs
                        .entry(name.clone())
                        .or_insert_with(|| config.clone());
                }
            }
        }
        configs
    }

    #[must_use]
    /// Return first-winning provider ids that own a custom stream handler.
    pub fn stream_provider_ids(&self) -> HashSet<String> {
        let mut selected = HashSet::new();
        let mut seen = HashSet::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for name in snapshot.provider_configs.keys() {
                    if seen.insert(name.clone()) && snapshot.stream_provider_ids.contains(name) {
                        selected.insert(name.clone());
                    }
                }
            }
        }
        selected
    }

    #[must_use]
    /// Return diagnostic extension paths for first-winning providers.
    pub fn provider_extension_paths(&self) -> HashMap<String, String> {
        let mut paths = HashMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for (name, path) in &snapshot.provider_extension_paths {
                    paths.entry(name.clone()).or_insert_with(|| path.clone());
                }
            }
        }
        paths
    }

    #[must_use]
    /// Return flag types, resolving duplicates by endpoint order.
    pub fn registered_flag_types(
        &self,
    ) -> BTreeMap<String, super::agent_session_services::ExtensionFlagType> {
        use super::agent_session_services::ExtensionFlagType;
        let mut flags = BTreeMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for flag in snapshot.registry.flags() {
                    flags
                        .entry(flag.name.clone())
                        .or_insert_with(|| match flag.kind {
                            adapters::FlagKind::Boolean => ExtensionFlagType::Boolean,
                            adapters::FlagKind::String => ExtensionFlagType::String,
                        });
                }
            }
        }
        flags
    }

    #[must_use]
    /// Build provider adapters bound to each first-winning provider's endpoint client.
    pub fn providers(&self) -> HashMap<String, ExtensionProvider> {
        let mut providers = HashMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for provider in snapshot.registry.providers() {
                    providers.entry(provider.name.clone()).or_insert_with(|| {
                        ExtensionProvider::new(provider.name.clone(), Arc::clone(&endpoint.client))
                    });
                }
            }
        }
        providers
    }

    #[must_use]
    /// Register first-winning provider configs and owning stream adapters.
    pub fn register_providers_on(
        &self,
        runtime: &ModelRuntime,
    ) -> Vec<(String, Result<(), ModelRuntimeError>)> {
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        for endpoint in self.endpoints.iter() {
            let Ok(snapshot) = endpoint.snapshot.read() else {
                continue;
            };
            for provider in snapshot.registry.providers() {
                let name = &provider.name;
                let Some(config) = snapshot.provider_configs.get(name) else {
                    continue;
                };
                if !seen.insert(name.clone()) {
                    continue;
                }
                let path = snapshot
                    .provider_extension_paths
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                let outcome = runtime.register_provider(name, config.clone());
                if outcome.is_ok() && snapshot.stream_provider_ids.contains(name) {
                    let adapter =
                        ExtensionProvider::new(name.clone(), Arc::clone(&endpoint.client));
                    runtime.register_extension_stream_provider(name.clone(), Arc::new(adapter));
                }
                results.push((path, outcome));
            }
        }
        results
    }

    /// Unregister every first-winning provider from the model runtime.
    pub fn unregister_providers_from(&self, runtime: &ModelRuntime) {
        for name in self.provider_configs().keys() {
            runtime.unregister_provider(name);
        }
    }

    #[must_use]
    /// Build the ordered first-winning aggregate extension registry.
    pub fn registry(&self) -> Registry {
        let mut registry = Registry::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                let source = &snapshot.registry;
                for item in source.tools() {
                    let _ = registry.register_tool(item.clone());
                }
                for item in source.commands() {
                    let _ = registry.register_command(item.clone());
                }
                for item in source.shortcuts() {
                    let _ = registry.register_shortcut(item.clone());
                }
                for item in source.flags() {
                    let _ = registry.register_flag(item.clone());
                }
                for item in source.renderers() {
                    let _ = registry.register_renderer(item.clone());
                }
                for item in source.providers() {
                    let _ = registry.register_provider(item.clone());
                }
            }
        }
        registry
    }

    #[must_use]
    /// Return raw shortcut registrations in true endpoint and path order.
    pub fn raw_shortcuts(&self) -> Vec<ShortcutRegistration> {
        self.endpoints
            .iter()
            .flat_map(|endpoint| {
                endpoint
                    .snapshot
                    .read()
                    .map(|snapshot| snapshot.raw_shortcuts.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[must_use]
    /// Return the aggregate reload generation.
    pub fn reload_generation(&self) -> u64 {
        self.aggregate.reload_generation.load(Ordering::Relaxed)
    }

    #[must_use]
    /// Return whether any endpoint transport remains live.
    pub fn is_running(&self) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.client.is_running() && !endpoint.disabled.load(Ordering::Relaxed)
        })
    }

    /// Fan out every validated flag value to every endpoint.
    ///
    /// Registry declarations describe flags but do not authorize or route user
    /// values. Each endpoint receives the complete map and updates its local
    /// overlay only after acknowledging it.
    ///
    /// # Errors
    ///
    /// Only runner-global request encoding failure returns `Err`. Every
    /// endpoint transport, decode, or rejection failure is returned as a
    /// path-qualified diagnostic after all siblings have been attempted.
    pub async fn apply_flag_values(
        &self,
        values: &BTreeMap<String, FlagValueWire>,
    ) -> Result<Vec<ExtensionHostDiagnostic>, HostClientError> {
        if values.is_empty() {
            return Ok(Vec::new());
        }
        let payload = protocol::to_payload(&FlagsSetRequest {
            values: values.clone(),
        })
        .map_err(|error| HostClientError::Payload(format!("encode flags.set: {error}")))?;
        let mut diagnostics = Vec::new();
        for endpoint in self.endpoints.iter() {
            let outcome = async {
                let frame = endpoint
                    .hook_request(protocol::FLAGS_SET_METHOD, payload.clone())
                    .await?;
                let response: FlagsSetResponse =
                    protocol::from_payload(&frame.payload).map_err(|error| {
                        HostClientError::Payload(format!("decode flags.set: {error}"))
                    })?;
                response.ok.then_some(()).ok_or_else(|| {
                    HostClientError::Payload("flags.set rejected by extension host".to_owned())
                })
            }
            .await;
            if let Err(error) = outcome {
                endpoint.report_host_error(&self.aggregate, &error);
                diagnostics.push(ExtensionHostDiagnostic::new(
                    endpoint
                        .extension_paths
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "<unknown>".to_owned()),
                    error.to_string(),
                ));
                continue;
            }
            if let Ok(mut flags) = endpoint.flag_values.write() {
                for (name, value) in values {
                    flags.insert(
                        name.clone(),
                        match value {
                            FlagValueWire::Boolean(value) => Value::Bool(*value),
                            FlagValueWire::String(value) => Value::String(value.clone()),
                        },
                    );
                }
            }
        }
        Ok(diagnostics)
    }

    /// Execute a shortcut on its last raw-owning endpoint.
    ///
    /// Shortcut execution is last-wins across endpoints: the reverse scan
    /// finds the final endpoint whose raw shortcut registrations carry `key`,
    /// and a dead last owner yields `NotRunning` with no fallback to an older
    /// duplicate. (Command/tool/provider routing stays first-wins.)
    ///
    /// # Errors
    ///
    /// Returns `HostClientError::NotRunning` when no owning endpoint is
    /// active, or the RPC/encode/decode failure from the owning endpoint.
    pub async fn execute_shortcut(
        &self,
        key: impl Into<String>,
    ) -> Result<ShortcutExecuteResponse, HostClientError> {
        let key = key.into();
        let endpoint =
            self.endpoints
                .iter()
                .rev()
                .find(|endpoint| {
                    endpoint.snapshot.read().is_ok_and(|snapshot| {
                        snapshot.raw_shortcuts.iter().any(|item| item.key == key)
                    })
                })
                .or_else(|| {
                    (self.endpoints.len() == 1)
                        .then(|| self.endpoints.first())
                        .flatten()
                })
                .ok_or(HostClientError::NotRunning)?;
        if !endpoint.active() {
            return Err(HostClientError::NotRunning);
        }
        let payload = protocol::to_payload(&ShortcutExecuteRequest { key }).map_err(|error| {
            HostClientError::Payload(format!("encode shortcut.execute: {error}"))
        })?;
        let frame = endpoint
            .hook_request(protocol::SHORTCUT_EXECUTE_METHOD, payload)
            .await?;
        protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode shortcut.execute: {error}")))
    }

    /// Route a namespaced slot event back to the owning endpoint.
    ///
    /// # Errors
    ///
    /// Returns `HostClientError::NotRunning` when the routed endpoint is gone
    /// or inactive, or the RPC/encode/decode failure from the owner.
    pub async fn send_ui_event(
        &self,
        mut request: UiEventRequest,
    ) -> Result<UiEventResponse, HostClientError> {
        let route = self
            .aggregate
            .slot_routes
            .read()
            .ok()
            .and_then(|routes| routes.get(&request.key).cloned())
            .or_else(|| {
                let endpoint = self.endpoints.first()?;
                (self.endpoints.len() == 1).then(|| SlotRoute {
                    endpoint_id: endpoint.id,
                    local_key: request.key.clone(),
                })
            });
        let Some(route) = route else {
            return Ok(UiEventResponse { delivered: false });
        };
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == route.endpoint_id && endpoint.active())
            .ok_or(HostClientError::NotRunning)?;
        request.key = route.local_key;
        let payload = protocol::to_payload(&request)
            .map_err(|error| HostClientError::Payload(format!("encode uiEvent: {error}")))?;
        let frame = endpoint
            .hook_request(protocol::Method::UiEvent.as_str(), payload)
            .await?;
        protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode uiEvent: {error}")))
    }

    #[must_use]
    /// Subscribe to one external aggregate slot key.
    pub fn subscribe_slot(&self, key: &str) -> watch::Receiver<Option<SanitizedSlot>> {
        if let Ok(mut slots) = self.aggregate.slots.write() {
            return slots
                .entry(key.to_owned())
                .or_insert_with(|| watch::channel(None).0)
                .subscribe();
        }
        watch::channel(None).1
    }

    #[must_use]
    /// Return all live external aggregate slot keys.
    pub fn slot_keys(&self) -> Vec<String> {
        self.aggregate
            .slots
            .read()
            .map(|slots| slots.keys().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    /// Snapshot every live sanitized aggregate slot.
    pub fn current_slots(&self) -> Vec<SanitizedSlot> {
        let mut slots = self
            .aggregate
            .slots
            .read()
            .map(|slots| {
                slots
                    .values()
                    .filter_map(|sender| sender.borrow().clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        slots.sort_by(|left, right| left.key.cmp(&right.key));
        slots
    }

    #[must_use]
    /// Subscribe to tool updates fanned in from every endpoint.
    pub fn subscribe_tool_updates(&self) -> broadcast::Receiver<ToolUpdate> {
        self.aggregate.tool_updates_tx.subscribe()
    }

    #[must_use]
    /// Subscribe to provider events fanned in from every endpoint.
    pub fn subscribe_provider_events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.aggregate.provider_events_tx.subscribe()
    }

    #[must_use]
    /// Subscribe to errors fanned in from every endpoint.
    pub fn subscribe_errors(&self) -> broadcast::Receiver<ExtensionErrorEvent> {
        self.aggregate.errors_tx.subscribe()
    }

    #[must_use]
    /// Return whether any active endpoint handles terminal input.
    pub fn has_terminal_input_handlers(&self) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.active()
                && endpoint
                    .snapshot
                    .read()
                    .is_ok_and(|snapshot| snapshot.terminal_input)
        })
    }

    /// Fold terminal input rewrites in endpoint order and stop when consumed.
    ///
    /// One shared 4ms deadline bounds the whole keypress: before each active
    /// endpoint the remaining budget is computed, a zero remainder stops the
    /// dispatch (later endpoints stay uncontacted), and the remainder is
    /// handed to that endpoint's request.
    ///
    /// # Errors
    ///
    /// With a single endpoint, returns the `terminalInput` RPC failure; with
    /// multiple endpoints, per-endpoint failures are isolated and reported as
    /// `extension_error` events instead.
    pub async fn terminal_input(
        &self,
        data: &str,
    ) -> Result<protocol::TerminalInputResult, HostClientError> {
        let mut current = data.to_owned();
        let mut transformed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(4);
        for endpoint in self.endpoints.iter() {
            if !endpoint.active()
                || !endpoint
                    .snapshot
                    .read()
                    .is_ok_and(|snapshot| snapshot.terminal_input)
            {
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = endpoint
                .client
                .request(
                    protocol::Method::TerminalInput,
                    serde_json::json!({ "data": current }),
                    remaining,
                )
                .await
                .and_then(|frame| {
                    protocol::from_payload::<protocol::TerminalInputResult>(&frame.payload).map_err(
                        |error| HostClientError::Payload(format!("decode terminalInput: {error}")),
                    )
                });
            match result {
                Ok(result) => {
                    if let Some(data) = result.data {
                        current = data;
                        transformed = true;
                    }
                    if result.consume {
                        return Ok(protocol::TerminalInputResult {
                            consume: true,
                            data: transformed.then_some(current),
                        });
                    }
                }
                Err(error) if self.endpoints.len() == 1 => return Err(error),
                Err(error) => endpoint.report_host_error(&self.aggregate, &error),
            }
        }
        Ok(protocol::TerminalInputResult {
            consume: false,
            data: transformed.then_some(current),
        })
    }

    #[must_use]
    /// Subscribe to UI activity fanned in from every endpoint.
    pub fn subscribe_ui(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.aggregate.ui_tx.subscribe()
    }

    #[must_use]
    /// Return the latest aggregate theme generation.
    pub fn theme_generation(&self) -> u64 {
        self.aggregate.theme_generation.load(Ordering::Relaxed)
    }

    /// Broadcast a theme update to every active endpoint in order.
    pub async fn push_theme_update(&self, update: &ThemeUpdate) {
        self.aggregate
            .theme_generation
            .store(update.theme_generation, Ordering::Relaxed);
        let payload = match serde_json::to_value(update) {
            Ok(payload) => payload,
            Err(error) => {
                self.aggregate.publish_error(
                    "extension_protocol",
                    &format!("encode theme.update: {error}"),
                    None,
                );
                return;
            }
        };
        for endpoint in self.endpoints.iter().filter(|endpoint| endpoint.active()) {
            if let Err(error) = endpoint
                .client
                .send_event(protocol::THEME_UPDATE_METHOD, payload.clone())
                .await
            {
                endpoint.report_host_error(&self.aggregate, &error);
            }
        }
    }

    #[must_use]
    /// Claim the sole aggregate UI-request receiver.
    pub fn take_ui_requests(&self) -> Option<mpsc::Receiver<HostUiRequest>> {
        let receiver = self
            .aggregate
            .ui_requests_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if receiver.is_some() {
            self.aggregate
                .ui_requests_claimed
                .store(true, Ordering::Release);
        }
        receiver
    }

    /// Route a synthetic UI response to its generation-matched endpoint.
    ///
    /// # Errors
    ///
    /// Returns `HostClientError::NotRunning` when no pending route matches the
    /// response id or the endpoint is gone, or the response send failure.
    pub async fn respond_ui(&self, response: HostUiResponse) -> Result<(), HostClientError> {
        let aggregate_id = host_ui_response_id(&response);
        let route = self
            .aggregate
            .take_route(aggregate_id, PendingKind::Ui)
            .ok_or(HostClientError::NotRunning)?;
        let endpoint = self.endpoint_for_route(route)?;
        endpoint
            .client
            .respond_ui(retag_ui_response(response, route.local_id))
            .await
    }

    #[must_use]
    /// Return whether any endpoint remains active.
    pub fn is_active(&self) -> bool {
        self.endpoints.iter().any(|endpoint| endpoint.active())
    }

    #[must_use]
    /// Claim the sole aggregate session-action receiver.
    pub fn take_session_bridge(&self) -> Option<mpsc::Receiver<SessionBridgeEvent>> {
        let receiver = self
            .aggregate
            .session_bridge_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if receiver.is_some() {
            self.aggregate
                .session_bridge_claimed
                .store(true, Ordering::Release);
        }
        receiver
    }

    /// Route a synthetic set-model response to its generation-matched endpoint.
    ///
    /// # Errors
    ///
    /// Returns `HostClientError::NotRunning` when no pending route matches the
    /// response id or the endpoint is gone, or the response send failure.
    pub async fn respond_set_model(
        &self,
        id: FrameId,
        success: bool,
    ) -> Result<(), HostClientError> {
        let route = self
            .aggregate
            .take_route(id, PendingKind::SetModel)
            .ok_or(HostClientError::NotRunning)?;
        self.endpoint_for_route(route)?
            .client
            .respond_set_model(route.local_id, success)
            .await
    }

    /// Route a synthetic compact response to its generation-matched endpoint.
    ///
    /// # Errors
    ///
    /// Returns `HostClientError::NotRunning` when no pending route matches the
    /// response id or the endpoint is gone, or the response send failure.
    pub async fn respond_compact(
        &self,
        id: FrameId,
        outcome: Result<Value, String>,
    ) -> Result<(), HostClientError> {
        let route = self
            .aggregate
            .take_route(id, PendingKind::Compact)
            .ok_or(HostClientError::NotRunning)?;
        self.endpoint_for_route(route)?
            .client
            .respond_compact(route.local_id, outcome)
            .await
    }

    /// Broadcast mirrored session state to every active endpoint in order.
    pub async fn push_session_state(&self, state: &SessionStateWire) {
        self.broadcast_event(protocol::SESSION_UPDATE_METHOD, state, "session.update")
            .await;
    }

    /// Broadcast mirrored UI state to every active endpoint in order.
    pub async fn push_ui_state(&self, state: &UiStateWire) {
        self.broadcast_event(protocol::UI_STATE_METHOD, state, "ui.state")
            .await;
    }

    async fn broadcast_event<T: Serialize>(&self, method: &str, value: &T, label: &str) {
        let payload = match serde_json::to_value(value) {
            Ok(payload) => payload,
            Err(error) => {
                self.aggregate.publish_error(
                    "extension_protocol",
                    &format!("encode {label}: {error}"),
                    None,
                );
                return;
            }
        };
        for endpoint in self.endpoints.iter().filter(|endpoint| endpoint.active()) {
            if let Err(error) = endpoint.client.send_event(method, payload.clone()).await {
                endpoint.report_host_error(&self.aggregate, &error);
            }
        }
    }

    /// Render extension tool HTML on the first-owning endpoint.
    ///
    /// Ownership: the endpoint registering the tool itself wins; otherwise
    /// only a renderer whose name matches AND whose kind is
    /// [`RendererKind::Tool`] qualifies — same-named widget/message renderers
    /// never receive `tool.renderHtml`.
    pub async fn render_extension_tool_html(
        &self,
        phase: ToolRenderPhase,
        tool_name: &str,
        payload: &Value,
    ) -> Option<String> {
        let endpoint = self
            .first_owner(|snapshot| {
                snapshot.registry.tool(tool_name).is_some()
                    || snapshot.registry.renderers().iter().any(|renderer| {
                        renderer.name == tool_name && renderer.kind == RendererKind::Tool
                    })
            })
            .or_else(|| {
                (self.endpoints.len() == 1)
                    .then(|| self.endpoints.first())
                    .flatten()
            })?;
        if !endpoint.active() {
            return None;
        }
        let request = serde_json::json!({
            "phase": phase.as_str(),
            "toolName": tool_name,
            "payload": payload,
        });
        match endpoint
            .client
            .request_raw(TOOL_RENDER_HTML_METHOD, request, endpoint.hook_timeout)
            .await
        {
            Ok(frame) => serde_json::from_value::<ToolRenderHtmlWire>(frame.payload)
                .ok()
                .and_then(|wire| wire.html.as_deref().map(sanitize_html)),
            Err(error) => {
                endpoint.report_host_error(&self.aggregate, &error);
                None
            }
        }
    }

    /// Reap every endpoint and advance the aggregate reload generation.
    pub async fn reload(&self) -> u64 {
        let generation = self
            .aggregate
            .reload_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.shutdown_once().await;
        for endpoint in self.endpoints.iter() {
            endpoint.stale.store(true, Ordering::Relaxed);
        }
        generation
    }

    /// Classify and start extension endpoints with an explicit load directory.
    ///
    /// # Errors
    ///
    /// Per-endpoint failures are isolated and surfaced through
    /// [`HostExtensionRunner::load_errors`]; an `Err` is reserved for failures
    /// that prevent the aggregate runner itself from being constructed.
    pub async fn start_with_cwd(
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::start_with_cwd_and_trust(extension_paths, load_cwd, false).await
    }

    /// Classify and start extension endpoints with explicit load trust.
    ///
    /// # Errors
    ///
    /// Per-endpoint failures are isolated and surfaced through
    /// [`HostExtensionRunner::load_errors`]; an `Err` is reserved for failures
    /// that prevent the aggregate runner itself from being constructed.
    pub async fn start_with_cwd_and_trust(
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        project_trusted: bool,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::start_classified(extension_paths, load_cwd.into(), project_trusted).await
    }

    /// Invalidate every endpoint and clear aggregate routes and slots.
    pub fn invalidate(&self) {
        for endpoint in self.endpoints.iter() {
            endpoint.stale.store(true, Ordering::Relaxed);
        }
        self.aggregate.dispose_all_slots();
        let mut routes = self
            .aggregate
            .pending_routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        routes.clear();
    }

    /// Shut down and reap every endpoint exactly once.
    pub async fn shutdown_once(&self) {
        futures::future::join_all(
            self.endpoints
                .iter()
                .map(|endpoint| Self::shutdown_endpoint_once(endpoint, &self.aggregate)),
        )
        .await;
        self.aggregate.dispose_all_slots();
    }

    /// Prepare a replacement without mutating the current runner or provider registry.
    ///
    /// # Errors
    ///
    /// Returns an error if preserved flags cannot be encoded, the replacement host
    /// cannot start, or applying the preserved flags to the replacement fails.
    pub async fn prepare_restart(
        &self,
        preserved_flags: HashMap<String, Value>,
    ) -> Result<PreparedHostRestart, HostStartError> {
        self.prepare_restart_with(preserved_flags, |paths, cwd, project_trusted| async move {
            Self::start_with_cwd_and_trust(paths, cwd, project_trusted).await
        })
        .await
    }

    async fn prepare_restart_with<F, Fut>(
        &self,
        preserved_flags: HashMap<String, Value>,
        start: F,
    ) -> Result<PreparedHostRestart, HostStartError>
    where
        F: FnOnce(Vec<String>, String, bool) -> Fut,
        Fut: Future<Output = Result<Arc<Self>, HostStartError>>,
    {
        self.prepare_restart_with_injected_error(preserved_flags, start, None)
            .await
    }

    async fn prepare_restart_with_injected_error<F, Fut>(
        &self,
        preserved_flags: HashMap<String, Value>,
        start: F,
        injected_flag_error: Option<HostClientError>,
    ) -> Result<PreparedHostRestart, HostStartError>
    where
        F: FnOnce(Vec<String>, String, bool) -> Fut,
        Fut: Future<Output = Result<Arc<Self>, HostStartError>>,
    {
        let preserved_flags = preserved_flags
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
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let replacement = start(
            self.discovery_paths.clone(),
            self.load_cwd.clone(),
            self.project_trusted,
        )
        .await?;
        let mut diagnostics = replacement
            .load_errors()
            .into_iter()
            .map(|(path, message)| ExtensionHostDiagnostic::new(path, message))
            .collect::<Vec<_>>();
        let flag_outcome = match injected_flag_error {
            Some(error) => Err(error),
            None => replacement.apply_flag_values(&preserved_flags).await,
        };
        match flag_outcome {
            Ok(flag_diagnostics) => diagnostics.extend(flag_diagnostics),
            Err(error) => {
                replacement.shutdown_once().await;
                return Err(HostStartError::FlagSync(error.to_string()));
            }
        }
        Ok(PreparedHostRestart {
            runner: replacement,
            diagnostics,
        })
    }

    #[cfg(test)]
    async fn prepare_restart_with_fatal_flag_error<F, Fut>(
        &self,
        preserved_flags: HashMap<String, Value>,
        start: F,
    ) -> Result<PreparedHostRestart, HostStartError>
    where
        F: FnOnce(Vec<String>, String, bool) -> Fut,
        Fut: Future<Output = Result<Arc<Self>, HostStartError>>,
    {
        self.prepare_restart_with_injected_error(
            preserved_flags,
            start,
            Some(HostClientError::Payload(
                "injected fatal flag encoding error".to_owned(),
            )),
        )
        .await
    }

    #[cfg(test)]
    async fn restart_and_rewire_with<F, Fut>(
        &self,
        runtime: &ModelRuntime,
        preserved_flags: HashMap<String, Value>,
        start: F,
    ) -> Result<Arc<Self>, HostStartError>
    where
        F: FnOnce(Vec<String>, String, bool) -> Fut,
        Fut: Future<Output = Result<Arc<Self>, HostStartError>>,
    {
        let prepared = self.prepare_restart_with(preserved_flags, start).await?;
        Ok(self.commit_restart(runtime, prepared).await.runner)
    }

    /// Commit a prepared replacement. No operation after provider removal can fail.
    pub async fn commit_restart(
        &self,
        runtime: &ModelRuntime,
        prepared: PreparedHostRestart,
    ) -> HostRestartResult {
        let PreparedHostRestart {
            runner,
            mut diagnostics,
        } = prepared;
        self.unregister_providers_from(runtime);
        diagnostics.extend(
            runner
                .register_providers_on(runtime)
                .into_iter()
                .filter_map(|(path, outcome)| {
                    outcome
                        .err()
                        .map(|error| ExtensionHostDiagnostic::new(path, error.to_string()))
                }),
        );
        let _ = self.reload().await;
        HostRestartResult {
            runner,
            diagnostics,
        }
    }
}

/// Spawn the unsolicited-event pump. Routes typed host events into the bounded
/// subscribers; on fatal host conditions marks the runner disabled and emits a
/// single non-retryable `extension_error`.
///
/// Subscribe-then-check: a fast-exiting host can broadcast `Eof` (then clear
/// `running`) before this pump subscribes, so the flag is probed after
/// subscribing — state catches an early EOF, the broadcast catches a late one.
fn spawn_event_pump(endpoint: Arc<Endpoint>, aggregate: Arc<AggregateState>) {
    let mut rx = endpoint.client.subscribe();
    tokio::spawn(async move {
        let pump = EventPump {
            endpoint: &endpoint,
            aggregate: &aggregate,
        };
        if !endpoint.client.is_running() {
            pump.fatal("extension_closed", "extension host stream closed")
                .await;
            return;
        }
        while pump.handle(rx.recv().await).await {}
    });
}

/// Typed forwarding view over one endpoint's unsolicited-event stream.
struct EventPump<'a> {
    endpoint: &'a Arc<Endpoint>,
    aggregate: &'a Arc<AggregateState>,
}

impl EventPump<'_> {
    /// Handle one pump event; returns `false` when the pump must stop.
    async fn handle(&self, event: Result<HostEvent, broadcast::error::RecvError>) -> bool {
        match event {
            Ok(HostEvent::UiRequest(request)) => self.forward_ui_request(request).await,
            Ok(HostEvent::Notify(notification)) => {
                self.forward_ui(ExtensionUiEvent::Notify(notification));
            }
            Ok(HostEvent::ThemeSet(set)) => self.forward_ui(ExtensionUiEvent::ThemeSet(set)),
            Ok(HostEvent::UiControl(control)) => {
                self.forward_ui(ExtensionUiEvent::UiControl(control));
            }
            Ok(HostEvent::SessionCommand(command)) => {
                forward_session_bridge(
                    self.endpoint,
                    self.aggregate,
                    SessionBridgeEvent::Command(command),
                )
                .await;
            }
            Ok(HostEvent::SetModelRequest { id, request }) => {
                forward_session_bridge(
                    self.endpoint,
                    self.aggregate,
                    SessionBridgeEvent::SetModel { id, request },
                )
                .await;
            }
            Ok(HostEvent::CompactRequest { id, request }) => {
                forward_session_bridge(
                    self.endpoint,
                    self.aggregate,
                    SessionBridgeEvent::Compact { id, request },
                )
                .await;
            }
            Ok(HostEvent::UiSlot(slot)) => forward_slot(self.endpoint, self.aggregate, &slot),
            Ok(HostEvent::DisposeSlot(dispose)) => {
                forward_dispose(self.endpoint, self.aggregate, &dispose);
            }
            Ok(HostEvent::ToolUpdate(update)) => {
                let _ = self.aggregate.tool_updates_tx.send(update);
            }
            Ok(HostEvent::ProviderEvent(event)) => {
                let _ = self.aggregate.provider_events_tx.send(event);
            }
            Ok(HostEvent::ExtensionError(event)) => {
                let _ = self.aggregate.errors_tx.send(event);
            }
            Ok(HostEvent::Raw(frame)) => {
                self.fatal(
                    "extension_protocol",
                    &format!("unhandled host frame: {} {}", frame.kind, frame.method),
                )
                .await;
                return false;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                self.aggregate.publish_error(
                    "extension_event_lagged",
                    &format!("dropped {skipped} extension host events"),
                    None,
                );
            }
            Ok(HostEvent::Eof) => {
                self.fatal("extension_closed", "extension host stream closed")
                    .await;
                return false;
            }
            Ok(HostEvent::ProtocolError(message)) => {
                self.fatal("extension_protocol", &message).await;
                return false;
            }
            Err(broadcast::error::RecvError::Closed) => return false,
        }
        true
    }

    /// Disable the endpoint, publish one terminal error, and reap exactly once.
    async fn fatal(&self, code: &str, message: &str) {
        self.endpoint.disabled.store(true, Ordering::Relaxed);
        self.aggregate.publish_error(code, message, None);
        HostExtensionRunner::shutdown_endpoint_once(self.endpoint, self.aggregate).await;
    }

    /// Forward extension UI activity when the endpoint is still active.
    fn forward_ui(&self, event: ExtensionUiEvent) {
        if self.endpoint.active() {
            let _ = self.aggregate.ui_tx.send(event);
        }
    }

    /// Claim-or-default a host UI request and retag it with an aggregate id.
    async fn forward_ui_request(&self, request: HostUiRequest) {
        if !self.endpoint.active() || !self.aggregate.ui_requests_claimed.load(Ordering::Acquire) {
            let _ = self
                .endpoint
                .client
                .respond_ui(default_ui_response(&request))
                .await;
            return;
        }
        let local_request = request.clone();
        let local_id = host_ui_request_id(&request);
        let aggregate_id = self
            .aggregate
            .insert_route(self.endpoint, local_id, PendingKind::Ui);
        let request = retag_ui_request(request, aggregate_id);
        if self.aggregate.ui_requests_tx.send(request).await.is_err() {
            let _ = self.aggregate.take_route(aggregate_id, PendingKind::Ui);
            let _ = self
                .endpoint
                .client
                .respond_ui(default_ui_response(&local_request))
                .await;
        }
    }
}

async fn forward_session_bridge(
    endpoint: &Arc<Endpoint>,
    aggregate: &Arc<AggregateState>,
    event: SessionBridgeEvent,
) {
    let claimed = endpoint.active() && aggregate.session_bridge_claimed.load(Ordering::Acquire);
    let (event, route) = if claimed {
        match event {
            SessionBridgeEvent::SetModel { id, request } => {
                let aggregate_id = aggregate.insert_route(endpoint, id, PendingKind::SetModel);
                (
                    SessionBridgeEvent::SetModel {
                        id: aggregate_id,
                        request,
                    },
                    Some((aggregate_id, PendingKind::SetModel)),
                )
            }
            SessionBridgeEvent::Compact { id, request } => {
                let aggregate_id = aggregate.insert_route(endpoint, id, PendingKind::Compact);
                (
                    SessionBridgeEvent::Compact {
                        id: aggregate_id,
                        request,
                    },
                    Some((aggregate_id, PendingKind::Compact)),
                )
            }
            SessionBridgeEvent::Command(command) => (SessionBridgeEvent::Command(command), None),
        }
    } else {
        (event, None)
    };
    let undelivered = if claimed {
        aggregate
            .session_bridge_tx
            .try_send(event)
            .err()
            .map(mpsc::error::TrySendError::into_inner)
    } else {
        Some(event)
    };
    match undelivered {
        None | Some(SessionBridgeEvent::Command(_)) => {}
        Some(SessionBridgeEvent::SetModel { id, .. }) => {
            let local_id = route
                .and_then(|(aggregate_id, kind)| aggregate.take_route(aggregate_id, kind))
                .map_or(id, |route| route.local_id);
            let _ = endpoint.client.respond_set_model(local_id, false).await;
        }
        Some(SessionBridgeEvent::Compact { id, .. }) => {
            let local_id = route
                .and_then(|(aggregate_id, kind)| aggregate.take_route(aggregate_id, kind))
                .map_or(id, |route| route.local_id);
            let _ = endpoint
                .client
                .respond_compact(local_id, Err("no active session".to_owned()))
                .await;
        }
    }
}

fn host_ui_request_id(request: &HostUiRequest) -> FrameId {
    match request {
        HostUiRequest::Select { id, .. }
        | HostUiRequest::Confirm { id, .. }
        | HostUiRequest::Input { id, .. }
        | HostUiRequest::Editor { id, .. } => *id,
    }
}

fn retag_ui_request(mut request: HostUiRequest, id: FrameId) -> HostUiRequest {
    match &mut request {
        HostUiRequest::Select { id: current, .. }
        | HostUiRequest::Confirm { id: current, .. }
        | HostUiRequest::Input { id: current, .. }
        | HostUiRequest::Editor { id: current, .. } => *current = id,
    }
    request
}

fn host_ui_response_id(response: &HostUiResponse) -> FrameId {
    match response {
        HostUiResponse::Select { id, .. }
        | HostUiResponse::Confirm { id, .. }
        | HostUiResponse::Input { id, .. }
        | HostUiResponse::Editor { id, .. } => *id,
    }
}

fn retag_ui_response(mut response: HostUiResponse, id: FrameId) -> HostUiResponse {
    match &mut response {
        HostUiResponse::Select { id: current, .. }
        | HostUiResponse::Confirm { id: current, .. }
        | HostUiResponse::Input { id: current, .. }
        | HostUiResponse::Editor { id: current, .. } => *current = id,
    }
    response
}

fn default_ui_response(request: &HostUiRequest) -> HostUiResponse {
    match request {
        HostUiRequest::Select { id, .. } => HostUiResponse::Select {
            id: *id,
            value: None,
        },
        HostUiRequest::Confirm { id, .. } => HostUiResponse::Confirm {
            id: *id,
            confirmed: false,
        },
        HostUiRequest::Input { id, .. } => HostUiResponse::Input {
            id: *id,
            value: None,
        },
        HostUiRequest::Editor { id, .. } => HostUiResponse::Editor {
            id: *id,
            value: None,
        },
    }
}

fn forward_slot(endpoint: &Arc<Endpoint>, aggregate: &Arc<AggregateState>, slot: &UiSlot) {
    let sanitized = sanitize_slot(slot);
    if sanitized.had_rejections {
        aggregate.publish_error(
            "extension_sanitized",
            "extension uiSlot contained rejected control sequences or oversized fields",
            None,
        );
    }
    aggregate.slot_send(endpoint, sanitized);
}

fn forward_dispose(
    endpoint: &Arc<Endpoint>,
    aggregate: &Arc<AggregateState>,
    dispose: &DisposeSlot,
) {
    aggregate.slot_dispose(endpoint, &dispose.key);
}

/// Strip `<script>` / `<style>` blocks and escape ampersand / angle brackets
/// so an extension-supplied HTML fragment cannot inject active content into an
/// exported session document. Attribute-level injection is out of scope for
/// the export path (fragments are written into a known template).
fn sanitize_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut kept = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while cursor < html.len() {
        let script_rel = lower[cursor..].find("<script");
        let style_rel = lower[cursor..].find("<style");
        // Earliest dangerous open tag and its matching close marker.
        let (start, open_len, close_marker): (usize, usize, &str) = match (script_rel, style_rel) {
            (None, None) => {
                kept.push_str(&html[cursor..]);
                break;
            }
            (Some(rel), None) => (cursor + rel, "<script".len(), "</script>"),
            (None, Some(rel)) => (cursor + rel, "<style".len(), "</style>"),
            (Some(script), Some(style)) => {
                if script <= style {
                    (cursor + script, "<script".len(), "</script>")
                } else {
                    (cursor + style, "<style".len(), "</style>")
                }
            }
        };
        // Preserve everything before the dangerous block.
        kept.push_str(&html[cursor..start]);
        let search_from = start + open_len;
        match lower[search_from..].find(close_marker) {
            Some(rel) => {
                cursor = search_from + rel + close_marker.len();
            }
            None => {
                // Unterminated dangerous block: drop the remainder entirely.
                cursor = html.len();
            }
        }
    }
    // Escape the surviving markup so no raw tags can become active in the
    // exported document.
    kept.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Assistant metadata without the growing `content` array.
fn compact_assistant_meta(message: &AssistantMessage) -> Value {
    serde_json::to_value(message.metadata_view()).unwrap_or_else(|_| Value::Object(Map::new()))
}

/// The single content block addressed by a streaming event, if any.
fn compact_assistant_block(message: &AssistantMessage, content_index: u64) -> Value {
    usize::try_from(content_index)
        .ok()
        .and_then(|index| message.content.get(index))
        .and_then(|content| serde_json::to_value(content).ok())
        .unwrap_or(Value::Null)
}

fn compact_message_update_event(event: &AssistantMessageEvent) -> Value {
    use AssistantMessageEvent as Ev;
    // (type name, partial, contentIndex, delta text, include block)
    let (kind, partial, content_index, delta, with_block) = match event {
        Ev::Start { partial } => ("start", partial, None, None, false),
        Ev::TextStart {
            content_index,
            partial,
        } => ("text_start", partial, Some(*content_index), None, true),
        Ev::TextDelta {
            content_index,
            delta,
            partial,
        } => (
            "text_delta",
            partial,
            Some(*content_index),
            Some(delta),
            false,
        ),
        Ev::TextEnd {
            content_index,
            partial,
            ..
        } => ("text_end", partial, Some(*content_index), None, true),
        Ev::ThinkingStart {
            content_index,
            partial,
        } => ("thinking_start", partial, Some(*content_index), None, true),
        Ev::ThinkingDelta {
            content_index,
            delta,
            partial,
        } => (
            "thinking_delta",
            partial,
            Some(*content_index),
            Some(delta),
            false,
        ),
        Ev::ThinkingEnd {
            content_index,
            partial,
            ..
        } => ("thinking_end", partial, Some(*content_index), None, true),
        Ev::ToolCallStart {
            content_index,
            partial,
        } => ("toolcall_start", partial, Some(*content_index), None, true),
        Ev::ToolCallDelta {
            content_index,
            delta,
            partial,
        } => (
            "toolcall_delta",
            partial,
            Some(*content_index),
            Some(delta),
            false,
        ),
        Ev::ToolCallEnd {
            content_index,
            partial,
            ..
        } => ("toolcall_end", partial, Some(*content_index), None, true),
        Ev::Done { reason, message } => {
            return serde_json::json!({
                "type": "done",
                "reason": reason,
                "final": message,
            });
        }
        Ev::Error { reason, error } => {
            return serde_json::json!({
                "type": "error",
                "reason": reason,
                "final": error,
            });
        }
    };

    let mut object = Map::new();
    object.insert("type".to_owned(), Value::String(kind.to_owned()));
    object.insert("meta".to_owned(), compact_assistant_meta(partial));
    if let Some(content_index) = content_index {
        object.insert("contentIndex".to_owned(), Value::from(content_index));
        if with_block {
            object.insert(
                "block".to_owned(),
                compact_assistant_block(partial, content_index),
            );
        }
    }
    if let Some(delta) = delta {
        object.insert("delta".to_owned(), Value::String(delta.clone()));
    }
    Value::Object(object)
}

// ---------------------------------------------------------------------------
// ExtensionRunner trait impl
// ---------------------------------------------------------------------------

impl ExtensionRunner for HostExtensionRunner {
    fn has_handlers(&self, event: &str) -> bool {
        self.endpoints
            .iter()
            .any(|endpoint| endpoint.has_handlers(event))
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<
        '_,
        Result<Option<CancelResult>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        Box::pin(async move {
            let method = match event.type_name() {
                "compaction_start" => "session_before_compact",
                "compaction_end" => "session_compact",
                "thinking_level_changed" => "thinking_level_select",
                name => name,
            };
            let payload =
                serde_json::to_value(&event).unwrap_or_else(|_| Value::Object(Map::new()));
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers(method) {
                    continue;
                }
                match endpoint.hook_request(method, payload.clone()).await {
                    Ok(frame) => {
                        let result = serde_json::from_value::<Option<CancelWire>>(frame.payload)
                            .ok()
                            .flatten();
                        if let Some(wire) = result
                            && wire.cancel
                        {
                            return Ok(Some(CancelResult {
                                cancel: true,
                                reason: wire.reason,
                            }));
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(None)
        })
    }

    fn emit_message_update_delta<'a>(
        &'a self,
        event: &'a AssistantMessageEvent,
    ) -> BoxFuture<
        'a,
        Result<Option<CancelResult>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let payload = serde_json::json!({
            "type": MESSAGE_UPDATE_DELTA_METHOD,
            "event": compact_message_update_event(event),
        });
        Box::pin(async move {
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("message_update") {
                    continue;
                }
                match endpoint
                    .hook_request(MESSAGE_UPDATE_DELTA_METHOD, payload.clone())
                    .await
                {
                    Ok(frame) => {
                        if let Some(wire) =
                            serde_json::from_value::<Option<CancelWire>>(frame.payload)
                                .ok()
                                .flatten()
                            && wire.cancel
                        {
                            return Ok(Some(CancelResult {
                                cancel: true,
                                reason: wire.reason,
                            }));
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(None)
        })
    }

    fn emit_message_end(
        &self,
        message: AgentMessage,
    ) -> BoxFuture<
        '_,
        Result<Option<AgentMessage>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        Box::pin(async move {
            let mut current = message;
            let mut changed = false;
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("message_end") {
                    continue;
                }
                let payload = serde_json::to_value(&current).unwrap_or(Value::Null);
                match endpoint.hook_request("message_end", payload).await {
                    Ok(frame) => {
                        let replacement = serde_json::from_value::<MessageEndWire>(frame.payload)
                            .ok()
                            .and_then(|wire| wire.message);
                        if let Some(replacement) = replacement {
                            if replacement.role() == current.role() {
                                current = replacement;
                                changed = true;
                            } else {
                                aggregate.publish_error(
                                    "extension_message_end",
                                    "message_end handler returned a message with a different role",
                                    None,
                                );
                            }
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(changed.then_some(current))
        })
    }

    fn emit_tool_call(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: Map<String, Value>,
    ) -> BoxFuture<
        '_,
        Result<
            Option<BeforeToolCallResult>,
            super::agent_session::extension_runner::ExtensionRunnerError,
        >,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let tool_name = tool_name.to_owned();
        let tool_call_id = tool_call_id.to_owned();
        Box::pin(async move {
            let mut current = input;
            let mut changed = false;
            let mut responded = false;
            let mut reason = None;
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("tool_call") {
                    continue;
                }
                let payload = serde_json::json!({
                    "toolName": tool_name,
                    "toolCallId": tool_call_id,
                    "input": current,
                });
                match endpoint.hook_request("tool_call", payload).await {
                    Ok(frame) => {
                        let wire =
                            serde_json::from_value::<Option<BeforeToolCallWire>>(frame.payload)
                                .ok()
                                .flatten();
                        if let Some(wire) = wire {
                            responded = true;
                            if let Some(input) = wire.input {
                                current = input;
                                changed = true;
                            }
                            // The last responder owns the reason outright: a
                            // blocking endpoint that omits it must not inherit
                            // an earlier non-blocking endpoint's reason.
                            reason = wire.reason;
                            if wire.block {
                                return Ok(Some(BeforeToolCallResult {
                                    block: true,
                                    reason,
                                    arguments: changed.then_some(current),
                                }));
                            }
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(responded.then_some(BeforeToolCallResult {
                block: false,
                reason,
                arguments: changed.then_some(current),
            }))
        })
    }

    fn emit_tool_result(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        input: Map<String, Value>,
        content: Vec<ToolResultContent>,
        details: Value,
        is_error: bool,
    ) -> BoxFuture<
        '_,
        Result<
            Option<AfterToolCallResult>,
            super::agent_session::extension_runner::ExtensionRunnerError,
        >,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let tool_name = tool_name.to_owned();
        let tool_call_id = tool_call_id.to_owned();
        Box::pin(async move {
            let mut current_content = content;
            let mut current_details = details;
            let mut current_is_error = is_error;
            let mut content_changed = false;
            let mut details_changed = false;
            let mut error_changed = false;
            let mut responded = false;
            let mut terminate = None;
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("tool_result") {
                    continue;
                }
                let payload = serde_json::json!({
                    "toolName": tool_name,
                    "toolCallId": tool_call_id,
                    "input": input,
                    "content": current_content,
                    "details": current_details,
                    "isError": current_is_error,
                });
                match endpoint.hook_request("tool_result", payload).await {
                    Ok(frame) => {
                        let wire =
                            serde_json::from_value::<Option<AfterToolCallWire>>(frame.payload)
                                .ok()
                                .flatten();
                        if let Some(wire) = wire {
                            responded = true;
                            if let Some(content) = wire.content {
                                current_content = content;
                                content_changed = true;
                            }
                            if let Some(details) = wire.details {
                                current_details = details;
                                details_changed = true;
                            }
                            if let Some(is_error) = wire.is_error {
                                current_is_error = is_error;
                                error_changed = true;
                            }
                            if wire.terminate.is_some() {
                                terminate = wire.terminate;
                            }
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(responded.then_some(AfterToolCallResult {
                content: content_changed.then_some(current_content),
                details: details_changed.then_some(current_details),
                is_error: error_changed.then_some(current_is_error),
                terminate,
            }))
        })
    }

    fn emit_input(
        &self,
        text: &str,
        images: Option<Value>,
        source: &str,
        streaming_behavior: Option<&str>,
    ) -> BoxFuture<
        '_,
        Result<InputTransformResult, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let mut current_text = text.to_owned();
        let mut current_images = images;
        let source = source.to_owned();
        let streaming_behavior = streaming_behavior.map(str::to_owned);
        Box::pin(async move {
            let mut text_changed = false;
            let mut images_changed = false;
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("input") {
                    continue;
                }
                let payload = serde_json::json!({
                    "text": current_text,
                    "images": current_images,
                    "source": source,
                    "streamingBehavior": streaming_behavior,
                });
                match endpoint.hook_request("input", payload).await {
                    Ok(frame) => {
                        match serde_json::from_value::<InputTransformWire>(frame.payload) {
                            Ok(InputTransformWire::Handled) => {
                                return Ok(InputTransformResult {
                                    handled: true,
                                    text: text_changed.then_some(current_text),
                                    images: images_changed.then_some(current_images).flatten(),
                                });
                            }
                            Ok(InputTransformWire::Transform { text, images }) => {
                                current_text = text;
                                text_changed = true;
                                if let Some(images) = images {
                                    current_images = Some(images);
                                    images_changed = true;
                                }
                            }
                            Ok(InputTransformWire::Continue) | Err(_) => {}
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(InputTransformResult {
                handled: false,
                text: text_changed.then_some(current_text),
                images: images_changed.then_some(current_images).flatten(),
            })
        })
    }

    fn emit_before_agent_start(
        &self,
        prompt: &str,
        images: Option<Value>,
        system_prompt: Option<String>,
    ) -> BoxFuture<
        '_,
        Result<
            Option<BeforeAgentStartResult>,
            super::agent_session::extension_runner::ExtensionRunnerError,
        >,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let prompt = prompt.to_owned();
        Box::pin(async move {
            let mut messages = Vec::new();
            let mut system_prompt = system_prompt;
            let mut responded = false;
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("before_agent_start") {
                    continue;
                }
                let payload = serde_json::json!({
                    "prompt": prompt,
                    "images": images,
                    "messages": messages,
                    "systemPrompt": system_prompt,
                });
                match endpoint.hook_request("before_agent_start", payload).await {
                    Ok(frame) => {
                        let wire =
                            serde_json::from_value::<Option<BeforeAgentStartWire>>(frame.payload)
                                .ok()
                                .flatten();
                        if let Some(wire) = wire {
                            responded = true;
                            // Injections accumulate across endpoints; each
                            // endpoint observes the running list in the next
                            // request payload.
                            messages.extend(wire.messages);
                            if wire.system_prompt.is_some() {
                                system_prompt = wire.system_prompt;
                            }
                        }
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(responded.then_some(BeforeAgentStartResult {
                messages,
                system_prompt,
            }))
        })
    }

    fn emit_resources_discover(
        &self,
        cwd: &str,
        reason: &str,
    ) -> BoxFuture<
        '_,
        Result<
            ResourceExtensionPaths,
            super::agent_session::extension_runner::ExtensionRunnerError,
        >,
    > {
        let endpoints = Arc::clone(&self.endpoints);
        let aggregate = Arc::clone(&self.aggregate);
        let cwd = cwd.to_owned();
        let reason = reason.to_owned();
        Box::pin(async move {
            let mut result = ResourceExtensionPaths::default();
            for endpoint in endpoints.iter() {
                if !endpoint.has_handlers("resources_discover") {
                    continue;
                }
                let payload = serde_json::json!({ "cwd": cwd, "reason": reason });
                match endpoint.hook_request("resources_discover", payload).await {
                    Ok(frame) => {
                        let wire = serde_json::from_value::<ResourcesDiscoverWire>(frame.payload)
                            .unwrap_or_default();
                        let discovered = |paths: Option<Vec<ResourcePathWire>>| {
                            paths
                                .unwrap_or_default()
                                .into_iter()
                                .map(|entry| {
                                    ExtensionResourcePath::discovered(
                                        entry.path,
                                        &entry.extension_path,
                                    )
                                })
                                .collect::<Vec<_>>()
                        };
                        result.skill_paths.extend(discovered(wire.skills));
                        result.prompt_paths.extend(discovered(wire.prompts));
                        result.theme_paths.extend(discovered(wire.themes));
                    }
                    Err(error) => endpoint.report_host_error(&aggregate, &error),
                }
            }
            Ok(result)
        })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        self.registry()
            .commands()
            .iter()
            .map(|command| command.name.clone())
            .collect()
    }

    fn execute_command(
        &self,
        name: &str,
        args: &str,
    ) -> BoxFuture<'_, Result<bool, super::agent_session::extension_runner::ExtensionRunnerError>>
    {
        let endpoint = self
            .first_owner(|snapshot| {
                snapshot
                    .registry
                    .commands()
                    .iter()
                    .any(|command| command.name == name)
            })
            .cloned();
        let aggregate = Arc::clone(&self.aggregate);
        let name = name.to_owned();
        let args = args.to_owned();
        Box::pin(async move {
            let Some(endpoint) = endpoint else {
                return Ok(false);
            };
            if !endpoint.active() {
                return Ok(false);
            }
            let payload = serde_json::json!({ "name": name, "args": args });
            match endpoint
                .client
                .request_raw(COMMAND_EXECUTE_METHOD, payload, endpoint.hook_timeout)
                .await
            {
                Ok(frame) => Ok(serde_json::from_value::<CommandExecuteWire>(frame.payload)
                    .is_ok_and(|wire| wire.ok)),
                Err(error) => {
                    let not_running = matches!(error, HostClientError::NotRunning);
                    endpoint.report_host_error(&aggregate, &error);
                    Ok(!not_running)
                }
            }
        })
    }

    fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>> {
        let mut tools = HashMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(snapshot) = endpoint.snapshot.read() {
                for (name, tool) in &snapshot.tools {
                    tools
                        .entry(name.clone())
                        .or_insert_with(|| Arc::clone(tool));
                }
            }
        }
        tools
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        let mut values = HashMap::new();
        for endpoint in self.endpoints.iter() {
            if let Ok(flags) = endpoint.flag_values.read() {
                for (name, value) in flags.iter() {
                    values.entry(name.clone()).or_insert_with(|| value.clone());
                }
            }
        }
        values
    }

    fn invalidate(&self) {
        HostExtensionRunner::invalidate(self);
    }

    fn emit_error(&self, message: String) {
        self.aggregate
            .publish_error("extension_error", &message, None);
    }
}

impl HostExtensionRunner {
    async fn shutdown_endpoint_once(endpoint: &Arc<Endpoint>, aggregate: &Arc<AggregateState>) {
        let _guard = endpoint.shutdown_lock.lock().await;
        if endpoint.shutdown_done.load(Ordering::Relaxed) {
            return;
        }
        endpoint.disabled.store(true, Ordering::Relaxed);
        aggregate.remove_endpoint_routes(endpoint.id);
        aggregate.dispose_endpoint_slots(endpoint.id);
        let _ = endpoint.client.shutdown().await;
        endpoint.shutdown_done.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
