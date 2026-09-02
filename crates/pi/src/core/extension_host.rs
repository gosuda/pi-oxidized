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
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_agent::{AfterToolCallResult, AgentMessage, AgentTool, BeforeToolCallResult};
use pi_ai::{AssistantMessage, AssistantMessageEvent, ToolResultContent};
use pi_ext::adapters::{
    self, CommandRegistration, CommandSourceInfo, ExtensionAgentTool, ExtensionProvider,
    FlagRegistration, ProviderRegistration, Registry, RendererRegistration, ShortcutRegistration,
    ToolRegistration,
};
use pi_ext::client::{
    HostClient, HostClientError, HostNotification, HostSessionControlEvent, HostSessionRequest,
    HostUiRequest, HostUiResponse,
};
use pi_ext::host::{self, HostError, HostSpec};
use pi_ext::protocol::{
    self, DisposeSlot, ExtensionErrorEvent, FlagValueWire, FlagsSetRequest, FlagsSetResponse,
    ProviderEvent, SessionCommandInfoWire, SessionScopedModelWire, SessionStateWire,
    SessionToolWire, ShortcutExecuteRequest, ShortcutExecuteResponse, ThemeUpdate, ToolUpdate,
    UiEventRequest, UiEventResponse, UiSlot, UiStateWire,
};
use pi_ext::sanitize::{SanitizedSlot, sanitize_slot};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc, watch};

use super::agent_session::bridge_types::{
    BridgeMethod, BridgeRequestId, CommandCatalogEntry, CompactRequest, ExtensionHostError,
    ForkRequest, NavigateTreeRequest, NewSessionRequest, SessionCommand, SessionCommandEnvelope,
    SessionState, SetModelRequest, SetupEntriesRequest, SwitchSessionRequest,
};
use super::agent_session::events::AgentSessionEvent;
use super::agent_session::extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, InputTransformResult,
};
use super::agent_session::tree::NavigateTreeResult;
use super::extension_runtime_set::EndpointId;
use super::model_runtime::{
    ModelRuntime, ModelRuntimeError, ProviderConfigInput, ProviderModelDefinition,
};
use super::resources::source_info::{SourceInfo, SourceOrigin, SourceScope};
use super::resources::{ExtensionResourcePath, ResourceExtensionPaths};

mod ui_event;

pub use ui_event::{
    ExtensionNotice, ExtensionNoticeLevel, ExtensionThemeRequest, ExtensionUiControl,
    ExtensionUiEvent, MalformedThemeSet,
};

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

/// The 35 lifecycle event `type` discriminants mirrored from the reference
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
    "ui_prompt_start",
    "ui_prompt_end",
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

/// One item on the claimed session-action bridge.
///
/// Delivered by [`HostExtensionRunner::take_session_bridge`]; the claiming
/// session task applies each item against the effective `AgentSession` and
/// answers every correlated request through this runner.
#[derive(Debug, Clone)]
pub(crate) enum SessionBridgeEvent {
    /// Fire-and-forget extension session action.
    Command {
        /// Action and optional pending-replacement scope.
        envelope: SessionCommandEnvelope,
        /// Endpoint that emitted the command.
        origin: Option<EndpointId>,
    },
    /// Correlated `pi.setModel` request.
    SetModel {
        /// Host correlation id (echo into `respond_set_model`).
        id: BridgeRequestId,
        /// Requested model payload.
        request: SetModelRequest,
    },
    /// Correlated `ctx.compact` request.
    Compact {
        /// Host correlation id (echo into `respond_compact`).
        id: BridgeRequestId,
        /// Compact request payload.
        request: CompactRequest,
    },
    /// Correlated `ctx.newSession` request.
    NewSession {
        /// Host correlation id (echo into `respond_new_session`).
        id: BridgeRequestId,
        /// New-session request payload.
        request: NewSessionRequest,
    },
    /// Correlated `ctx.fork` request.
    Fork {
        /// Host correlation id (echo into `respond_fork`).
        id: BridgeRequestId,
        /// Fork request payload.
        request: ForkRequest,
    },
    /// Correlated `ctx.navigateTree` request.
    NavigateTree {
        /// Host correlation id (echo into `respond_navigate_tree`).
        id: BridgeRequestId,
        /// Navigate-tree request payload.
        request: NavigateTreeRequest,
    },
    /// Correlated `ctx.switchSession` request.
    SwitchSession {
        /// Host correlation id (echo into `respond_switch_session`).
        id: BridgeRequestId,
        /// Switch-session request payload.
        request: SwitchSessionRequest,
    },
    /// Correlated `ctx.reload` request.
    Reload {
        /// Host correlation id (echo into `respond_reload`).
        id: BridgeRequestId,
    },
    /// Correlated `session.setupEntries` request (host → Rust).
    SetupEntries {
        /// Host correlation id (echo into `respond_setup_entries`).
        id: BridgeRequestId,
        /// Setup-entries request payload.
        request: SetupEntriesRequest,
        /// Endpoint that requested the candidate snapshot.
        origin: Option<EndpointId>,
    },
    /// Host completed the command that initiated a ready-gated operation.
    ReplacementReady {
        /// Facade token returned by the replacement response.
        token: String,
        /// Endpoint that completed the replacement.
        origin: Option<EndpointId>,
    },
    /// Host abandoned a ready-gated replacement without completing it.
    ReplacementAbort {
        /// Facade token returned by the replacement response.
        token: String,
        /// Endpoint that abandoned the replacement.
        origin: Option<EndpointId>,
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

// ---------------------------------------------------------------------------
// Wire → product conversions (owned at the host adapter seam)
// ---------------------------------------------------------------------------

impl From<HostClientError> for ExtensionHostError {
    fn from(value: HostClientError) -> Self {
        match value {
            HostClientError::Handshake { message } => Self::Handshake { message },
            HostClientError::Timeout { id, timeout } => Self::Timeout {
                message: format!("host request {id} timed out after {timeout:?}"),
            },
            HostClientError::Cancelled { .. } => Self::Cancelled {
                message: "request cancelled".to_owned(),
            },
            HostClientError::Closed { message, stderr } => Self::Closed { message, stderr },
            HostClientError::Protocol { message, stderr } => Self::Protocol { message, stderr },
            HostClientError::Remote { code, message } => Self::Remote { code, message },
            HostClientError::Spawn { message } => Self::Spawn { message },
            HostClientError::NotRunning => Self::NotRunning,
            HostClientError::Payload(message) => Self::Payload { message },
        }
    }
}

impl From<pi_ext::protocol::SessionCommandEnvelope> for SessionCommandEnvelope {
    fn from(wire: pi_ext::protocol::SessionCommandEnvelope) -> Self {
        Self {
            replacement_token: wire.replacement_token,
            command: wire.command.into(),
        }
    }
}

impl From<pi_ext::protocol::SessionCommand> for SessionCommand {
    fn from(wire: pi_ext::protocol::SessionCommand) -> Self {
        match wire {
            pi_ext::protocol::SessionCommand::SendMessage { message, options } => {
                Self::SendMessage { message, options }
            }
            pi_ext::protocol::SessionCommand::SendUserMessage { content, options } => {
                Self::SendUserMessage { content, options }
            }
            pi_ext::protocol::SessionCommand::AppendEntry { custom_type, data } => {
                Self::AppendEntry { custom_type, data }
            }
            pi_ext::protocol::SessionCommand::SetSessionName { name } => {
                Self::SetSessionName { name }
            }
            pi_ext::protocol::SessionCommand::SetLabel { entry_id, label } => {
                Self::SetLabel { entry_id, label }
            }
            pi_ext::protocol::SessionCommand::SetActiveTools { tool_names } => {
                Self::SetActiveTools { tool_names }
            }
            pi_ext::protocol::SessionCommand::RefreshTools => Self::RefreshTools,
            pi_ext::protocol::SessionCommand::SetThinkingLevel { level } => {
                Self::SetThinkingLevel { level }
            }
            pi_ext::protocol::SessionCommand::Abort => Self::Abort,
            pi_ext::protocol::SessionCommand::Shutdown => Self::Shutdown,
        }
    }
}

impl From<pi_ext::protocol::SessionSetModelRequest> for SetModelRequest {
    fn from(wire: pi_ext::protocol::SessionSetModelRequest) -> Self {
        Self { model: wire.model }
    }
}

impl From<pi_ext::protocol::SessionCompactRequest> for CompactRequest {
    fn from(wire: pi_ext::protocol::SessionCompactRequest) -> Self {
        Self {
            custom_instructions: wire.custom_instructions,
        }
    }
}

impl From<pi_ext::protocol::SessionNewSessionRequest> for NewSessionRequest {
    fn from(wire: pi_ext::protocol::SessionNewSessionRequest) -> Self {
        Self {
            parent_session: wire.parent_session,
        }
    }
}

impl From<pi_ext::protocol::SessionForkRequest> for ForkRequest {
    fn from(wire: pi_ext::protocol::SessionForkRequest) -> Self {
        Self {
            entry_id: wire.entry_id,
            position: wire.position.map(|p| match p {
                pi_ext::protocol::SessionForkPosition::Before => {
                    crate::core::agent_session_runtime::ForkPosition::Before
                }
                pi_ext::protocol::SessionForkPosition::At => {
                    crate::core::agent_session_runtime::ForkPosition::At
                }
            }),
        }
    }
}

impl From<pi_ext::protocol::SessionSwitchSessionRequest> for SwitchSessionRequest {
    fn from(wire: pi_ext::protocol::SessionSwitchSessionRequest) -> Self {
        Self {
            session_path: wire.session_path,
        }
    }
}

impl From<pi_ext::protocol::SessionNavigateTreeRequest> for NavigateTreeRequest {
    fn from(wire: pi_ext::protocol::SessionNavigateTreeRequest) -> Self {
        Self {
            target_id: wire.target_id,
            options: super::agent_session::tree::NavigateTreeOptions {
                summarize: wire.summarize.unwrap_or(false),
                custom_instructions: wire.custom_instructions,
                replace_instructions: wire.replace_instructions.unwrap_or(false),
                label: wire.label,
            },
        }
    }
}

impl From<pi_ext::protocol::SessionSetupEntriesRequest> for SetupEntriesRequest {
    fn from(wire: pi_ext::protocol::SessionSetupEntriesRequest) -> Self {
        Self {
            replacement_token: wire.replacement_token,
        }
    }
}

/// Convert the pi-ext wire `CommandSourceInfo` into the product `SourceInfo`.
///
/// Moved here from `agent_session/extension.rs:695-701` — the host adapter
/// owns every pi-ext ↔ product conversion.
impl From<CommandSourceInfo> for SourceInfo {
    fn from(info: CommandSourceInfo) -> Self {
        Self {
            path: info.path,
            source: info.source,
            scope: match info.scope {
                pi_ext::adapters::CommandSourceScope::User => SourceScope::User,
                pi_ext::adapters::CommandSourceScope::Project => SourceScope::Project,
                pi_ext::adapters::CommandSourceScope::Temporary => SourceScope::Temporary,
            },
            origin: match info.origin {
                pi_ext::adapters::CommandSourceOrigin::Package => SourceOrigin::Package,
                pi_ext::adapters::CommandSourceOrigin::TopLevel => SourceOrigin::TopLevel,
            },
            base_dir: info.base_dir,
        }
    }
}

/// Map a [`BridgeMethod`] to its wire method string.
fn bridge_method_to_wire(method: BridgeMethod) -> &'static str {
    match method {
        BridgeMethod::NewSession => protocol::SESSION_NEW_SESSION_METHOD,
        BridgeMethod::Fork => protocol::SESSION_FORK_METHOD,
        BridgeMethod::SwitchSession => protocol::SESSION_SWITCH_SESSION_METHOD,
        BridgeMethod::NavigateTree => protocol::SESSION_NAVIGATE_TREE_METHOD,
        BridgeMethod::Reload => protocol::SESSION_RELOAD_METHOD,
        BridgeMethod::SetupEntries => protocol::SESSION_SETUP_ENTRIES_METHOD,
        BridgeMethod::SetModel => protocol::SESSION_SET_MODEL_METHOD,
        BridgeMethod::Compact => protocol::SESSION_COMPACT_METHOD,
    }
}

/// Wire string for a thinking level (serde `lowercase` discriminant).
///
/// Mirrors the private function in `agent_session/extension.rs`; kept here so
/// `session_state_to_wire` does not depend on that module's internals.
fn thinking_level_wire(level: pi_ai::ModelThinkingLevel) -> String {
    serde_json::to_value(level)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "medium".to_owned())
}

/// Convert a product [`SessionState`] into the wire [`SessionStateWire`].
fn session_state_to_wire(state: &SessionState) -> SessionStateWire {
    SessionStateWire {
        session_name: state.session_name.clone(),
        thinking_level: thinking_level_wire(state.thinking_level),
        active_tools: state.active_tools.clone(),
        all_tools: state
            .all_tools
            .iter()
            .map(|tool| SessionToolWire {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                source: None,
            })
            .collect(),
        commands: state
            .commands
            .iter()
            .map(|command| SessionCommandInfoWire {
                name: command.name.clone(),
                description: command.description.clone(),
                source: match command.source {
                    super::resources::slash::SlashCommandSource::Extension => {
                        "extension".to_owned()
                    }
                    super::resources::slash::SlashCommandSource::Prompt => "prompt".to_owned(),
                    super::resources::slash::SlashCommandSource::Skill => "skill".to_owned(),
                },
            })
            .collect(),
        model: state
            .model
            .as_ref()
            .and_then(|m| serde_json::to_value(m).ok()),
        scoped_models: state
            .scoped_models
            .iter()
            .map(|scoped| SessionScopedModelWire {
                model: serde_json::to_value(&scoped.model).unwrap_or(Value::Null),
                thinking_level: scoped.thinking_level.map(thinking_level_wire),
            })
            .collect(),
        is_idle: state.is_idle,
        has_pending_messages: state.has_pending_messages,
        context_usage: state.context_usage.map(|usage| {
            serde_json::json!({
                "tokens": usage.tokens,
                "contextWindow": usage.context_window,
                "percent": usage.percent,
            })
        }),
        system_prompt: state.system_prompt.clone(),
    }
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
    #[serde(default)]
    source_info: Option<CommandSourceInfo>,
}

/// Wire form of [`ShortcutRegistration`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutWire {
    key: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    extension_path: Option<String>,
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
    /// Native and TypeScript hosts emit boolean or string defaults (`FlagValueWire`).
    #[serde(default)]
    default: Option<FlagValueWire>,
    /// Currently resolved value (from CLI / settings), if any.
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    extension_path: Option<String>,
}

fn flag_value_wire_to_json(value: &FlagValueWire) -> Value {
    match value {
        FlagValueWire::Boolean(value) => Value::Bool(*value),
        FlagValueWire::String(value) => Value::String(value.clone()),
    }
}

fn flag_value_wire_to_legacy_default(value: &FlagValueWire) -> String {
    match value {
        FlagValueWire::Boolean(value) => value.to_string(),
        FlagValueWire::String(value) => value.clone(),
    }
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
    /// Whether `ui.onTerminalInput` has at least one active handler.
    #[serde(default)]
    terminal_input: bool,
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
        let _ = snapshot.registry.register_command(CommandRegistration {
            name: command.name,
            description: command.description,
            source: command.source,
            source_info: command.source_info,
        });
    }

    for shortcut in wire.shortcuts {
        let registration = ShortcutRegistration {
            key: shortcut.key,
            description: shortcut.description,
            extension_path: shortcut.extension_path,
        };
        snapshot.raw_shortcuts.push(registration.clone());
        let _ = snapshot.registry.register_shortcut(registration);
    }

    for flag in wire.flags {
        if snapshot.registry.register_flag(FlagRegistration {
            name: flag.name.clone(),
            description: flag.description,
            kind: match flag.kind.as_deref() {
                Some("boolean") => adapters::FlagKind::Boolean,
                _ => adapters::FlagKind::String,
            },
            default: flag.default.as_ref().map(flag_value_wire_to_legacy_default),
            extension_path: flag.extension_path,
        }) {
            // First-wins: prefer the host-resolved value, fall back to default,
            // then to a typed fallback (false for boolean flags, "" for string).
            let value = flag
                .value
                .clone()
                .or_else(|| flag.default.as_ref().map(flag_value_wire_to_json))
                .unwrap_or_else(|| match flag.kind.as_deref() {
                    Some("boolean") => Value::Bool(false),
                    _ => Value::String(String::new()),
                });
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

type ReplacementDropHandler = Arc<dyn Fn(&str, Option<EndpointId>) + Send + Sync>;

struct Inner {
    client: Arc<HostClient>,
    snapshot: RwLock<RegistrySnapshot>,
    flag_values: RwLock<HashMap<String, Value>>,
    slots: RwLock<HashMap<String, SlotWatch>>,
    tool_updates_tx: broadcast::Sender<ToolUpdate>,
    provider_events_tx: broadcast::Sender<ProviderEvent>,
    errors_tx: broadcast::Sender<ExtensionErrorEvent>,
    ui_tx: broadcast::Sender<ExtensionUiEvent>,
    session_bridge_tx: mpsc::Sender<SessionBridgeEvent>,
    session_bridge_rx: StdMutex<Option<mpsc::Receiver<SessionBridgeEvent>>>,
    session_bridge_claimed: AtomicBool,
    /// Paths passed to `extensions.load` (restart reuses them).
    extension_paths: Vec<String>,
    /// Cwd passed to `extensions.load`.
    load_cwd: String,
    /// Project trust passed to `extensions.load` and preserved across restart.
    project_trusted: bool,
    /// Monotonic reload generation; bumps invalidate every active slot.
    reload_generation: AtomicU64,
    /// Runtime theme generation carried on measure/render requests; updated
    /// by every `theme.update` push.
    theme_generation: AtomicU64,
    /// Host transport is gone (EOF / crash / protocol error). All hooks and
    /// handler-presence queries short-circuit to no-ops once set.
    disabled: AtomicBool,
    /// Runner invalidated after session replacement (`/reload` / runtime swap).
    stale: AtomicBool,
    /// `shutdown` has completed at least once.
    shutdown_done: AtomicBool,
    /// Serializes shutdown so concurrent callers await the same completed reap.
    shutdown_lock: tokio::sync::Mutex<()>,
    /// Per-hook control-RPC deadline (`HOOK_TIMEOUT` in production; shorter in
    /// tests to exercise the timeout path quickly).
    hook_timeout: Duration,
    /// Stable endpoint identity assigned when this runner joins a generation.
    endpoint_id: StdMutex<Option<EndpointId>>,
    /// Lower-layer callback invoked when a token-bearing replacement event is
    /// dropped at either host bounded-send hop. The callback receives the
    /// facade token and endpoint so the facade can reject cross-endpoint control.
    replacement_drop: StdMutex<Option<ReplacementDropHandler>>,
}
impl Inner {
    fn new(
        client: Arc<HostClient>,
        snapshot: RegistrySnapshot,
        extension_paths: Vec<String>,
        load_cwd: String,
        project_trusted: bool,
        hook_timeout: Duration,
    ) -> Self {
        let (tool_updates_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (provider_events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (errors_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (ui_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (session_bridge_tx, session_bridge_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let flag_values = snapshot.flag_values.clone();
        Self {
            client,
            snapshot: RwLock::new(snapshot),
            flag_values: RwLock::new(flag_values),
            slots: RwLock::new(HashMap::new()),
            tool_updates_tx,
            provider_events_tx,
            errors_tx,
            ui_tx,
            session_bridge_tx,
            session_bridge_rx: StdMutex::new(Some(session_bridge_rx)),
            session_bridge_claimed: AtomicBool::new(false),
            extension_paths,
            load_cwd,
            project_trusted,
            reload_generation: AtomicU64::new(1),
            disabled: AtomicBool::new(false),
            stale: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
            shutdown_lock: tokio::sync::Mutex::new(()),
            hook_timeout,
            theme_generation: AtomicU64::new(0),
            replacement_drop: StdMutex::new(None),
            endpoint_id: StdMutex::new(None),
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
        // Teardown must not be reanimated: after invalidate/shutdown a
        // delayed in-flight slot would otherwise re-insert a watch entry.
        // Both the active check AND the synchronous ui_tx publish run UNDER
        // the slots lock, so they serialize against dispose_all_slots and a
        // Slot event can never be published after the teardown Dispose.
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        if !self.active() {
            return;
        }
        let sender = slots
            .entry(slot.key.clone())
            .or_insert_with(|| watch::channel(None).0);
        let _ = sender.send(Some(slot.clone()));
        let _ = self.ui_tx.send(ExtensionUiEvent::Slot(slot));
    }

    fn slot_dispose(&self, key: &str) {
        // Same lock discipline as slot_send: watch update and public publish
        // are one atomic step relative to teardown.
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        if !self.active() {
            return;
        }
        if let Some(sender) = slots.get(key) {
            let _ = sender.send(None);
        }
        slots.remove(key);
        let _ = self.ui_tx.send(ExtensionUiEvent::Dispose {
            key: key.to_owned(),
        });
    }

    fn theme_set_send(&self, request: ExtensionThemeRequest) {
        // Same lock discipline as notify_send: serialize against teardown.
        let Ok(_slots) = self.slots.write() else {
            return;
        };
        if !self.active() {
            return;
        }
        let _ = self.ui_tx.send(ExtensionUiEvent::ThemeSet(request));
    }

    fn ui_control_send(&self, control: ExtensionUiControl) {
        // Same lock discipline as notify_send: serialize against teardown.
        let Ok(_slots) = self.slots.write() else {
            return;
        };
        if !self.active() {
            return;
        }
        let _ = self.ui_tx.send(ExtensionUiEvent::UiControl(control));
    }

    fn notify_send(&self, notice: ExtensionNotice) {
        // Same lock discipline as slot_send/slot_dispose: the active check
        // and the synchronous publish serialize against teardown so a Notify
        // can never land after the teardown Dispose.
        let Ok(_slots) = self.slots.write() else {
            return;
        };
        if !self.active() {
            return;
        }
        let _ = self.ui_tx.send(ExtensionUiEvent::Notify(notice));
    }

    fn dispose_all_slots(&self) {
        // Publishing the Dispose events while still holding the lock keeps
        // Slot-before-teardown-Dispose ordering: any concurrent slot_send
        // either published before we acquired the lock or is dropped by the
        // inactive gate afterwards. `broadcast::Sender::send` never blocks.
        let Ok(mut slots) = self.slots.write() else {
            return;
        };
        for (key, sender) in slots.iter() {
            let _ = sender.send(None);
            let _ = self
                .ui_tx
                .send(ExtensionUiEvent::Dispose { key: key.clone() });
        }
        slots.clear();
    }

    fn endpoint_id(&self) -> Option<EndpointId> {
        *self
            .endpoint_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let load_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::connect_with_cwd_and_trust(client, extension_paths, load_cwd, false, hook_timeout)
            .await
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
        Self::connect_with_cwd_and_trust(client, extension_paths, load_cwd, false, hook_timeout)
            .await
    }

    /// Bind a runner with an explicit load cwd and project-trust value.
    ///
    /// # Errors
    ///
    /// Returns [`HostStartError::Handshake`] or [`HostStartError::Load`].
    pub async fn connect_with_cwd_and_trust(
        client: Arc<HostClient>,
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        project_trusted: bool,
        hook_timeout: Duration,
    ) -> Result<Arc<Self>, HostStartError> {
        client.handshake().await?;
        let load_cwd = load_cwd.into();
        let snapshot = Self::load(&client, &extension_paths, &load_cwd, project_trusted).await?;
        let inner = Arc::new(Inner::new(
            Arc::clone(&client),
            snapshot,
            extension_paths,
            load_cwd,
            project_trusted,
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

    /// Replace this endpoint's provider snapshot from a `providers.update`
    /// event. The update carries the endpoint's complete current provider
    /// list; unknown fields are ignored.
    ///
    /// This only mutates the runner's local snapshot — the caller must
    /// rebuild the aggregate in live endpoint order and rewire
    /// `ModelRuntime` after releasing registry locks.
    pub(crate) fn apply_providers_update(&self, update: &pi_ext::protocol::ProvidersUpdate) {
        let mut guard = self
            .inner
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = &mut *guard;
        // Clear and rebuild from the update.
        snap.provider_configs.clear();
        snap.stream_provider_ids.clear();
        snap.registry.clear_providers();
        snap.provider_extension_paths.clear();
        for entry in &update.providers {
            let name = entry.name.clone();
            let config = ProviderConfigInput {
                name: Some(entry.name.clone()),
                base_url: entry.base_url.clone(),
                api_key: entry.api_key.clone(),
                api: entry.api.clone(),
                headers: entry.headers.clone(),
                auth_header: entry.auth_header,
                models: entry.models.as_ref().map(|models| {
                    models
                        .iter()
                        .filter_map(|v| {
                            serde_json::from_value::<ProviderModelDefinition>(v.clone()).ok()
                        })
                        .collect()
                }),
                model_overrides: None,
                oauth: None,
            };
            snap.provider_configs.insert(name.clone(), config);
            if entry.stream_simple {
                snap.stream_provider_ids.insert(name.clone());
            }
            if let Some(path) = &entry.extension_path {
                snap.provider_extension_paths
                    .insert(name.clone(), path.clone());
            }
            // Register in the registry.
            let _ = snap.registry.register_provider(ProviderRegistration {
                name: name.clone(),
                base_url: entry.base_url.clone(),
                api: entry.api.clone(),
            });
        }
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

    /// Product command catalog: extension-registered commands with
    /// host-reported provenance converted to `SourceInfo`. This is the
    /// adapter-owned `pi_ext::adapters::Registry` conversion — callers never
    /// see wire registry metadata. Resource-discovered provenance overlays
    /// this in `ExtensionRuntimeSet::command_catalog`.
    #[must_use]
    pub fn command_catalog(&self) -> Vec<CommandCatalogEntry> {
        self.registry()
            .commands()
            .iter()
            .map(|command| CommandCatalogEntry {
                name: command.name.clone(),
                description: command.description.clone().unwrap_or_default(),
                source: command.source.clone(),
                source_info: command.source_info.clone().map(SourceInfo::from),
            })
            .collect()
    }

    /// Ordered, undeduplicated host shortcut registrations.
    ///
    /// Product code applies last-wins filtering after combining extension and native shortcuts.
    #[must_use]
    pub fn raw_shortcuts(&self) -> Vec<ShortcutRegistration> {
        self.inner
            .snapshot
            .read()
            .map(|guard| guard.raw_shortcuts.clone())
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

    /// Synchronize a complete validated flag overlay with the host.
    ///
    /// The local flag snapshot is updated only after the host acknowledges the request.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Payload`] when the request or response payload
    /// cannot be (de)serialized, or when the host rejects the overlay
    /// (`ok == false`). Propagates the transport-level error from
    /// [`hook_request`](HostClientInner::hook_request) otherwise:
    /// [`HostClientError::NotRunning`] when the host is down, and
    /// [`HostClientError::Timeout`], [`HostClientError::Closed`], or
    /// [`HostClientError::Remote`] on transport failure.
    pub async fn apply_flag_values(
        &self,
        values: &BTreeMap<String, FlagValueWire>,
    ) -> Result<(), HostClientError> {
        let payload = protocol::to_payload(&FlagsSetRequest {
            values: values.clone(),
        })
        .map_err(|error| HostClientError::Payload(format!("encode flags.set: {error}")))?;
        let frame = self
            .inner
            .hook_request(protocol::FLAGS_SET_METHOD, payload)
            .await?;
        let response: FlagsSetResponse = protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode flags.set: {error}")))?;
        if !response.ok {
            return Err(HostClientError::Payload(
                "flags.set rejected by extension host".to_owned(),
            ));
        }
        if let Ok(mut flags) = self.inner.flag_values.write() {
            for (name, value) in values {
                let value = match value {
                    FlagValueWire::Boolean(value) => Value::Bool(*value),
                    FlagValueWire::String(value) => Value::String(value.clone()),
                };
                flags.insert(name.clone(), value);
            }
        }
        Ok(())
    }

    /// Dispatch one effective extension shortcut.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Payload`] when the request or response payload
    /// cannot be (de)serialized. Propagates the transport-level error from
    /// [`hook_request`](HostClientInner::hook_request) otherwise:
    /// [`HostClientError::NotRunning`] when the host is down, and
    /// [`HostClientError::Timeout`], [`HostClientError::Closed`], or
    /// [`HostClientError::Remote`] on transport failure.
    pub async fn execute_shortcut(
        &self,
        key: impl Into<String>,
    ) -> Result<ShortcutExecuteResponse, HostClientError> {
        let payload =
            protocol::to_payload(&ShortcutExecuteRequest { key: key.into() }).map_err(|error| {
                HostClientError::Payload(format!("encode shortcut.execute: {error}"))
            })?;
        let frame = self
            .inner
            .hook_request(protocol::SHORTCUT_EXECUTE_METHOD, payload)
            .await?;
        protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode shortcut.execute: {error}")))
    }

    /// Deliver one event to a keyed UI slot generation.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Payload`] when the request or response payload
    /// cannot be (de)serialized. Propagates the transport-level error from
    /// [`hook_request`](HostClientInner::hook_request) otherwise:
    /// [`HostClientError::NotRunning`] when the host is down, and
    /// [`HostClientError::Timeout`], [`HostClientError::Closed`], or
    /// [`HostClientError::Remote`] on transport failure.
    pub async fn send_ui_event(
        &self,
        request: UiEventRequest,
    ) -> Result<UiEventResponse, HostClientError> {
        let payload = protocol::to_payload(&request)
            .map_err(|error| HostClientError::Payload(format!("encode uiEvent: {error}")))?;
        let frame = self
            .inner
            .hook_request(protocol::Method::UiEvent.as_str(), payload)
            .await?;
        protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode uiEvent: {error}")))
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

    /// Snapshot all currently live sanitized slots.
    ///
    /// This lets a mode attach after `session_start` without losing widgets
    /// already published before its broadcast subscription existed.
    #[must_use]
    pub fn current_slots(&self) -> Vec<SanitizedSlot> {
        let mut slots = self
            .inner
            .slots
            .read()
            .map(|guard| {
                guard
                    .values()
                    .filter_map(|sender| sender.borrow().clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        slots.sort_by(|left, right| left.key.cmp(&right.key));
        slots
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

    /// Whether the loaded host has an active `ui.onTerminalInput` handler.
    #[must_use]
    pub fn has_terminal_input_handlers(&self) -> bool {
        self.inner
            .snapshot
            .read()
            .is_ok_and(|snapshot| snapshot.terminal_input)
    }

    /// Offer canonical terminal input to the host's sequential 4 ms actor.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError`] when the host transport is down, the 4 ms
    /// deadline elapses, or the response payload cannot be decoded.
    pub async fn terminal_input(
        &self,
        data: &str,
    ) -> Result<protocol::TerminalInputResult, HostClientError> {
        self.terminal_input_within(data, Duration::from_millis(4))
            .await
    }

    /// Deadline-injectable terminal-input request.
    ///
    /// Production callers go through [`Self::terminal_input`] (4 ms); the
    /// facade aggregate fan-out forwards a shared deadline here so tests can
    /// exercise the full routing path with a generous deadline instead of
    /// racing the host transport under the production 4 ms budget.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError`] when the host transport is down, `deadline`
    /// elapses, or the response payload cannot be decoded.
    pub(crate) async fn terminal_input_within(
        &self,
        data: &str,
        deadline: Duration,
    ) -> Result<protocol::TerminalInputResult, HostClientError> {
        let frame = self
            .inner
            .client
            .request(
                protocol::Method::TerminalInput,
                serde_json::json!({ "data": data }),
                deadline,
            )
            .await?;
        protocol::from_payload(&frame.payload)
            .map_err(|error| HostClientError::Payload(format!("decode terminalInput: {error}")))
    }

    /// Subscribe to host notifications and sanitized slot lifecycle.
    #[must_use]
    pub fn subscribe_ui(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.inner.ui_tx.subscribe()
    }

    /// Runtime theme generation from the latest [`Self::push_theme_update`].
    ///
    /// Measure/render callers pass this so host components re-measure after
    /// a theme switch.
    #[must_use]
    pub fn theme_generation(&self) -> u64 {
        self.inner.theme_generation.load(Ordering::Relaxed)
    }

    /// Push the active theme, catalog, and generation to the host
    /// (`theme.update` event). The host refreshes `ctx.ui.theme`, its theme
    /// catalog, and re-pushes every live slot with the new colors.
    ///
    /// Host failures are isolated as a single non-retryable
    /// `extension_error`; the session survives.
    pub async fn push_theme_update(&self, update: &ThemeUpdate) {
        if !self.inner.active() {
            return;
        }
        self.inner
            .theme_generation
            .store(update.theme_generation, Ordering::Relaxed);
        let payload = match serde_json::to_value(update) {
            Ok(payload) => payload,
            Err(error) => {
                self.inner.publish_error(
                    "extension_protocol",
                    &format!("encode theme.update: {error}"),
                    None,
                );
                return;
            }
        };
        if let Err(error) = self
            .inner
            .client
            .send_event(protocol::THEME_UPDATE_METHOD, payload)
            .await
        {
            self.inner.report_host_error(&error);
        }
    }

    /// Claim the sole lossless receiver for correlated host dialog requests.
    ///
    /// A product mode calls this exactly once when it binds. Subsequent callers
    /// receive `None`, preventing two modes from racing responses.
    #[must_use]
    pub fn take_ui_requests(&self) -> Option<mpsc::Receiver<HostUiRequest>> {
        self.inner.client.take_ui_requests()
    }

    /// Answer a correlated host-initiated dialog request.
    ///
    /// # Errors
    ///
    /// Returns a transport error if the host has already exited.
    pub async fn respond_ui(&self, response: HostUiResponse) -> Result<(), HostClientError> {
        self.inner.client.respond_ui(response).await
    }

    /// Whether the host transport is live and the runner not invalidated.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.inner.active()
    }

    /// Claim the sole lossless receiver for extension session actions and requests.
    ///
    /// The session bridge task calls this exactly once per host instance.
    /// Subsequent callers receive `None`. While unclaimed, fire-and-forget
    /// actions are dropped and correlated requests receive their existing
    /// `no active session` fallback.
    #[must_use]
    pub(crate) fn take_session_bridge(&self) -> Option<mpsc::Receiver<SessionBridgeEvent>> {
        let receiver = self
            .inner
            .session_bridge_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if receiver.is_some() {
            self.inner
                .session_bridge_claimed
                .store(true, Ordering::Release);
        }
        receiver
    }

    /// Claim the sole receiver for live provider registry updates.
    ///
    /// The facade relay calls this exactly once per host instance.
    #[must_use]
    pub(crate) fn take_providers_updates(
        &self,
    ) -> Option<watch::Receiver<pi_ext::protocol::ProvidersUpdate>> {
        self.client().take_providers_updates()
    }

    /// Install the lower-layer callback invoked when a token-bearing
    /// replacement event is dropped at either host bounded-send hop. The
    /// callback receives the facade token and endpoint. Called from
    /// [`ExtensionRuntimeSet::start_bridges`] before any relay begins.
    pub(crate) fn set_replacement_drop_handler(&self, handler: ReplacementDropHandler) {
        *self
            .inner
            .replacement_drop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    pub(crate) fn set_endpoint_id(&self, endpoint: EndpointId) {
        *self
            .inner
            .endpoint_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(endpoint);
    }

    /// Answer a correlated `session.setModel` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_set_model(
        &self,
        id: BridgeRequestId,
        success: bool,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_set_model(id.0, success)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.compact` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_compact(
        &self,
        id: BridgeRequestId,
        outcome: Result<Value, String>,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_compact(id.0, outcome)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.newSession` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_new_session(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_new_session(id.0, cancelled, token)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.fork` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_fork(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        selected_text: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_fork(id.0, cancelled, selected_text, token)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.navigateTree` request.
    ///
    /// Converts the product [`NavigateTreeResult`] into the wire
    /// [`protocol::SessionNavigateTreeResponse`] at the seam.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_navigate_tree(
        &self,
        id: BridgeRequestId,
        outcome: Result<NavigateTreeResult, String>,
    ) -> Result<(), ExtensionHostError> {
        let wire_outcome = outcome.map(|result| protocol::SessionNavigateTreeResponse {
            cancelled: result.cancelled,
            editor_text: result.editor_text,
            aborted: if result.aborted { Some(true) } else { None },
            summary_entry: result
                .summary_entry
                .and_then(|entry| serde_json::to_value(&entry).ok()),
        });
        self.inner
            .client
            .respond_navigate_tree(id.0, wire_outcome)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.switchSession` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_switch_session(
        &self,
        id: BridgeRequestId,
        cancelled: bool,
        token: Option<&str>,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_switch_session(id.0, cancelled, token)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.reload` request.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_reload(
        &self,
        id: BridgeRequestId,
        outcome: Result<Option<&str>, String>,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_reload(id.0, outcome)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer a correlated `session.setupEntries` request from the host.
    ///
    /// Wraps the product `Vec<Value>` into the wire
    /// [`protocol::SessionSetupEntriesResponse`] at the seam.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_setup_entries(
        &self,
        id: BridgeRequestId,
        outcome: Result<Vec<Value>, String>,
    ) -> Result<(), ExtensionHostError> {
        let wire_outcome = outcome.map(|entries| protocol::SessionSetupEntriesResponse { entries });
        self.inner
            .client
            .respond_setup_entries(id.0, wire_outcome)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Reject a ready-gated operation while another operation owns the facade slot.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_replacement_busy(
        &self,
        id: BridgeRequestId,
        method: BridgeMethod,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_replacement_busy(id.0, bridge_method_to_wire(method))
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Answer an unclaimed correlated session request without synthesizing cancellation.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtensionHostError`] if the host has already exited.
    pub async fn respond_session_error(
        &self,
        id: BridgeRequestId,
        method: BridgeMethod,
        message: &str,
    ) -> Result<(), ExtensionHostError> {
        self.inner
            .client
            .respond_session_error(id.0, bridge_method_to_wire(method), message)
            .await
            .map_err(ExtensionHostError::from)
    }

    /// Push the mirrored session state to the host (`session.update` event).
    ///
    /// Converts the product [`SessionState`] into the wire
    /// [`SessionStateWire`] at the seam. The host serves the synchronous
    /// `ExtensionActions` / context getters from the latest push. Host
    /// failures are isolated as a single non-retryable `extension_error`;
    /// the session survives.
    pub async fn push_session_state(&self, state: &SessionState) {
        if !self.inner.active() {
            return;
        }
        let wire = session_state_to_wire(state);
        let payload = match serde_json::to_value(&wire) {
            Ok(payload) => payload,
            Err(error) => {
                self.inner.publish_error(
                    "extension_protocol",
                    &format!("encode session.update: {error}"),
                    None,
                );
                return;
            }
        };
        if let Err(error) = self
            .inner
            .client
            .send_event(protocol::SESSION_UPDATE_METHOD, payload)
            .await
        {
            self.inner.report_host_error(&error);
        }
    }

    /// Push mirrored UI state (editor text, tool expansion) to the host
    /// (`ui.state` event). Pushed at UI sync points, not per keystroke; the
    /// host serves `getEditorText` / `getToolsExpanded` from the latest push.
    pub async fn push_ui_state(&self, state: &UiStateWire) {
        if !self.inner.active() {
            return;
        }
        let payload = match serde_json::to_value(state) {
            Ok(payload) => payload,
            Err(error) => {
                self.inner.publish_error(
                    "extension_protocol",
                    &format!("encode ui.state: {error}"),
                    None,
                );
                return;
            }
        };
        if let Err(error) = self
            .inner
            .client
            .send_event(protocol::UI_STATE_METHOD, payload)
            .await
        {
            self.inner.report_host_error(&error);
        }
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

    /// Bump the reload generation, dispose every active slot, and reap the
    /// current host exactly once (reap-only; the session layer owns the
    /// typed `session_shutdown{reload}` emission before calling this). The
    /// caller re-creates the runner (via [`HostExtensionRunner::start`] /
    /// [`connect`](Self::connect)) for the clean registration pass. Returns
    /// the new generation.
    pub async fn reload(&self) -> u64 {
        let generation = self
            .inner
            .reload_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        Self::shutdown_once_with_inner(&self.inner).await;
        self.inner.stale.store(true, Ordering::Relaxed);
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
        self.restart_and_rewire_with(
            runtime,
            preserved_flags,
            |paths, cwd, project_trusted| async move {
                Self::start_with_cwd_and_trust(paths, cwd, project_trusted).await
            },
        )
        .await
    }

    async fn restart_and_rewire_with<F, Fut>(
        &self,
        runtime: &ModelRuntime,
        preserved_flags: HashMap<String, Value>,
        start: F,
    ) -> Result<Arc<Self>, HostStartError>
    where
        F: FnOnce(Vec<String>, String, bool) -> Fut,
        Fut: std::future::Future<Output = Result<Arc<Self>, HostStartError>>,
    {
        // 1. Drop old provider registrations before the new host binds.
        self.unregister_providers_from(runtime);
        // 2. Await old transport shutdown / process reap exactly once.
        let _ = self.reload().await;
        // 3. Spawn replacement host with the same paths + cwd + trust bit.
        let replacement = start(
            self.inner.extension_paths.clone(),
            self.inner.load_cwd.clone(),
            self.inner.project_trusted,
        )
        .await?;
        // 4. Restore flags before any replacement-host hook can run.
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
        replacement
            .apply_flag_values(&preserved_flags)
            .await
            .map_err(|error| HostStartError::FlagSync(error.to_string()))?;
        // 5. Re-register providers (sibling isolation on individual failures).
        let _ = replacement.register_providers_on(runtime);
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
        Self::start_with_cwd_and_trust(extension_paths, load_cwd, false).await
    }

    /// Resolve + spawn with an explicit load cwd and project-trust value.
    ///
    /// # Errors
    ///
    /// See [`HostExtensionRunner::start`].
    pub async fn start_with_cwd_and_trust(
        extension_paths: Vec<String>,
        load_cwd: impl Into<String>,
        project_trusted: bool,
    ) -> Result<Arc<Self>, HostStartError> {
        let spec = host::resolve_host()?;
        let client =
            Arc::new(HostClient::spawn(&spec).map_err(|e| HostStartError::Spawn(e.to_string()))?);
        let startup = Self::connect_with_cwd_and_trust(
            Arc::clone(&client),
            extension_paths,
            load_cwd,
            project_trusted,
            HOOK_TIMEOUT,
        )
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
        Self::shutdown_once_with_inner(&self.inner).await;
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
///
/// Three explicit input classes, drained biased in this order: lossless
/// correlated session requests, ordered session control, then lossy
/// notifications. Valid replacement traffic cannot exist before this runner is
/// published.
#[allow(clippy::too_many_lines)]
fn spawn_event_pump(inner: Arc<Inner>) {
    let mut notifications = inner.client.subscribe_notifications();
    let mut session_requests = inner.client.take_session_requests();
    let mut session_control = inner.client.take_session_control();
    tokio::spawn(async move {
        if !inner.client.is_running() {
            inner.disabled.store(true, Ordering::Relaxed);
            inner.publish_error("extension_closed", "extension host stream closed", None);
            HostExtensionRunner::shutdown_once_with_inner(&inner).await;
            return;
        }
        loop {
            tokio::select! {
                biased;
                // Lossless correlated session requests: every accepted request
                // must reach exactly one consumer or receive an explicit error
                // response, so this class drains first.
                request = async { match &mut session_requests {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }} => {
                    match request {
                        Some(HostSessionRequest::SetModel { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::SetModel {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::Compact { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::Compact {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::NewSession { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::NewSession {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::Fork { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::Fork {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::NavigateTree { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::NavigateTree {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::SwitchSession { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::SwitchSession {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::Reload { id }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::Reload {
                                    id: BridgeRequestId(id),
                                },
                            )
                            .await;
                        }
                        Some(HostSessionRequest::SetupEntries { id, request }) => {
                            forward_session_bridge(
                                &inner,
                                SessionBridgeEvent::SetupEntries {
                                    id: BridgeRequestId(id),
                                    request: request.into(),
                                    origin: inner.endpoint_id(),
                                },
                            )
                            .await;
                        }
                        None => {
                            // Channel closed (client gone); park this class.
                            session_requests = None;
                        }
                    }
                }
                // Ordered session control: must not be lost or reordered.
                control = async { match &mut session_control {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }} => {
                    match control {
                        Some(event) => deliver_session_control(&inner, event),
                        None => {
                            session_control = None;
                        }
                    }
                }
                notification = notifications.recv() => match notification {
                    Ok(HostNotification::Notify(notification)) => {
                        inner.notify_send(notification.into());
                    }
                    Ok(HostNotification::ThemeSet(set)) => {
                        // Exactly-one-of violation: dropped at the seam, as
                        // the mode did before.
                        if let Ok(request) = ExtensionThemeRequest::try_from(set) {
                            inner.theme_set_send(request);
                        }
                    }
                    Ok(HostNotification::UiControl(control)) => {
                        inner.ui_control_send(control.into());
                    }
                    Ok(HostNotification::UiSlot(slot)) => {
                        forward_slot(&inner, &slot);
                    }
                    Ok(HostNotification::DisposeSlot(d)) => {
                        forward_dispose(&inner, &d);
                    }
                    Ok(HostNotification::ToolUpdate(update)) => {
                        let _ = inner.tool_updates_tx.send(update);
                    }
                    Ok(HostNotification::ProviderEvent(event)) => {
                        let _ = inner.provider_events_tx.send(event);
                    }
                    Ok(HostNotification::ExtensionError(event)) => {
                        let _ = inner.errors_tx.send(event);
                    }
                    Ok(HostNotification::Raw(frame)) => {
                        inner.disabled.store(true, Ordering::Relaxed);
                        inner.publish_error(
                            "extension_protocol",
                            &format!("unhandled host frame: {} {}", frame.kind, frame.method),
                            None,
                        );
                        HostExtensionRunner::shutdown_once_with_inner(&inner).await;
                        break;
                    }
                    Ok(HostNotification::Eof) => {
                        // Host stdout closed: fatal. Disable once, report, then
                        // shut down / reap the transport exactly once before exit.
                        inner.disabled.store(true, Ordering::Relaxed);
                        inner
                            .publish_error("extension_closed", "extension host stream closed", None);
                        HostExtensionRunner::shutdown_once_with_inner(&inner).await;
                        break;
                    }
                    Ok(HostNotification::ProtocolError(message)) => {
                        inner.disabled.store(true, Ordering::Relaxed);
                        inner.publish_error("extension_protocol", &message, None);
                        HostExtensionRunner::shutdown_once_with_inner(&inner).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        inner.publish_error(
                            "extension_event_lagged",
                            &format!("dropped {skipped} extension host events"),
                            None,
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
}

/// Route ordered session control synchronously into the bounded bridge.
fn deliver_session_control(inner: &Arc<Inner>, event: HostSessionControlEvent) {
    let origin = inner.endpoint_id();
    let event = match event {
        HostSessionControlEvent::Command(envelope) => SessionBridgeEvent::Command {
            envelope: envelope.into(),
            origin,
        },
        HostSessionControlEvent::ReplacementReady { token } => {
            SessionBridgeEvent::ReplacementReady { token, origin }
        }
        HostSessionControlEvent::ReplacementAbort { token } => {
            SessionBridgeEvent::ReplacementAbort { token, origin }
        }
    };
    let claimed = inner.active() && inner.session_bridge_claimed.load(Ordering::Acquire);
    let undelivered = if claimed {
        inner
            .session_bridge_tx
            .try_send(event)
            .err()
            .map(mpsc::error::TrySendError::into_inner)
    } else {
        Some(event)
    };
    finish_direct_session_control_delivery(inner, undelivered);
}

fn finish_direct_session_control_delivery(
    inner: &Arc<Inner>,
    undelivered: Option<SessionBridgeEvent>,
) {
    let Some(event) = undelivered else {
        return;
    };
    if handle_dropped_session_control(inner, &event) {
        return;
    }
    inner.publish_error(
        "extension_protocol",
        "correlated event reached the direct session-control route",
        None,
    );
}

fn handle_dropped_session_control(inner: &Arc<Inner>, event: &SessionBridgeEvent) -> bool {
    match event {
        SessionBridgeEvent::Command { envelope, origin } => {
            if let Some(token) = envelope.replacement_token.as_deref() {
                handle_dropped_replacement_event(inner, token, *origin);
            }
            true
        }
        SessionBridgeEvent::ReplacementReady { token, origin }
        | SessionBridgeEvent::ReplacementAbort { token, origin } => {
            handle_dropped_replacement_event(inner, token, *origin);
            true
        }
        _ => false,
    }
}

fn handle_dropped_replacement_event(inner: &Arc<Inner>, token: &str, origin: Option<EndpointId>) {
    inner.publish_error(
        "extension_replacement_dropped",
        &format!(
            "replacement bridge event dropped for token {token}: session bridge unavailable or full"
        ),
        None,
    );
    let handler = inner
        .replacement_drop
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(handler) = handler {
        handler(token, origin);
    }
}

/// Route one session-bridge item to the claiming session task. Unclaimed,
/// closed, or FULL bridges answer correlated requests immediately (so the
/// host's awaiting extension never hangs) and drop fire-and-forget commands.
/// `try_send` keeps the pump from blocking behind a stalled drain task: the
/// drain task may itself await a hook response only the pump can read, so an
/// awaited send here could deadlock the transport.
async fn forward_session_bridge(inner: &Arc<Inner>, event: SessionBridgeEvent) {
    let claimed = inner.active() && inner.session_bridge_claimed.load(Ordering::Acquire);
    let undelivered = if claimed {
        inner
            .session_bridge_tx
            .try_send(event)
            .err()
            .map(mpsc::error::TrySendError::into_inner)
    } else {
        Some(event)
    };
    match undelivered {
        None => {}
        Some(
            event @ (SessionBridgeEvent::Command { .. }
            | SessionBridgeEvent::ReplacementReady { .. }
            | SessionBridgeEvent::ReplacementAbort { .. }),
        ) => {
            let handled = handle_dropped_session_control(inner, &event);
            debug_assert!(handled);
        }
        Some(SessionBridgeEvent::SetupEntries { id, .. }) => {
            let _ = inner
                .client
                .respond_setup_entries(id.0, Err("no active session".to_owned()))
                .await;
        }
        Some(SessionBridgeEvent::SetModel { id, .. }) => {
            let _ = inner.client.respond_set_model(id.0, false).await;
        }
        Some(SessionBridgeEvent::Compact { id, .. }) => {
            let _ = inner
                .client
                .respond_compact(id.0, Err("no active session".to_owned()))
                .await;
        }
        Some(SessionBridgeEvent::NewSession { id, .. }) => {
            let _ = inner
                .client
                .respond_session_error(
                    id.0,
                    protocol::SESSION_NEW_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        Some(SessionBridgeEvent::Fork { id, .. }) => {
            let _ = inner
                .client
                .respond_session_error(id.0, protocol::SESSION_FORK_METHOD, "no active session")
                .await;
        }
        Some(SessionBridgeEvent::NavigateTree { id, .. }) => {
            let _ = inner
                .client
                .respond_navigate_tree(id.0, Err("no active session".to_owned()))
                .await;
        }
        Some(SessionBridgeEvent::SwitchSession { id, .. }) => {
            let _ = inner
                .client
                .respond_session_error(
                    id.0,
                    protocol::SESSION_SWITCH_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        Some(SessionBridgeEvent::Reload { id }) => {
            let _ = inner
                .client
                .respond_reload(id.0, Err("no active session".to_owned()))
                .await;
        }
    }
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

/// Assistant metadata without the growing `content` array.
fn compact_assistant_meta(message: &AssistantMessage) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(object) = &mut value {
        object.remove("content");
    }
    value
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

    fn emit_message_update_delta<'a>(
        &'a self,
        event: &'a AssistantMessageEvent,
    ) -> BoxFuture<
        'a,
        Result<Option<CancelResult>, super::agent_session::extension_runner::ExtensionRunnerError>,
    > {
        let inner = Arc::clone(&self.inner);
        let payload = serde_json::json!({
            "type": MESSAGE_UPDATE_DELTA_METHOD,
            "event": compact_message_update_event(event),
        });
        Box::pin(async move {
            if !inner.has_handlers("message_update") {
                return Ok(None);
            }
            match inner
                .hook_request(MESSAGE_UPDATE_DELTA_METHOD, payload)
                .await
            {
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
                Err(error) => {
                    inner.report_host_error(&error);
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
}

impl HostExtensionRunner {
    async fn shutdown_once_with_inner(inner: &Arc<Inner>) {
        let _guard = inner.shutdown_lock.lock().await;
        if inner.shutdown_done.load(Ordering::Relaxed) {
            return;
        }
        Self::reap_inner(inner).await;
        inner.shutdown_done.store(true, Ordering::Relaxed);
    }

    async fn reap_inner(inner: &Arc<Inner>) {
        // Order matters for the slot_send/slot_dispose teardown gate: the
        // flag must be set before dispose_all_slots takes the slots lock.
        inner.disabled.store(true, Ordering::Relaxed);
        inner.dispose_all_slots();
        let _ = inner.client.shutdown().await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
