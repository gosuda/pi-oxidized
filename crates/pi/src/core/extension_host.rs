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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_agent::{AfterToolCallResult, AgentMessage, AgentTool, BeforeToolCallResult};
use pi_ai::ToolResultContent;
use pi_ext::adapters::{
    self, CommandRegistration, ExtensionAgentTool, ExtensionProvider, FlagRegistration,
    ProviderRegistration, Registry, RendererRegistration, ShortcutRegistration, ToolRegistration,
};
use pi_ext::client::{HostClient, HostClientError, HostEvent};
use pi_ext::host::{self, HostError, HostSpec};
use pi_ext::protocol::{self, DisposeSlot, ExtensionErrorEvent, ProviderEvent, ToolUpdate, UiSlot};
use pi_ext::sanitize::{SanitizedSlot, sanitize_slot};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, watch};

use super::agent_session::events::AgentSessionEvent;
use super::agent_session::extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, InputTransformResult,
};
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

// ---------------------------------------------------------------------------
// Registration snapshot wire types (host → Rust load response)
// ---------------------------------------------------------------------------

/// Wire form of [`ToolRegistration`] received from the host load response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolWire {
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    parameters: Value,
    #[serde(default)]
    execution_mode: Option<pi_agent::ToolExecutionMode>,
}

/// Wire form of [`CommandRegistration`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Wire form of [`ShortcutRegistration`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutWire {
    key: String,
    #[serde(default)]
    description: Option<String>,
}

/// Wire form of [`FlagRegistration`] with its current resolved value.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlagWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    default: Option<String>,
    /// Currently resolved value (from CLI / settings), if any.
    #[serde(default)]
    value: Option<Value>,
}

/// Wire form of [`RendererRegistration`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RendererWire {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    name: String,
}

/// Wire form of a host-registered custom provider.
///
/// Matches the host's `buildRegistrySnapshot` camelCase payload: full
/// `ProviderConfig` fields plus a boolean `streamSimple` flag (the function
/// itself never crosses the wire; the host keeps it and Rust proxies via
/// [`ExtensionProvider`] when the flag is true).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderWire {
    name: String,
    /// Display name (`config.name` in TypeScript); wire key is `displayName`.
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, String>>,
    #[serde(default)]
    auth_header: Option<bool>,
    #[serde(default)]
    models: Option<Vec<ProviderModelDefinition>>,
    /// `true` when the host holds a live `streamSimple` function for this provider.
    #[serde(default)]
    stream_simple: bool,
    /// Optional extension path used in diagnostic messages when present.
    #[serde(default)]
    extension_path: Option<String>,
}

impl ProviderWire {
    fn to_config_input(&self) -> ProviderConfigInput {
        ProviderConfigInput {
            name: self.display_name.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api: self.api.clone(),
            headers: self.headers.clone(),
            auth_header: self.auth_header,
            models: self.models.clone(),
            model_overrides: None,
            oauth: None,
        }
    }
}

/// Full host registration snapshot returned by `extensions.load`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySnapshotWire {
    /// Registered extension tools (host already applied first-wins).
    #[serde(default)]
    tools: Vec<ToolWire>,
    /// Registered slash commands.
    #[serde(default)]
    commands: Vec<CommandWire>,
    /// Registered keyboard shortcuts.
    #[serde(default)]
    shortcuts: Vec<ShortcutWire>,
    /// Registered CLI flags with current values.
    #[serde(default)]
    flags: Vec<FlagWire>,
    /// Registered renderers (message / tool / widget).
    #[serde(default)]
    renderers: Vec<RendererWire>,
    /// Registered custom providers.
    #[serde(default)]
    providers: Vec<ProviderWire>,
    /// Lifecycle event types with at least one handler installed.
    #[serde(default)]
    handlers: Vec<String>,
    /// Number of extensions successfully loaded (host diagnostic field).
    #[serde(default)]
    extensions: Option<u64>,
    /// Per-path load errors (sibling isolation).
    #[serde(default)]
    errors: Vec<LoadErrorWire>,
}

/// Per-extension load error from the host snapshot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadErrorWire {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Built registry snapshot: pi-ext [`Registry`] plus ready tool/provider
/// adapters and the handler-presence set.
#[derive(Default)]
struct RegistrySnapshot {
    /// Aggregate registrations (first-wins dedup applied on build).
    registry: Registry,
    /// Extension tool adapters keyed by tool name.
    tools: HashMap<String, Arc<dyn AgentTool>>,
    /// Lifecycle event types with at least one handler.
    handlers: HashSet<String>,
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

fn build_snapshot(wire: RegistrySnapshotWire, client: &Arc<HostClient>) -> RegistrySnapshot {
    let mut snapshot = RegistrySnapshot::default();

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
        let _ = snapshot.registry.register_command(CommandRegistration {
            name: command.name,
            description: command.description,
            source: command.source,
        });
    }

    for shortcut in wire.shortcuts {
        let _ = snapshot.registry.register_shortcut(ShortcutRegistration {
            key: shortcut.key,
            description: shortcut.description,
        });
    }

    for flag in wire.flags {
        if snapshot.registry.register_flag(FlagRegistration {
            name: flag.name.clone(),
            description: flag.description,
            kind: match flag.kind.as_deref() {
                Some("boolean") => adapters::FlagKind::Boolean,
                _ => adapters::FlagKind::String,
            },
            default: flag.default.clone(),
        }) {
            // First-wins: prefer the host-resolved value, fall back to default.
            let value = flag
                .value
                .clone()
                .or_else(|| flag.default.map(Value::String))
                .unwrap_or_else(|| Value::String(String::new()));
            snapshot.flag_values.insert(flag.name, value);
        }
    }

    for renderer in wire.renderers {
        let _ = snapshot.registry.register_renderer(RendererRegistration {
            kind: match renderer.kind.as_deref() {
                Some("tool") => adapters::RendererKind::Tool,
                Some("widget") => adapters::RendererKind::Widget,
                _ => adapters::RendererKind::Message,
            },
            name: renderer.name,
        });
    }

    for provider in wire.providers {
        let name = provider.name.clone();
        let stream_simple = provider.stream_simple;
        let extension_path = provider.extension_path.clone();
        let config = provider.to_config_input();
        if snapshot.registry.register_provider(ProviderRegistration {
            name: name.clone(),
            base_url: config.base_url.clone(),
            api: config.api.clone(),
        }) {
            snapshot.provider_configs.insert(name.clone(), config);
            if stream_simple {
                snapshot.stream_provider_ids.insert(name.clone());
            }
            if let Some(path) = extension_path {
                snapshot.provider_extension_paths.insert(name, path);
            }
        }
    }

    for err in wire.errors {
        let path = err.path.unwrap_or_else(|| "<unknown>".to_owned());
        let message = err
            .error
            .or(err.message)
            .unwrap_or_else(|| "extension load failed".to_owned());
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

struct Inner {
    client: Arc<HostClient>,
    snapshot: RwLock<RegistrySnapshot>,
    flag_values: RwLock<HashMap<String, Value>>,
    slots: RwLock<HashMap<String, SlotWatch>>,
    tool_updates_tx: broadcast::Sender<ToolUpdate>,
    provider_events_tx: broadcast::Sender<ProviderEvent>,
    errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    /// Paths passed to `extensions.load` (restart reuses them).
    extension_paths: Vec<String>,
    /// Cwd passed to `extensions.load`.
    load_cwd: String,
    /// Monotonic reload generation; bumps invalidate every active slot.
    reload_generation: AtomicU64,
    /// Host transport is gone (EOF / crash / protocol error). All hooks and
    /// handler-presence queries short-circuit to no-ops once set.
    disabled: AtomicBool,
    /// Runner invalidated after session replacement (`/reload` / runtime swap).
    stale: AtomicBool,
    /// `shutdown` has completed at least once.
    shutdown_done: AtomicBool,
    /// Per-hook control-RPC deadline (`HOOK_TIMEOUT` in production; shorter in
    /// tests to exercise the timeout path quickly).
    hook_timeout: Duration,
}

impl Inner {
    fn new(
        client: Arc<HostClient>,
        snapshot: RegistrySnapshot,
        extension_paths: Vec<String>,
        load_cwd: String,
        hook_timeout: Duration,
    ) -> Self {
        let (tool_updates_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (provider_events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (errors_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let flag_values = snapshot.flag_values.clone();
        Self {
            client,
            snapshot: RwLock::new(snapshot),
            flag_values: RwLock::new(flag_values),
            slots: RwLock::new(HashMap::new()),
            tool_updates_tx,
            provider_events_tx,
            errors_tx,
            extension_paths,
            load_cwd,
            reload_generation: AtomicU64::new(1),
            disabled: AtomicBool::new(false),
            stale: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
            hook_timeout,
        }
    }

    fn has_handlers(&self, event: &str) -> bool {
        if self.disabled.load(Ordering::Relaxed) || self.stale.load(Ordering::Relaxed) {
            return false;
        }
        self.snapshot
            .read()
            .is_ok_and(|guard| guard.handlers.contains(event))
    }

    fn active(&self) -> bool {
        !self.disabled.load(Ordering::Relaxed) && !self.stale.load(Ordering::Relaxed)
    }

    /// Whether a slash command named `name` is registered (handler-presence
    /// and disabled/stale gates apply).
    fn has_command(&self, name: &str) -> bool {
        if !self.active() {
            return false;
        }
        self.snapshot.read().is_ok_and(|guard| {
            guard
                .registry
                .commands()
                .iter()
                .any(|command| command.name == name)
        })
    }

    /// Send one hook request. Returns the validated response frame, or an
    /// error when the host is gone / timed out / returned an error frame.
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

    /// Map a transport failure to a single non-retryable `extension_error`,
    /// publish it to subscribers, and flip the disabled flag on fatal
    /// conditions. Never aborts the caller.
    fn report_host_error(&self, err: &HostClientError) {
        let fatal = matches!(
            err,
            HostClientError::Closed { .. }
                | HostClientError::Protocol { .. }
                | HostClientError::NotRunning
        );
        if fatal {
            self.disabled.store(true, Ordering::Relaxed);
        }
        let event = ExtensionErrorEvent {
            code: error_code(err).to_owned(),
            message: err.to_string(),
            retryable: false,
            data: None,
        };
        let _ = self.errors_tx.send(event);
    }

    fn publish_error(&self, code: &str, message: &str, data: Option<Value>) {
        let event = ExtensionErrorEvent {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable: false,
            data,
        };
        let _ = self.errors_tx.send(event);
    }

    fn slot_send(&self, slot: SanitizedSlot) {
        if let Ok(mut slots) = self.slots.write() {
            let key = slot.key.clone();
            let sender = slots.entry(key).or_insert_with(|| watch::channel(None).0);
            let _ = sender.send(Some(slot));
        }
    }

    fn slot_dispose(&self, key: &str) {
        if let Ok(mut slots) = self.slots.write() {
            if let Some(sender) = slots.get(key) {
                let _ = sender.send(None);
            }
            slots.remove(key);
        }
    }

    fn dispose_all_slots(&self) {
        if let Ok(mut slots) = self.slots.write() {
            for (_key, sender) in slots.drain() {
                let _ = sender.send(None);
            }
        }
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
    inner: Arc<Inner>,
}

impl HostExtensionRunner {
    /// Resolve, spawn, handshake, and load the host, returning a ready runner.
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError::Resolve`] when no host executable is
    /// available, [`HostStartError::Spawn`] when the process cannot start,
    /// [`HostStartError::Handshake`] on version mismatch, or
    /// [`HostStartError::Load`] when the registration snapshot is unreadable.
    pub async fn start(extension_paths: Vec<String>) -> Result<Arc<Self>, HostStartError> {
        let spec = host::resolve_host()?;
        Self::spawn_from(&spec, extension_paths).await
    }

    /// Spawn from an explicit [`HostSpec`], then bind.
    ///
    /// # Errors
    ///
    /// See [`HostExtensionRunner::start`].
    pub async fn spawn_from(
        spec: &HostSpec,
        extension_paths: Vec<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        let client =
            Arc::new(HostClient::spawn(spec).map_err(|e| HostStartError::Spawn(e.to_string()))?);
        let startup = Self::connect(Arc::clone(&client), extension_paths).await;
        Self::finish_startup(&client, startup).await
    }

    /// Bind a runner to a pre-built client: handshake, load, spawn the event
    /// pump. Used by [`start`](Self::start), the reload restart path, and the
    /// fake-host test harness.
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError::Handshake`] or [`HostStartError::Load`].
    pub async fn connect(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        Self::connect_with_timeout(client, extension_paths, HOOK_TIMEOUT).await
    }

    /// Bind a runner with a custom hook timeout (test harness; production uses
    /// [`connect`](Self::connect) which applies [`HOOK_TIMEOUT`]).
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError::Handshake`] or [`HostStartError::Load`].
    pub async fn connect_with_timeout(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        client.handshake().await?;
        let load_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let snapshot = Self::load(&client, &extension_paths, &load_cwd).await?;
        let inner = Arc::new(Inner::new(
            Arc::clone(&client),
            snapshot,
            extension_paths,
            load_cwd,
            hook_timeout,
        ));
        let runner = Arc::new(Self {
            inner: Arc::clone(&inner),
        });
        spawn_event_pump(inner);
        Ok(runner)
    }

    /// Bind a runner with an explicit load cwd (services factory / tests).
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError::Handshake`] or [`HostStartError::Load`].
    pub async fn connect_with_cwd(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        client.handshake().await?;
        let load_cwd = load_cwd.into();
        let snapshot = Self::load(&client, &extension_paths, &load_cwd).await?;
        let inner = Arc::new(Inner::new(
            Arc::clone(&client),
            snapshot,
            extension_paths,
            load_cwd,
            hook_timeout,
        ));
        let runner = Arc::new(Self {
            inner: Arc::clone(&inner),
        });
        spawn_event_pump(inner);
        Ok(runner)
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
    ) -> Result<RegistrySnapshot, HostStartError> {
        let payload = serde_json::json!({
            "extensionPaths": extension_paths,
            "cwd": cwd,
        });
        let frame = client
            .request_raw(LOAD_METHOD, payload, START_TIMEOUT)
            .await?;
        let wire: RegistrySnapshotWire = serde_json::from_value(frame.payload)
            .map_err(|e| HostStartError::Load(e.to_string()))?;
        Ok(build_snapshot(wire, client))
    }

    /// Borrowed host client (for provider registration by the model runtime).
    #[must_use]
    pub fn client(&self) -> &Arc<HostClient> {
        &self.inner.client
    }

    /// Extension paths used for the current host load.
    #[must_use]
    pub fn extension_paths(&self) -> Vec<String> {
        self.inner.extension_paths.clone()
    }

    /// Host-reported per-path load errors from the latest snapshot.
    #[must_use]
    pub fn load_errors(&self) -> Vec<(String, String)> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.load_errors.clone())
            .unwrap_or_default()
    }

    /// Registered provider config inputs keyed by provider id.
    #[must_use]
    pub fn provider_configs(&self) -> HashMap<String, ProviderConfigInput> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.provider_configs.clone())
            .unwrap_or_default()
    }

    /// Provider ids that expose a host-side `streamSimple` handler.
    #[must_use]
    pub fn stream_provider_ids(&self) -> HashSet<String> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.stream_provider_ids.clone())
            .unwrap_or_default()
    }

    /// Optional extension path per provider (diagnostics).
    #[must_use]
    pub fn provider_extension_paths(&self) -> HashMap<String, String> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.provider_extension_paths.clone())
            .unwrap_or_default()
    }

    /// Registered extension flags as name → type, for CLI validation.
    #[must_use]
    pub fn registered_flag_types(
        &self,
    ) -> BTreeMap<String, super::agent_session_services::ExtensionFlagType> {
        use super::agent_session_services::ExtensionFlagType;
        self.inner
            .snapshot
            .read()
            .map(|guard| {
                guard
                    .registry
                    .flags()
                    .iter()
                    .map(|flag| {
                        let kind = match flag.kind {
                            adapters::FlagKind::Boolean => ExtensionFlagType::Boolean,
                            adapters::FlagKind::String => ExtensionFlagType::String,
                        };
                        (flag.name.clone(), kind)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Registered extension provider adapters keyed by provider id, freshly
    /// bound to the live host client (callers register them with the model
    /// runtime). Rebuilt per call since [`ExtensionProvider`] is not `Clone`.
    ///
    /// Includes every host-registered provider. Custom-stream selection still
    /// requires `streamSimple: true` at registration time
    /// ([`Self::register_providers_on`]); baseURL-only providers stay native.
    #[must_use]
    pub fn providers(&self) -> HashMap<String, ExtensionProvider> {
        let client = Arc::clone(&self.inner.client);
        self.inner
            .snapshot
            .read()
            .map(|guard| {
                guard
                    .registry
                    .providers()
                    .iter()
                    .map(|provider| {
                        (
                            provider.name.clone(),
                            ExtensionProvider::new(provider.name.clone(), Arc::clone(&client)),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Register this host's provider configs + stream adapters on `runtime`.
    ///
    /// Each provider failure becomes a diagnostic string; siblings continue.
    /// Stream handlers are registered only when `streamSimple` was true.
    #[must_use]
    pub fn register_providers_on(
        &self,
        runtime: &ModelRuntime,
    ) -> Vec<(String, Result<(), ModelRuntimeError>)> {
        let configs = self.provider_configs();
        let stream_ids = self.stream_provider_ids();
        let paths = self.provider_extension_paths();
        let mut results = Vec::with_capacity(configs.len());
        for (name, config) in configs {
            let path = paths.get(&name).cloned().unwrap_or_else(|| name.clone());
            let outcome = runtime.register_provider(&name, config);
            if outcome.is_ok() && stream_ids.contains(&name) {
                let adapter = ExtensionProvider::new(name.clone(), Arc::clone(self.client()));
                runtime.register_extension_stream_provider(name.clone(), Arc::new(adapter));
            }
            results.push((path, outcome));
        }
        results
    }

    /// Unregister every provider currently owned by this runner from `runtime`.
    pub fn unregister_providers_from(&self, runtime: &ModelRuntime) {
        for name in self.provider_configs().keys() {
            runtime.unregister_provider(name);
        }
    }

    /// Snapshot of the pi-ext [`Registry`] (tools/commands/shortcuts/flags/
    /// renderers/providers with first-wins dedup applied).
    #[must_use]
    pub fn registry(&self) -> Registry {
        self.inner
            .snapshot
            .read()
            .map(|guard| clone_registry(&guard.registry))
            .unwrap_or_default()
    }

    /// Current reload generation (starts at 1, bumps on each reload).
    #[must_use]
    pub fn reload_generation(&self) -> u64 {
        self.inner.reload_generation.load(Ordering::Relaxed)
    }

    /// Whether the host transport is still believed alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.inner.client.is_running() && !self.inner.disabled.load(Ordering::Relaxed)
    }

    /// Set a flag value (CLI / settings source). Preserved across reload by
    /// the caller re-applying values after [`HostExtensionRunner::reload`].
    pub fn set_flag_value(&self, name: &str, value: Value) {
        if let Ok(mut flags) = self.inner.flag_values.write() {
            flags.insert(name.to_owned(), value);
        }
    }

    /// Apply a batch of flag values (CLI / preserved-across-reload).
    pub fn apply_flag_values(&self, values: &HashMap<String, Value>) {
        if let Ok(mut flags) = self.inner.flag_values.write() {
            for (name, value) in values {
                flags.insert(name.clone(), value.clone());
            }
        }
    }

    // -- Slot / tool-update / provider / error subscriptions ---------------

    /// Subscribe to a keyed UI slot lifecycle. The receiver yields the latest
    /// sanitized slot, or `None` when the slot is disposed or invalidated by a
    /// reload. New keys start disposed (`None`) until the host pushes content.
    #[must_use]
    pub fn subscribe_slot(&self, key: &str) -> watch::Receiver<Option<SanitizedSlot>> {
        if let Ok(mut slots) = self.inner.slots.write() {
            let sender = slots
                .entry(key.to_owned())
                .or_insert_with(|| watch::channel(None).0);
            return sender.subscribe();
        }
        // Lock poisoned: hand back a dead receiver.
        let (tx, rx) = watch::channel(None);
        let _ = tx.send(None);
        rx
    }

    /// Currently live slot keys.
    #[must_use]
    pub fn slot_keys(&self) -> Vec<String> {
        self.inner
            .slots
            .read()
            .map(|guard| guard.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Subscribe to unsolicited partial tool updates from extension tools.
    #[must_use]
    pub fn subscribe_tool_updates(&self) -> broadcast::Receiver<ToolUpdate> {
        self.inner.tool_updates_tx.subscribe()
    }

    /// Subscribe to unsolicited custom-provider stream events.
    #[must_use]
    pub fn subscribe_provider_events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.inner.provider_events_tx.subscribe()
    }

    /// Subscribe to non-retryable extension errors (host crashes, timeouts,
    /// remote error frames, handler-reported failures).
    #[must_use]
    pub fn subscribe_errors(&self) -> broadcast::Receiver<ExtensionErrorEvent> {
        self.inner.errors_tx.subscribe()
    }

    // -- Custom tool HTML rendering (session export) ----------------------

    /// Render an extension tool call or result as sanitized HTML for session
    /// export. Returns `Ok(None)` when no renderer is registered for
    /// `tool_name`. The host runs the registered `renderCall` / `renderResult`
    /// and returns an HTML fragment; Rust strips `<script>` / `<style>` blocks
    /// and escapes the remaining markup so plugin bytes never inject active
    /// content into an exported document.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionRunner` error](super::agent_session::extension_runner::ExtensionRunnerError)
    /// semantics: transport failures are reported as a non-retryable
    /// `extension_error` and the call resolves to `Ok(None)` (isolation).
    pub async fn render_extension_tool_html(
        &self,
        phase: ToolRenderPhase,
        tool_name: &str,
        payload: &Value,
    ) -> Option<String> {
        if !self.inner.active() {
            return None;
        }
        let request = serde_json::json!({
            "phase": phase.as_str(),
            "toolName": tool_name,
            "payload": payload,
        });
        match self
            .inner
            .client
            .request_raw(TOOL_RENDER_HTML_METHOD, request, self.inner.hook_timeout)
            .await
        {
            Ok(frame) => match serde_json::from_value::<ToolRenderHtmlWire>(frame.payload) {
                Ok(wire) => wire.html.as_deref().map(sanitize_html),
                Err(_) => None,
            },
            Err(err) => {
                self.inner.report_host_error(&err);
                None
            }
        }
    }

    // -- Reload / invalidate / shutdown -----------------------------------

    /// Bump the reload generation, dispose every active slot, and shut down
    /// the current host exactly once. The caller re-creates the runner (via
    /// [`HostExtensionRunner::start`] / [`connect`](Self::connect)) for the
    /// clean registration pass. Returns the new generation.
    pub async fn reload(&self) -> u64 {
        let generation = self
            .inner
            .reload_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.inner.dispose_all_slots();
        self.inner.stale.store(true, Ordering::Relaxed);
        // Mark shutdown done so a later dispose path does not double-reap.
        self.inner.shutdown_done.store(true, Ordering::Relaxed);
        let _ = self.inner.client.shutdown().await;
        self.inner.disabled.store(true, Ordering::Relaxed);
        generation
    }

    /// Sequential restart: await old transport reap, spawn a fresh host with
    /// the same extension paths, re-register providers on `runtime`, and
    /// restore flag values. Returns the new runner.
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError`] when the replacement host fails to start.
    /// On failure the old runner remains shut down; callers must degrade.
    pub async fn restart_and_rewire(
        &self,
        runtime: &ModelRuntime,
        preserved_flags: HashMap<String, Value>,
    ) -> Result<Arc<Self>, HostStartError> {
        // 1. Drop old provider registrations before the new host binds.
        self.unregister_providers_from(runtime);
        // 2. Await old transport shutdown / process reap exactly once.
        let _ = self.reload().await;
        // 3. Spawn replacement host with the same paths + cwd.
        let paths = self.inner.extension_paths.clone();
        let cwd = self.inner.load_cwd.clone();
        let replacement = Self::start_with_cwd(paths, cwd).await?;
        // 4. Re-register providers (sibling isolation on individual failures).
        let _ = replacement.register_providers_on(runtime);
        // 5. Restore flags.
        replacement.apply_flag_values(&preserved_flags);
        Ok(replacement)
    }

    /// Resolve + spawn with an explicit load cwd.
    ///
    /// # Errors
    ///
    /// See [`HostExtensionRunner::start`].
    pub async fn start_with_cwd(
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
    ) -> Result<Arc<Self>, HostStartError> {
        let spec = host::resolve_host()?;
        let client =
            Arc::new(HostClient::spawn(&spec).map_err(|e| HostStartError::Spawn(e.to_string()))?);
        let startup =
            Self::connect_with_cwd(Arc::clone(&client), extension_paths, load_cwd, HOOK_TIMEOUT)
                .await;
        Self::finish_startup(&client, startup).await
    }

    /// Mark this runner stale (session replacement). Subsequent hooks and
    /// handler-presence queries short-circuit to no-ops; active slots are
    /// disposed so the host disposes the previous component generation.
    pub fn invalidate(&self) {
        self.inner.stale.store(true, Ordering::Relaxed);
        self.inner.dispose_all_slots();
    }

    /// Graceful shutdown of the host client, exactly once. Repeated calls are
    /// no-ops. Slot subscriptions are disposed and the runner is marked
    /// disabled.
    pub async fn shutdown_once(&self) {
        if self.inner.shutdown_done.swap(true, Ordering::Relaxed) {
            return;
        }
        self.inner.dispose_all_slots();
        self.inner.disabled.store(true, Ordering::Relaxed);
        let _ = self.inner.client.shutdown().await;
    }
}

fn clone_registry(source: &Registry) -> Registry {
    let mut copy = Registry::new();
    for tool in source.tools() {
        let _ = copy.register_tool(tool.clone());
    }
    for command in source.commands() {
        let _ = copy.register_command(command.clone());
    }
    for shortcut in source.shortcuts() {
        let _ = copy.register_shortcut(shortcut.clone());
    }
    for flag in source.flags() {
        let _ = copy.register_flag(flag.clone());
    }
    for renderer in source.renderers() {
        let _ = copy.register_renderer(renderer.clone());
    }
    for provider in source.providers() {
        let _ = copy.register_provider(provider.clone());
    }
    copy
}

/// Spawn the unsolicited-event pump. Routes typed host events into the bounded
/// subscribers; on fatal host conditions marks the runner disabled and emits a
/// single non-retryable `extension_error`.
fn spawn_event_pump(inner: Arc<Inner>) {
    let mut rx = inner.client.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(HostEvent::UiSlot(slot)) => {
                    forward_slot(&inner, &slot);
                }
                Ok(HostEvent::DisposeSlot(d)) => {
                    forward_dispose(&inner, &d);
                }
                Ok(HostEvent::ToolUpdate(update)) => {
                    let _ = inner.tool_updates_tx.send(update);
                }
                Ok(HostEvent::ProviderEvent(event)) => {
                    let _ = inner.provider_events_tx.send(event);
                }
                Ok(HostEvent::ExtensionError(event)) => {
                    let _ = inner.errors_tx.send(event);
                }
                Ok(HostEvent::Raw(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Untyped / unexpected frame: ignore, host stays trusted.
                }
                Ok(HostEvent::Eof) => {
                    // Host stdout closed: fatal. Disable once, report, exit.
                    inner.disabled.store(true, Ordering::Relaxed);
                    inner.publish_error("extension_closed", "extension host stream closed", None);
                    break;
                }
                Ok(HostEvent::ProtocolError(message)) => {
                    inner.disabled.store(true, Ordering::Relaxed);
                    inner.publish_error("extension_protocol", &message, None);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn forward_slot(inner: &Arc<Inner>, slot: &UiSlot) {
    // Rust is the trust boundary: re-scrub every run/style/link field even
    // though the host is supposed to send structured runs.
    let sanitized = sanitize_slot(slot);
    if sanitized.had_rejections {
        inner.publish_error(
            "extension_sanitized",
            "extension uiSlot contained rejected control sequences or oversized fields",
            None,
        );
    }
    inner.slot_send(sanitized);
}

fn forward_dispose(inner: &Arc<Inner>, dispose: &DisposeSlot) {
    inner.slot_dispose(&dispose.key);
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

// ---------------------------------------------------------------------------
// ExtensionRunner trait impl
// ---------------------------------------------------------------------------

impl ExtensionRunner for HostExtensionRunner {
    fn has_handlers(&self, event: &str) -> bool {
        self.inner.has_handlers(event)
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<
        '_,
        Result<Option<CancelResult>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let method = match event.type_name() {
                "compaction_start" => "session_before_compact",
                "compaction_end" => "session_compact",
                "thinking_level_changed" => "thinking_level_select",
                name => name,
            };
            if !inner.has_handlers(method) {
                return Ok(None);
            }
            let payload =
                serde_json::to_value(&event).unwrap_or_else(|_| Value::Object(Map::new()));
            match inner.hook_request(method, payload).await {
                Ok(frame) => {
                    let result = serde_json::from_value::<Option<CancelWire>>(frame.payload)
                        .ok()
                        .flatten()
                        .map(|wire| CancelResult {
                            cancel: wire.cancel,
                            reason: wire.reason,
                        });
                    Ok(result)
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(None)
                }
            }
        })
    }

    fn emit_message_end(
        &self,
        message: AgentMessage,
    ) -> BoxFuture<
        '_,
        Result<Option<AgentMessage>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            if !inner.has_handlers("message_end") {
                return Ok(None);
            }
            let payload = serde_json::to_value(&message).unwrap_or(Value::Null);
            match inner.hook_request("message_end", payload).await {
                Ok(frame) => {
                    let replacement = serde_json::from_value::<MessageEndWire>(frame.payload)
                        .ok()
                        .and_then(|wire| wire.message);
                    // Enforce the role-preservation invariant the host merge
                    // guarantees; a mismatched role is dropped + reported.
                    let role_matches = replacement
                        .as_ref()
                        .is_some_and(|replacement| replacement.role() == message.role());
                    match replacement {
                        Some(message) if role_matches => Ok(Some(message)),
                        Some(_) => {
                            inner.publish_error(
                                "extension_message_end",
                                "message_end handler returned a message with a different role",
                                None,
                            );
                            Ok(None)
                        }
                        None => Ok(None),
                    }
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(None)
                }
            }
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
        let inner = Arc::clone(&self.inner);
        let tool_name = tool_name.to_owned();
        let tool_call_id = tool_call_id.to_owned();
        Box::pin(async move {
            if !inner.has_handlers("tool_call") {
                return Ok(None);
            }
            let payload = serde_json::json!({
                "toolName": tool_name,
                "toolCallId": tool_call_id,
                "input": input,
            });
            match inner.hook_request("tool_call", payload).await {
                Ok(frame) => {
                    let result =
                        serde_json::from_value::<Option<BeforeToolCallWire>>(frame.payload)
                            .ok()
                            .flatten()
                            .map(|wire| BeforeToolCallResult {
                                block: wire.block,
                                reason: wire.reason,
                            });
                    Ok(result)
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(None)
                }
            }
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
        let inner = Arc::clone(&self.inner);
        let tool_name = tool_name.to_owned();
        let tool_call_id = tool_call_id.to_owned();
        Box::pin(async move {
            if !inner.has_handlers("tool_result") {
                return Ok(None);
            }
            let payload = serde_json::json!({
                "toolName": tool_name,
                "toolCallId": tool_call_id,
                "input": input,
                "content": content,
                "details": details,
                "isError": is_error,
            });
            match inner.hook_request("tool_result", payload).await {
                Ok(frame) => {
                    let result = serde_json::from_value::<Option<AfterToolCallWire>>(frame.payload)
                        .ok()
                        .flatten()
                        .map(|wire| AfterToolCallResult {
                            content: wire.content,
                            details: wire.details,
                            is_error: wire.is_error,
                            terminate: wire.terminate,
                        });
                    Ok(result)
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(None)
                }
            }
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
        let inner = Arc::clone(&self.inner);
        let text = text.to_owned();
        let source = source.to_owned();
        let streaming_behavior = streaming_behavior.map(str::to_owned);
        Box::pin(async move {
            if !inner.has_handlers("input") {
                return Ok(InputTransformResult::default());
            }
            let payload = serde_json::json!({
                "text": text,
                "images": images,
                "source": source,
                "streamingBehavior": streaming_behavior,
            });
            match inner.hook_request("input", payload).await {
                Ok(frame) => {
                    let (handled, mapped_text, mapped_images) =
                        match serde_json::from_value::<InputTransformWire>(frame.payload) {
                            Ok(InputTransformWire::Handled) => (true, None, None),
                            Ok(InputTransformWire::Transform { text, images }) => {
                                (false, Some(text), images)
                            }
                            _ => (false, None, None),
                        };
                    Ok(InputTransformResult {
                        handled,
                        text: mapped_text,
                        images: mapped_images,
                    })
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(InputTransformResult::default())
                }
            }
        })
    }

    fn emit_before_agent_start(
        &self,
        prompt: &str,
        images: Option<Value>,
    ) -> BoxFuture<
        '_,
        Result<
            Option<BeforeAgentStartResult>,
            super::agent_session::extension_runner::ExtensionRunnerError,
        >,
    > {
        let inner = Arc::clone(&self.inner);
        let prompt = prompt.to_owned();
        Box::pin(async move {
            if !inner.has_handlers("before_agent_start") {
                return Ok(None);
            }
            let payload = serde_json::json!({
                "prompt": prompt,
                "images": images,
            });
            match inner.hook_request("before_agent_start", payload).await {
                Ok(frame) => {
                    let wire =
                        serde_json::from_value::<Option<BeforeAgentStartWire>>(frame.payload)
                            .ok()
                            .flatten();
                    Ok(wire.map(|wire| BeforeAgentStartResult {
                        messages: wire.messages,
                        system_prompt: wire.system_prompt,
                    }))
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(None)
                }
            }
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
        let inner = Arc::clone(&self.inner);
        let cwd = cwd.to_owned();
        let reason = reason.to_owned();
        Box::pin(async move {
            if !inner.has_handlers("resources_discover") {
                return Ok(ResourceExtensionPaths::default());
            }
            let payload = serde_json::json!({ "cwd": cwd, "reason": reason });
            match inner.hook_request("resources_discover", payload).await {
                Ok(frame) => {
                    let wire = serde_json::from_value::<ResourcesDiscoverWire>(frame.payload)
                        .unwrap_or_default();
                    let discovered = |paths: Option<Vec<ResourcePathWire>>| {
                        paths
                            .unwrap_or_default()
                            .into_iter()
                            .map(|entry| {
                                ExtensionResourcePath::discovered(entry.path, &entry.extension_path)
                            })
                            .collect()
                    };
                    Ok(ResourceExtensionPaths {
                        skill_paths: discovered(wire.skills),
                        prompt_paths: discovered(wire.prompts),
                        theme_paths: discovered(wire.themes),
                    })
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(ResourceExtensionPaths::default())
                }
            }
        })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        self.inner
            .snapshot
            .read()
            .map(|guard| {
                guard
                    .registry
                    .commands()
                    .iter()
                    .map(|command| command.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn execute_command(
        &self,
        name: &str,
        args: &str,
    ) -> BoxFuture<'_, Result<bool, super::agent_session::extension_runner::ExtensionRunnerError>>
    {
        let inner = Arc::clone(&self.inner);
        let name = name.to_owned();
        let args = args.to_owned();
        Box::pin(async move {
            if !inner.has_command(&name) {
                return Ok(false);
            }
            if !inner.active() {
                return Ok(false);
            }
            let payload = serde_json::json!({ "name": name, "args": args });
            match inner
                .client
                .request_raw(COMMAND_EXECUTE_METHOD, payload, inner.hook_timeout)
                .await
            {
                Ok(frame) => {
                    let ok = serde_json::from_value::<CommandExecuteWire>(frame.payload)
                        .is_ok_and(|wire| wire.ok);
                    Ok(ok)
                }
                Err(err) => {
                    inner.report_host_error(&err);
                    Ok(true)
                }
            }
        })
    }

    fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.tools.clone())
            .unwrap_or_default()
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        self.inner
            .flag_values
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn invalidate(&self) {
        HostExtensionRunner::invalidate(self);
    }

    fn emit_error(&self, message: String) {
        self.inner.publish_error("extension_error", &message, None);
    }

    fn shutdown(
        &self,
        reason: &str,
    ) -> BoxFuture<'_, Result<(), super::agent_session::extension_runner::ExtensionRunnerError>>
    {
        let inner = Arc::clone(&self.inner);
        let reason = reason.to_owned();
        Box::pin(async move {
            if reason == "reload" {
                inner.reload_generation.fetch_add(1, Ordering::Relaxed);
            }
            HostExtensionRunner::shutdown_once_with_inner(&inner).await;
            Ok(())
        })
    }
}

impl HostExtensionRunner {
    async fn shutdown_once_with_inner(inner: &Arc<Inner>) {
        if inner.shutdown_done.swap(true, Ordering::Relaxed) {
            return;
        }
        inner.dispose_all_slots();
        inner.disabled.store(true, Ordering::Relaxed);
        let _ = inner.client.shutdown().await;
    }
}

#[cfg(test)]
mod tests;
