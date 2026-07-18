//! RPC protocol wire types for headless operation.
//!
//! Port of `.references/pi/packages/coding-agent/src/modes/rpc/rpc-types.ts`.
//!
//! Commands arrive as JSON lines on stdin. Responses, extension UI requests, and
//! agent events leave as JSON lines on stdout. Unknown command discriminants are
//! retained as [`RpcCommand::Unknown`] so the server can echo `id` + `type`
//! without a serde hard-fail.

use pi_agent::{AgentMessage, QueueMode};
use pi_ai::{ImageContent, Model, ModelThinkingLevel};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::compaction::CompactionResult;
use crate::core::resources::{SourceInfo, SourceOrigin, SourceScope};
use crate::core::sessions::SessionEntry;

// ---------------------------------------------------------------------------
// Local payload types not yet owned by product-core modules
// ---------------------------------------------------------------------------

/// Bash execution result returned by the `bash` RPC command.
///
/// Matches `.references/pi/packages/coding-agent/src/core/bash-executor.ts`
/// `BashResult`. Defined here until the product bash-executor surface lands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    /// Combined stdout + stderr (sanitized, possibly truncated).
    pub output: String,
    /// Process exit code (`None` when killed/cancelled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether the command was cancelled via signal.
    pub cancelled: bool,
    /// Whether the output was truncated.
    pub truncated: bool,
    /// Path to a spill file holding full output when truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Context-window usage snapshot embedded in [`SessionStats`].
///
/// Matches `ContextUsage` from coding-agent `extensions/types.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Estimated context tokens, or `null` when unknown (e.g. right after compaction).
    #[serde(default)]
    pub tokens: Option<u64>,
    /// Model context-window size.
    pub context_window: u64,
    /// Usage as a percentage of the context window, or `null` when tokens unknown.
    #[serde(default)]
    pub percent: Option<f64>,
}

/// Token counters inside [`SessionStats`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatsTokens {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Total tokens.
    pub total: u64,
}

/// Session statistics for the `get_session_stats` RPC command.
///
/// Matches `SessionStats` from coding-agent `agent-session.ts`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    /// Absolute session file path, when persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Count of user messages.
    pub user_messages: u64,
    /// Count of assistant messages.
    pub assistant_messages: u64,
    /// Count of tool-call content blocks.
    pub tool_calls: u64,
    /// Count of tool-result messages.
    pub tool_results: u64,
    /// Total messages in the session.
    pub total_messages: u64,
    /// Aggregated token usage.
    pub tokens: SessionStatsTokens,
    /// Aggregated cost.
    pub cost: f64,
    /// Optional context-window usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
}

/// Tree node returned by `get_tree`.
///
/// Mirrors `SessionTreeNode` from coding-agent `session-manager.ts`. Product
/// `SessionTreeNode` is not yet `Serialize`; this wire-facing twin reuses
/// [`SessionEntry`] which already round-trips JSONL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionTreeNode {
    /// Entry at this node.
    pub entry: SessionEntry,
    /// Children sorted by timestamp ascending.
    pub children: Vec<RpcSessionTreeNode>,
    /// Resolved label for this entry, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Timestamp of the latest label change for this entry, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_timestamp: Option<String>,
}

/// Wire-facing [`SourceInfo`] with serde (product `SourceInfo` is not yet serde).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSourceInfo {
    /// Absolute (or synthetic) path of the resource.
    pub path: String,
    /// Source label (`local`, `auto`, `cli`, package id, …).
    pub source: String,
    /// Scope relative to the project boundary.
    pub scope: RpcSourceScope,
    /// Package vs top-level origin.
    pub origin: RpcSourceOrigin,
    /// Optional base directory used for relative resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl From<&SourceInfo> for RpcSourceInfo {
    fn from(value: &SourceInfo) -> Self {
        Self {
            path: value.path.clone(),
            source: value.source.clone(),
            scope: RpcSourceScope::from(value.scope),
            origin: RpcSourceOrigin::from(value.origin),
            base_dir: value.base_dir.clone(),
        }
    }
}

impl From<SourceInfo> for RpcSourceInfo {
    fn from(value: SourceInfo) -> Self {
        Self::from(&value)
    }
}

/// Wire discriminant for [`SourceScope`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcSourceScope {
    /// Global agent directory.
    User,
    /// Project-local.
    Project,
    /// Temporary/CLI or synthetic.
    Temporary,
}

impl From<SourceScope> for RpcSourceScope {
    fn from(value: SourceScope) -> Self {
        match value {
            SourceScope::User => Self::User,
            SourceScope::Project => Self::Project,
            SourceScope::Temporary => Self::Temporary,
        }
    }
}

/// Wire discriminant for [`SourceOrigin`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RpcSourceOrigin {
    /// Installed or local package root.
    #[serde(rename = "package")]
    Package,
    /// Settings array, auto-discovery, or CLI temporary path.
    #[serde(rename = "top-level")]
    TopLevel,
}

impl From<SourceOrigin> for RpcSourceOrigin {
    fn from(value: SourceOrigin) -> Self {
        match value {
            SourceOrigin::Package => Self::Package,
            SourceOrigin::TopLevel => Self::TopLevel,
        }
    }
}

/// Streaming behavior for a prompt that arrives while the agent is busy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    /// Inject as a steering message.
    Steer,
    /// Queue as a follow-up after the current turn.
    FollowUp,
}

// ---------------------------------------------------------------------------
// RpcCommand (stdin)
// ---------------------------------------------------------------------------

/// All 31 known RPC commands, plus an unknown catch-all.
///
/// Each known variant carries an optional correlation `id`. The unknown arm
/// preserves the raw `type` string and remaining fields so the server can echo
/// `id`/`type` on error without serde hard-failing the line.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcCommand {
    /// Submit a user prompt (async — events follow; response at preflight).
    Prompt {
        /// Correlation id.
        id: Option<String>,
        /// Prompt text.
        message: String,
        /// Optional inline images.
        images: Option<Vec<ImageContent>>,
        /// How to handle the prompt while streaming.
        streaming_behavior: Option<StreamingBehavior>,
    },
    /// Steer into the current turn.
    Steer {
        /// Correlation id.
        id: Option<String>,
        /// Steering text.
        message: String,
        /// Optional inline images.
        images: Option<Vec<ImageContent>>,
    },
    /// Queue a follow-up after the current turn.
    FollowUp {
        /// Correlation id.
        id: Option<String>,
        /// Follow-up text.
        message: String,
        /// Optional inline images.
        images: Option<Vec<ImageContent>>,
    },
    /// Abort the current agent turn.
    Abort {
        /// Correlation id.
        id: Option<String>,
    },
    /// Start a new session, optionally forked from a parent session file.
    NewSession {
        /// Correlation id.
        id: Option<String>,
        /// Optional parent session path.
        parent_session: Option<String>,
    },
    /// Snapshot current session state.
    GetState {
        /// Correlation id.
        id: Option<String>,
    },
    /// Select a model by provider + id.
    SetModel {
        /// Correlation id.
        id: Option<String>,
        /// Provider identifier.
        provider: String,
        /// Model identifier.
        model_id: String,
    },
    /// Cycle to the next available model.
    CycleModel {
        /// Correlation id.
        id: Option<String>,
    },
    /// List available models.
    GetAvailableModels {
        /// Correlation id.
        id: Option<String>,
    },
    /// Set the reasoning/thinking level.
    SetThinkingLevel {
        /// Correlation id.
        id: Option<String>,
        /// Target level (includes `off`).
        level: ModelThinkingLevel,
    },
    /// Cycle to the next thinking level.
    CycleThinkingLevel {
        /// Correlation id.
        id: Option<String>,
    },
    /// Set the steering queue drain mode.
    SetSteeringMode {
        /// Correlation id.
        id: Option<String>,
        /// Drain mode.
        mode: QueueMode,
    },
    /// Set the follow-up queue drain mode.
    SetFollowUpMode {
        /// Correlation id.
        id: Option<String>,
        /// Drain mode.
        mode: QueueMode,
    },
    /// Compact the session.
    Compact {
        /// Correlation id.
        id: Option<String>,
        /// Optional custom instructions for the summarizer.
        custom_instructions: Option<String>,
    },
    /// Enable or disable auto-compaction.
    SetAutoCompaction {
        /// Correlation id.
        id: Option<String>,
        /// Whether auto-compaction is enabled.
        enabled: bool,
    },
    /// Enable or disable auto-retry.
    SetAutoRetry {
        /// Correlation id.
        id: Option<String>,
        /// Whether auto-retry is enabled.
        enabled: bool,
    },
    /// Abort an in-flight auto-retry.
    AbortRetry {
        /// Correlation id.
        id: Option<String>,
    },
    /// Execute a bash command via the session.
    Bash {
        /// Correlation id.
        id: Option<String>,
        /// Shell command.
        command: String,
        /// When true, exclude output from model context.
        exclude_from_context: Option<bool>,
    },
    /// Abort a running bash command.
    AbortBash {
        /// Correlation id.
        id: Option<String>,
    },
    /// Return session statistics.
    GetSessionStats {
        /// Correlation id.
        id: Option<String>,
    },
    /// Export the session to HTML.
    ExportHtml {
        /// Correlation id.
        id: Option<String>,
        /// Optional output path.
        output_path: Option<String>,
    },
    /// Switch to another session file.
    SwitchSession {
        /// Correlation id.
        id: Option<String>,
        /// Path of the session to open.
        session_path: String,
    },
    /// Fork the session before `entry_id`.
    Fork {
        /// Correlation id.
        id: Option<String>,
        /// Entry id to fork before.
        entry_id: String,
    },
    /// Clone the session at the current leaf.
    Clone {
        /// Correlation id.
        id: Option<String>,
    },
    /// List user messages available for forking.
    GetForkMessages {
        /// Correlation id.
        id: Option<String>,
    },
    /// List session entries, optionally after `since`.
    GetEntries {
        /// Correlation id.
        id: Option<String>,
        /// Optional entry id; returns entries strictly after this id.
        since: Option<String>,
    },
    /// Return the session tree.
    GetTree {
        /// Correlation id.
        id: Option<String>,
    },
    /// Return the last assistant text content, if any.
    GetLastAssistantText {
        /// Correlation id.
        id: Option<String>,
    },
    /// Set the display name of the current session.
    SetSessionName {
        /// Correlation id.
        id: Option<String>,
        /// New session name.
        name: String,
    },
    /// Return all agent messages in the current session.
    GetMessages {
        /// Correlation id.
        id: Option<String>,
    },
    /// List available slash commands (extension/prompt/skill).
    GetCommands {
        /// Correlation id.
        id: Option<String>,
    },
    /// Unknown command discriminant preserved for error echo.
    Unknown {
        /// Correlation id when present.
        id: Option<String>,
        /// Raw `type` string from the wire.
        command_type: String,
        /// Remaining fields (excluding `type` and `id`).
        payload: Map<String, Value>,
    },
}

impl RpcCommand {
    /// Optional correlation id shared by every command variant.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Prompt { id, .. }
            | Self::Steer { id, .. }
            | Self::FollowUp { id, .. }
            | Self::Abort { id }
            | Self::NewSession { id, .. }
            | Self::GetState { id }
            | Self::SetModel { id, .. }
            | Self::CycleModel { id }
            | Self::GetAvailableModels { id }
            | Self::SetThinkingLevel { id, .. }
            | Self::CycleThinkingLevel { id }
            | Self::SetSteeringMode { id, .. }
            | Self::SetFollowUpMode { id, .. }
            | Self::Compact { id, .. }
            | Self::SetAutoCompaction { id, .. }
            | Self::SetAutoRetry { id, .. }
            | Self::AbortRetry { id }
            | Self::Bash { id, .. }
            | Self::AbortBash { id }
            | Self::GetSessionStats { id }
            | Self::ExportHtml { id, .. }
            | Self::SwitchSession { id, .. }
            | Self::Fork { id, .. }
            | Self::Clone { id }
            | Self::GetForkMessages { id }
            | Self::GetEntries { id, .. }
            | Self::GetTree { id }
            | Self::GetLastAssistantText { id }
            | Self::SetSessionName { id, .. }
            | Self::GetMessages { id }
            | Self::GetCommands { id }
            | Self::Unknown { id, .. } => id.as_deref(),
        }
    }

    /// Wire `type` discriminant for this command.
    #[must_use]
    pub fn command_type(&self) -> &str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Steer { .. } => "steer",
            Self::FollowUp { .. } => "follow_up",
            Self::Abort { .. } => "abort",
            Self::NewSession { .. } => "new_session",
            Self::GetState { .. } => "get_state",
            Self::SetModel { .. } => "set_model",
            Self::CycleModel { .. } => "cycle_model",
            Self::GetAvailableModels { .. } => "get_available_models",
            Self::SetThinkingLevel { .. } => "set_thinking_level",
            Self::CycleThinkingLevel { .. } => "cycle_thinking_level",
            Self::SetSteeringMode { .. } => "set_steering_mode",
            Self::SetFollowUpMode { .. } => "set_follow_up_mode",
            Self::Compact { .. } => "compact",
            Self::SetAutoCompaction { .. } => "set_auto_compaction",
            Self::SetAutoRetry { .. } => "set_auto_retry",
            Self::AbortRetry { .. } => "abort_retry",
            Self::Bash { .. } => "bash",
            Self::AbortBash { .. } => "abort_bash",
            Self::GetSessionStats { .. } => "get_session_stats",
            Self::ExportHtml { .. } => "export_html",
            Self::SwitchSession { .. } => "switch_session",
            Self::Fork { .. } => "fork",
            Self::Clone { .. } => "clone",
            Self::GetForkMessages { .. } => "get_fork_messages",
            Self::GetEntries { .. } => "get_entries",
            Self::GetTree { .. } => "get_tree",
            Self::GetLastAssistantText { .. } => "get_last_assistant_text",
            Self::SetSessionName { .. } => "set_session_name",
            Self::GetMessages { .. } => "get_messages",
            Self::GetCommands { .. } => "get_commands",
            Self::Unknown { command_type, .. } => command_type.as_str(),
        }
    }
}

impl Serialize for RpcCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_rpc_command(self, serializer)
    }
}

fn serialize_rpc_command<S: Serializer>(
    command: &RpcCommand,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match command {
        RpcCommand::Prompt {
            id,
            message,
            images,
            streaming_behavior,
        } => serialize_prompt(
            serializer,
            id.as_deref(),
            message,
            images.as_deref(),
            *streaming_behavior,
        ),
        RpcCommand::Steer {
            id,
            message,
            images,
        } => serialize_message_images(
            serializer,
            id.as_deref(),
            "steer",
            message,
            images.as_deref(),
        ),
        RpcCommand::FollowUp {
            id,
            message,
            images,
        } => serialize_message_images(
            serializer,
            id.as_deref(),
            "follow_up",
            message,
            images.as_deref(),
        ),
        RpcCommand::Abort { id } => serialize_type_only(serializer, id.as_deref(), "abort"),
        RpcCommand::NewSession { id, parent_session } => {
            serialize_new_session(serializer, id.as_deref(), parent_session.as_deref())
        }
        RpcCommand::GetState { id } => serialize_type_only(serializer, id.as_deref(), "get_state"),
        RpcCommand::SetModel {
            id,
            provider,
            model_id,
        } => serialize_set_model(serializer, id.as_deref(), provider, model_id),
        RpcCommand::CycleModel { id } => {
            serialize_type_only(serializer, id.as_deref(), "cycle_model")
        }
        RpcCommand::GetAvailableModels { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_available_models")
        }
        RpcCommand::SetThinkingLevel { id, level } => {
            serialize_set_thinking_level(serializer, id.as_deref(), *level)
        }
        RpcCommand::CycleThinkingLevel { id } => {
            serialize_type_only(serializer, id.as_deref(), "cycle_thinking_level")
        }
        RpcCommand::SetSteeringMode { id, mode } => {
            serialize_queue_mode(serializer, id.as_deref(), "set_steering_mode", *mode)
        }
        RpcCommand::SetFollowUpMode { id, mode } => {
            serialize_queue_mode(serializer, id.as_deref(), "set_follow_up_mode", *mode)
        }
        other => serialize_rpc_command_rest(other, serializer),
    }
}

fn serialize_rpc_command_rest<S: Serializer>(
    command: &RpcCommand,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match command {
        RpcCommand::Compact {
            id,
            custom_instructions,
        } => serialize_compact(serializer, id.as_deref(), custom_instructions.as_deref()),
        RpcCommand::SetAutoCompaction { id, enabled } => {
            serialize_enabled(serializer, id.as_deref(), "set_auto_compaction", *enabled)
        }
        RpcCommand::SetAutoRetry { id, enabled } => {
            serialize_enabled(serializer, id.as_deref(), "set_auto_retry", *enabled)
        }
        RpcCommand::AbortRetry { id } => {
            serialize_type_only(serializer, id.as_deref(), "abort_retry")
        }
        RpcCommand::Bash {
            id,
            command,
            exclude_from_context,
        } => serialize_bash(serializer, id.as_deref(), command, *exclude_from_context),
        RpcCommand::AbortBash { id } => {
            serialize_type_only(serializer, id.as_deref(), "abort_bash")
        }
        RpcCommand::GetSessionStats { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_session_stats")
        }
        RpcCommand::ExportHtml { id, output_path } => {
            serialize_export_html(serializer, id.as_deref(), output_path.as_deref())
        }
        RpcCommand::SwitchSession { id, session_path } => {
            serialize_switch_session(serializer, id.as_deref(), session_path)
        }
        RpcCommand::Fork { id, entry_id } => serialize_fork(serializer, id.as_deref(), entry_id),
        RpcCommand::Clone { id } => serialize_type_only(serializer, id.as_deref(), "clone"),
        RpcCommand::GetForkMessages { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_fork_messages")
        }
        RpcCommand::GetEntries { id, since } => {
            serialize_get_entries(serializer, id.as_deref(), since.as_deref())
        }
        RpcCommand::GetTree { id } => serialize_type_only(serializer, id.as_deref(), "get_tree"),
        RpcCommand::GetLastAssistantText { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_last_assistant_text")
        }
        RpcCommand::SetSessionName { id, name } => {
            serialize_set_session_name(serializer, id.as_deref(), name)
        }
        RpcCommand::GetMessages { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_messages")
        }
        RpcCommand::GetCommands { id } => {
            serialize_type_only(serializer, id.as_deref(), "get_commands")
        }
        RpcCommand::Unknown {
            id,
            command_type,
            payload,
        } => serialize_unknown(serializer, id.as_deref(), command_type, payload),
        _ => serialize_type_only(serializer, None, "abort"),
    }
}

fn serialize_prompt<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    message: &str,
    images: Option<&[ImageContent]>,
    streaming_behavior: Option<StreamingBehavior>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "prompt")?;
    map.serialize_entry("message", message)?;
    if let Some(images) = images {
        map.serialize_entry("images", images)?;
    }
    if let Some(behavior) = streaming_behavior {
        map.serialize_entry("streamingBehavior", &behavior)?;
    }
    map.end()
}

fn serialize_message_images<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    type_name: &str,
    message: &str,
    images: Option<&[ImageContent]>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", type_name)?;
    map.serialize_entry("message", message)?;
    if let Some(images) = images {
        map.serialize_entry("images", images)?;
    }
    map.end()
}

fn serialize_new_session<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    parent_session: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "new_session")?;
    if let Some(parent) = parent_session {
        map.serialize_entry("parentSession", parent)?;
    }
    map.end()
}

fn serialize_set_model<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    provider: &str,
    model_id: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "set_model")?;
    map.serialize_entry("provider", provider)?;
    map.serialize_entry("modelId", model_id)?;
    map.end()
}

fn serialize_set_thinking_level<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    level: ModelThinkingLevel,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "set_thinking_level")?;
    map.serialize_entry("level", &level)?;
    map.end()
}

fn serialize_queue_mode<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    type_name: &str,
    mode: QueueMode,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", type_name)?;
    map.serialize_entry("mode", &mode)?;
    map.end()
}

fn serialize_compact<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    custom_instructions: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "compact")?;
    if let Some(custom) = custom_instructions {
        map.serialize_entry("customInstructions", custom)?;
    }
    map.end()
}

fn serialize_enabled<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    type_name: &str,
    enabled: bool,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", type_name)?;
    map.serialize_entry("enabled", &enabled)?;
    map.end()
}

fn serialize_bash<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    command: &str,
    exclude_from_context: Option<bool>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "bash")?;
    map.serialize_entry("command", command)?;
    if let Some(exclude) = exclude_from_context {
        map.serialize_entry("excludeFromContext", &exclude)?;
    }
    map.end()
}

fn serialize_export_html<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    output_path: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "export_html")?;
    if let Some(path) = output_path {
        map.serialize_entry("outputPath", path)?;
    }
    map.end()
}

fn serialize_switch_session<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    session_path: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "switch_session")?;
    map.serialize_entry("sessionPath", session_path)?;
    map.end()
}

fn serialize_fork<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    entry_id: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "fork")?;
    map.serialize_entry("entryId", entry_id)?;
    map.end()
}

fn serialize_get_entries<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    since: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "get_entries")?;
    if let Some(since) = since {
        map.serialize_entry("since", since)?;
    }
    map.end()
}

fn serialize_set_session_name<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    name: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", "set_session_name")?;
    map.serialize_entry("name", name)?;
    map.end()
}

fn serialize_unknown<S: Serializer>(
    serializer: S,
    id: Option<&str>,
    command_type: &str,
    payload: &Map<String, Value>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", command_type)?;
    for (key, value) in payload {
        map.serialize_entry(key, value)?;
    }
    map.end()
}

impl<'de> Deserialize<'de> for RpcCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| de::Error::custom("rpc command must be a JSON object"))?;
        let command_type = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("rpc command missing type"))?
            .to_owned();
        let id = optional_string(obj, "id");
        parse_known_command(obj, id, command_type).map_err(de::Error::custom)
    }
}

fn parse_known_command(
    obj: &Map<String, Value>,
    id: Option<String>,
    command_type: String,
) -> Result<RpcCommand, String> {
    match command_type.as_str() {
        "prompt" => parse_prompt(obj, id),
        "steer" => parse_message_images_cmd(obj, id, "steer"),
        "follow_up" => parse_message_images_cmd(obj, id, "follow_up"),
        "abort" => Ok(RpcCommand::Abort { id }),
        "new_session" => Ok(RpcCommand::NewSession {
            id,
            parent_session: optional_string(obj, "parentSession"),
        }),
        "get_state" => Ok(RpcCommand::GetState { id }),
        "set_model" => Ok(RpcCommand::SetModel {
            id,
            provider: required_string_owned(obj, "provider")?,
            model_id: required_string_owned(obj, "modelId")?,
        }),
        "cycle_model" => Ok(RpcCommand::CycleModel { id }),
        "get_available_models" => Ok(RpcCommand::GetAvailableModels { id }),
        "set_thinking_level" => parse_set_thinking_level(obj, id),
        "cycle_thinking_level" => Ok(RpcCommand::CycleThinkingLevel { id }),
        "set_steering_mode" => parse_queue_mode_cmd(obj, id, true),
        "set_follow_up_mode" => parse_queue_mode_cmd(obj, id, false),
        "compact" => Ok(RpcCommand::Compact {
            id,
            custom_instructions: optional_string(obj, "customInstructions"),
        }),
        "set_auto_compaction" => Ok(RpcCommand::SetAutoCompaction {
            id,
            enabled: required_bool_owned(obj, "enabled")?,
        }),
        "set_auto_retry" => Ok(RpcCommand::SetAutoRetry {
            id,
            enabled: required_bool_owned(obj, "enabled")?,
        }),
        "abort_retry" => Ok(RpcCommand::AbortRetry { id }),
        "bash" => Ok(RpcCommand::Bash {
            id,
            command: required_string_owned(obj, "command")?,
            exclude_from_context: optional_bool(obj, "excludeFromContext"),
        }),
        "abort_bash" => Ok(RpcCommand::AbortBash { id }),
        "get_session_stats" => Ok(RpcCommand::GetSessionStats { id }),
        "export_html" => Ok(RpcCommand::ExportHtml {
            id,
            output_path: optional_string(obj, "outputPath"),
        }),
        "switch_session" => Ok(RpcCommand::SwitchSession {
            id,
            session_path: required_string_owned(obj, "sessionPath")?,
        }),
        "fork" => Ok(RpcCommand::Fork {
            id,
            entry_id: required_string_owned(obj, "entryId")?,
        }),
        "clone" => Ok(RpcCommand::Clone { id }),
        "get_fork_messages" => Ok(RpcCommand::GetForkMessages { id }),
        "get_entries" => Ok(RpcCommand::GetEntries {
            id,
            since: optional_string(obj, "since"),
        }),
        "get_tree" => Ok(RpcCommand::GetTree { id }),
        "get_last_assistant_text" => Ok(RpcCommand::GetLastAssistantText { id }),
        "set_session_name" => Ok(RpcCommand::SetSessionName {
            id,
            name: required_string_owned(obj, "name")?,
        }),
        "get_messages" => Ok(RpcCommand::GetMessages { id }),
        "get_commands" => Ok(RpcCommand::GetCommands { id }),
        _ => Ok(parse_unknown_command(obj, id, command_type)),
    }
}

fn parse_prompt(obj: &Map<String, Value>, id: Option<String>) -> Result<RpcCommand, String> {
    let message = required_string_owned(obj, "message")?;
    let images = optional_images_owned(obj)?;
    let streaming_behavior = match obj.get("streamingBehavior") {
        None | Some(Value::Null) => None,
        Some(v) => Some(StreamingBehavior::deserialize(v).map_err(|e| e.to_string())?),
    };
    Ok(RpcCommand::Prompt {
        id,
        message,
        images,
        streaming_behavior,
    })
}

fn parse_message_images_cmd(
    obj: &Map<String, Value>,
    id: Option<String>,
    kind: &str,
) -> Result<RpcCommand, String> {
    let message = required_string_owned(obj, "message")?;
    let images = optional_images_owned(obj)?;
    match kind {
        "steer" => Ok(RpcCommand::Steer {
            id,
            message,
            images,
        }),
        _ => Ok(RpcCommand::FollowUp {
            id,
            message,
            images,
        }),
    }
}

fn parse_set_thinking_level(
    obj: &Map<String, Value>,
    id: Option<String>,
) -> Result<RpcCommand, String> {
    let level = obj
        .get("level")
        .ok_or_else(|| "set_thinking_level missing level".to_owned())?;
    let level = ModelThinkingLevel::deserialize(level).map_err(|e| e.to_string())?;
    Ok(RpcCommand::SetThinkingLevel { id, level })
}

fn parse_queue_mode_cmd(
    obj: &Map<String, Value>,
    id: Option<String>,
    steering: bool,
) -> Result<RpcCommand, String> {
    let mode = obj
        .get("mode")
        .ok_or_else(|| "queue mode command missing mode".to_owned())?;
    let mode = QueueMode::deserialize(mode).map_err(|e| e.to_string())?;
    if steering {
        Ok(RpcCommand::SetSteeringMode { id, mode })
    } else {
        Ok(RpcCommand::SetFollowUpMode { id, mode })
    }
}

fn parse_unknown_command(
    obj: &Map<String, Value>,
    id: Option<String>,
    command_type: String,
) -> RpcCommand {
    let mut payload = Map::new();
    for (key, value) in obj {
        if key == "type" || key == "id" {
            continue;
        }
        payload.insert(key.clone(), value.clone());
    }
    RpcCommand::Unknown {
        id,
        command_type,
        payload,
    }
}

fn required_string_owned(obj: &Map<String, Value>, key: &str) -> Result<String, String> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("field {key} must be a string, got {other}")),
        None => Err(format!("missing field {key}")),
    }
}

fn required_bool_owned(obj: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match obj.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("field {key} must be a boolean, got {other}")),
        None => Err(format!("missing field {key}")),
    }
}

fn optional_images_owned(obj: &Map<String, Value>) -> Result<Option<Vec<ImageContent>>, String> {
    match obj.get("images") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let images = Vec::<ImageContent>::deserialize(v).map_err(|e| e.to_string())?;
            Ok(Some(images))
        }
    }
}

// ---------------------------------------------------------------------------
// RpcSessionState / RpcSlashCommand
// ---------------------------------------------------------------------------

/// Snapshot returned by `get_state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionState {
    /// Active model, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    /// Current thinking level (includes `off`).
    pub thinking_level: ModelThinkingLevel,
    /// Whether the agent is currently streaming.
    pub is_streaming: bool,
    /// Whether compaction is in progress.
    pub is_compacting: bool,
    /// Steering queue drain mode.
    pub steering_mode: QueueMode,
    /// Follow-up queue drain mode.
    pub follow_up_mode: QueueMode,
    /// Absolute session file path, when persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Optional display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// Whether auto-compaction is enabled.
    pub auto_compaction_enabled: bool,
    /// Number of messages currently in the session.
    pub message_count: u64,
    /// Number of pending queued messages.
    pub pending_message_count: u64,
}

/// Kind of slash command exposed by `get_commands`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpcSlashCommandSource {
    /// Registered by a TypeScript extension.
    Extension,
    /// Prompt template.
    Prompt,
    /// Skill (`skill:{name}`).
    Skill,
}

/// A command available for invocation via prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSlashCommand {
    /// Command name (without leading slash).
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What kind of command this is.
    pub source: RpcSlashCommandSource,
    /// Source metadata for the owning resource.
    pub source_info: RpcSourceInfo,
}

// ---------------------------------------------------------------------------
// RpcResponse (stdout)
// ---------------------------------------------------------------------------

/// Typed success payload for each command that returns `data`.
///
/// Serialization is untagged (payload is the raw `data` object). Deserialization
/// of full responses is command-directed via [`RpcResponse`] so overlapping
/// shapes (e.g. `{cancelled}` vs bash `{cancelled,...}`) never collide.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RpcResponseData {
    /// `{ cancelled }` for `new_session` / `switch_session` / `clone`.
    Cancelled {
        /// Whether the user cancelled the operation.
        cancelled: bool,
    },
    /// Full session state.
    SessionState(RpcSessionState),
    /// Selected model from `set_model`.
    Model(Model),
    /// `cycle_model` result (or JSON `null`).
    CycleModel(Option<CycleModelData>),
    /// `{ models: [...] }` from `get_available_models`.
    AvailableModels {
        /// Available models.
        models: Vec<Model>,
    },
    /// `cycle_thinking_level` result (or JSON `null`).
    CycleThinkingLevel(Option<CycleThinkingLevelData>),
    /// Compaction result.
    Compaction(CompactionResult),
    /// Bash result.
    Bash(BashResult),
    /// Session stats.
    SessionStats(SessionStats),
    /// `{ path }` from `export_html`.
    ExportHtml {
        /// Written HTML path.
        path: String,
    },
    /// `{ text, cancelled }` from `fork`.
    Fork {
        /// Selected user text at the fork point.
        text: String,
        /// Whether the user cancelled.
        cancelled: bool,
    },
    /// `{ messages: [{ entryId, text }] }` from `get_fork_messages`.
    ForkMessages {
        /// Forkable user messages.
        messages: Vec<ForkMessage>,
    },
    /// `{ entries, leafId }` from `get_entries`.
    Entries {
        /// Session entries.
        entries: Vec<SessionEntry>,
        /// Current leaf id.
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    /// `{ tree, leafId }` from `get_tree`.
    Tree {
        /// Session tree.
        tree: Vec<RpcSessionTreeNode>,
        /// Current leaf id.
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    /// `{ text }` from `get_last_assistant_text` (text may be null).
    LastAssistantText {
        /// Last assistant text, or null.
        text: Option<String>,
    },
    /// `{ messages }` from `get_messages`.
    Messages {
        /// Agent messages.
        messages: Vec<AgentMessage>,
    },
    /// `{ commands }` from `get_commands`.
    Commands {
        /// Available slash commands.
        commands: Vec<RpcSlashCommand>,
    },
}

impl RpcResponseData {
    /// Deserialize a `data` payload using the echoed command discriminant.
    fn deserialize_for_command(command: &str, value: &Value) -> Result<Self, String> {
        match command {
            "new_session" | "switch_session" | "clone" => parse_cancelled_data(command, value),
            "get_state" => Ok(Self::SessionState(
                RpcSessionState::deserialize(value).map_err(|e| e.to_string())?,
            )),
            "set_model" => Ok(Self::Model(
                Model::deserialize(value).map_err(|e| e.to_string())?,
            )),
            "cycle_model" => parse_cycle_model_data(value),
            "get_available_models" => parse_available_models_data(value),
            "cycle_thinking_level" => parse_cycle_thinking_data(value),
            "compact" => Ok(Self::Compaction(
                CompactionResult::deserialize(value).map_err(|e| e.to_string())?,
            )),
            "bash" => Ok(Self::Bash(
                BashResult::deserialize(value).map_err(|e| e.to_string())?,
            )),
            "get_session_stats" => Ok(Self::SessionStats(
                SessionStats::deserialize(value).map_err(|e| e.to_string())?,
            )),
            "export_html" => parse_export_html_data(value),
            "fork" => parse_fork_data(value),
            "get_fork_messages" => parse_fork_messages_data(value),
            "get_entries" => parse_entries_data(value),
            "get_tree" => parse_tree_data(value),
            "get_last_assistant_text" => parse_last_assistant_text_data(value),
            "get_messages" => parse_messages_data(value),
            "get_commands" => parse_commands_data(value),
            other => Err(format!("no typed data parser for command {other}")),
        }
    }
}

fn parse_cancelled_data(command: &str, value: &Value) -> Result<RpcResponseData, String> {
    let cancelled = value
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{command} data missing cancelled"))?;
    Ok(RpcResponseData::Cancelled { cancelled })
}

fn parse_cycle_model_data(value: &Value) -> Result<RpcResponseData, String> {
    if value.is_null() {
        return Ok(RpcResponseData::CycleModel(None));
    }
    let data = CycleModelData::deserialize(value).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::CycleModel(Some(data)))
}

fn parse_available_models_data(value: &Value) -> Result<RpcResponseData, String> {
    let models = value
        .get("models")
        .ok_or_else(|| "get_available_models data missing models".to_owned())?;
    let models = Vec::<Model>::deserialize(models).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::AvailableModels { models })
}

fn parse_cycle_thinking_data(value: &Value) -> Result<RpcResponseData, String> {
    if value.is_null() {
        return Ok(RpcResponseData::CycleThinkingLevel(None));
    }
    let data = CycleThinkingLevelData::deserialize(value).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::CycleThinkingLevel(Some(data)))
}

fn parse_export_html_data(value: &Value) -> Result<RpcResponseData, String> {
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "export_html data missing path".to_owned())?
        .to_owned();
    Ok(RpcResponseData::ExportHtml { path })
}

fn parse_fork_data(value: &Value) -> Result<RpcResponseData, String> {
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "fork data missing text".to_owned())?
        .to_owned();
    let cancelled = value
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| "fork data missing cancelled".to_owned())?;
    Ok(RpcResponseData::Fork { text, cancelled })
}

fn parse_fork_messages_data(value: &Value) -> Result<RpcResponseData, String> {
    let messages = value
        .get("messages")
        .ok_or_else(|| "get_fork_messages data missing messages".to_owned())?;
    let messages = Vec::<ForkMessage>::deserialize(messages).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::ForkMessages { messages })
}

fn parse_leaf_id(value: &Value, command: &str) -> Result<Option<String>, String> {
    match value.get("leafId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("{command} leafId must be string|null, got {other}")),
    }
}

fn parse_entries_data(value: &Value) -> Result<RpcResponseData, String> {
    let entries = value
        .get("entries")
        .ok_or_else(|| "get_entries data missing entries".to_owned())?;
    let entries = Vec::<SessionEntry>::deserialize(entries).map_err(|e| e.to_string())?;
    let leaf_id = parse_leaf_id(value, "get_entries")?;
    Ok(RpcResponseData::Entries { entries, leaf_id })
}

fn parse_tree_data(value: &Value) -> Result<RpcResponseData, String> {
    let tree = value
        .get("tree")
        .ok_or_else(|| "get_tree data missing tree".to_owned())?;
    let tree = Vec::<RpcSessionTreeNode>::deserialize(tree).map_err(|e| e.to_string())?;
    let leaf_id = parse_leaf_id(value, "get_tree")?;
    Ok(RpcResponseData::Tree { tree, leaf_id })
}

fn parse_last_assistant_text_data(value: &Value) -> Result<RpcResponseData, String> {
    let text = match value.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err(format!(
                "get_last_assistant_text text must be string|null, got {other}"
            ));
        }
    };
    Ok(RpcResponseData::LastAssistantText { text })
}

fn parse_messages_data(value: &Value) -> Result<RpcResponseData, String> {
    let messages = value
        .get("messages")
        .ok_or_else(|| "get_messages data missing messages".to_owned())?;
    let messages = Vec::<AgentMessage>::deserialize(messages).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::Messages { messages })
}

fn parse_commands_data(value: &Value) -> Result<RpcResponseData, String> {
    let commands = value
        .get("commands")
        .ok_or_else(|| "get_commands data missing commands".to_owned())?;
    let commands = Vec::<RpcSlashCommand>::deserialize(commands).map_err(|e| e.to_string())?;
    Ok(RpcResponseData::Commands { commands })
}

/// Payload for a successful `cycle_model` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleModelData {
    /// Newly selected model.
    pub model: Model,
    /// Thinking level after the cycle.
    pub thinking_level: ModelThinkingLevel,
    /// Whether the model is scoped.
    pub is_scoped: bool,
}

/// Payload for a successful `cycle_thinking_level` response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleThinkingLevelData {
    /// New thinking level.
    pub level: ModelThinkingLevel,
}

/// One forkable user message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkMessage {
    /// Entry id.
    pub entry_id: String,
    /// User text.
    pub text: String,
}

/// Success or error response envelope (`type: "response"`).
#[derive(Clone, Debug, PartialEq)]
pub enum RpcResponse {
    /// Successful command response.
    Success {
        /// Correlation id.
        id: Option<String>,
        /// Command discriminant echoed back.
        command: String,
        /// Optional typed data payload (boxed: variants differ widely in size).
        data: Option<Box<RpcResponseData>>,
    },
    /// Failed command response.
    Error {
        /// Correlation id (echoed when known).
        id: Option<String>,
        /// Command discriminant or `"parse"`.
        command: String,
        /// Error message.
        error: String,
    },
}

impl RpcResponse {
    /// Build a success response with no data.
    #[must_use]
    pub fn ok(id: Option<String>, command: impl Into<String>) -> Self {
        Self::Success {
            id,
            command: command.into(),
            data: None,
        }
    }

    /// Build a success response with data.
    #[must_use]
    pub fn ok_data(id: Option<String>, command: impl Into<String>, data: RpcResponseData) -> Self {
        Self::Success {
            id,
            command: command.into(),
            data: Some(Box::new(data)),
        }
    }

    /// Build an error response.
    #[must_use]
    pub fn err(id: Option<String>, command: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Error {
            id,
            command: command.into(),
            error: error.into(),
        }
    }

    /// Correlation id.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Success { id, .. } | Self::Error { id, .. } => id.as_deref(),
        }
    }

    /// Echoed command discriminant.
    #[must_use]
    pub fn command(&self) -> &str {
        match self {
            Self::Success { command, .. } | Self::Error { command, .. } => command.as_str(),
        }
    }

    /// Whether this is a success response.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

impl Serialize for RpcResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Success { id, command, data } => {
                let mut map = serializer.serialize_map(None)?;
                serialize_id(&mut map, id.as_deref())?;
                map.serialize_entry("type", "response")?;
                map.serialize_entry("command", command)?;
                map.serialize_entry("success", &true)?;
                if let Some(data) = data {
                    map.serialize_entry("data", data.as_ref())?;
                }
                map.end()
            }
            Self::Error { id, command, error } => {
                let mut map = serializer.serialize_map(None)?;
                serialize_id(&mut map, id.as_deref())?;
                map.serialize_entry("type", "response")?;
                map.serialize_entry("command", command)?;
                map.serialize_entry("success", &false)?;
                map.serialize_entry("error", error)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RpcResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| de::Error::custom("rpc response must be a JSON object"))?;

        let type_field = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("rpc response missing type"))?;
        if type_field != "response" {
            return Err(de::Error::custom(format!(
                "expected type \"response\", got {type_field:?}"
            )));
        }

        let id = optional_string(obj, "id");
        let command = required_string(obj, "command")?;
        let success = obj
            .get("success")
            .and_then(Value::as_bool)
            .ok_or_else(|| de::Error::custom("rpc response missing success"))?;

        if success {
            let data = match obj.get("data") {
                None => None,
                Some(v) => Some(Box::new(
                    RpcResponseData::deserialize_for_command(command.as_str(), v)
                        .map_err(de::Error::custom)?,
                )),
            };
            Ok(Self::Success { id, command, data })
        } else {
            let error = required_string(obj, "error")?;
            Ok(Self::Error { id, command, error })
        }
    }
}

// ---------------------------------------------------------------------------
// Extension UI request / response
// ---------------------------------------------------------------------------

/// Notify severity for [`RpcExtensionUiRequest::Notify`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NotifyType {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// Widget placement for [`RpcExtensionUiRequest::SetWidget`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WidgetPlacement {
    /// Above the editor.
    AboveEditor,
    /// Below the editor.
    BelowEditor,
}

/// Extension UI request emitted on stdout (`type: "extension_ui_request"`).
#[derive(Clone, Debug, PartialEq)]
pub enum RpcExtensionUiRequest {
    /// Present a select list.
    Select {
        /// Correlation id.
        id: String,
        /// Dialog title.
        title: String,
        /// Options to choose from.
        options: Vec<String>,
        /// Optional timeout in milliseconds.
        timeout: Option<u64>,
    },
    /// Present a confirm dialog.
    Confirm {
        /// Correlation id.
        id: String,
        /// Dialog title.
        title: String,
        /// Dialog message.
        message: String,
        /// Optional timeout in milliseconds.
        timeout: Option<u64>,
    },
    /// Present a single-line input.
    Input {
        /// Correlation id.
        id: String,
        /// Dialog title.
        title: String,
        /// Optional placeholder.
        placeholder: Option<String>,
        /// Optional timeout in milliseconds.
        timeout: Option<u64>,
    },
    /// Present a multi-line editor.
    Editor {
        /// Correlation id.
        id: String,
        /// Dialog title.
        title: String,
        /// Optional prefilled text.
        prefill: Option<String>,
    },
    /// Fire-and-forget notification.
    Notify {
        /// Correlation id.
        id: String,
        /// Notification text.
        message: String,
        /// Optional severity.
        notify_type: Option<NotifyType>,
    },
    /// Set or clear a status key.
    SetStatus {
        /// Correlation id.
        id: String,
        /// Status key.
        status_key: String,
        /// Status text (`undefined` clears).
        status_text: Option<String>,
    },
    /// Set or clear a widget.
    SetWidget {
        /// Correlation id.
        id: String,
        /// Widget key.
        widget_key: String,
        /// Widget lines (`undefined` clears).
        widget_lines: Option<Vec<String>>,
        /// Optional placement.
        widget_placement: Option<WidgetPlacement>,
    },
    /// Set the window/tab title.
    SetTitle {
        /// Correlation id.
        id: String,
        /// New title.
        title: String,
    },
    /// Replace editor text.
    SetEditorText {
        /// Correlation id.
        id: String,
        /// New editor text.
        text: String,
    },
}

impl RpcExtensionUiRequest {
    /// Correlation id.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Select { id, .. }
            | Self::Confirm { id, .. }
            | Self::Input { id, .. }
            | Self::Editor { id, .. }
            | Self::Notify { id, .. }
            | Self::SetStatus { id, .. }
            | Self::SetWidget { id, .. }
            | Self::SetTitle { id, .. }
            | Self::SetEditorText { id, .. } => id.as_str(),
        }
    }

    /// Wire `method` discriminant.
    #[must_use]
    pub fn method(&self) -> &str {
        match self {
            Self::Select { .. } => "select",
            Self::Confirm { .. } => "confirm",
            Self::Input { .. } => "input",
            Self::Editor { .. } => "editor",
            Self::Notify { .. } => "notify",
            Self::SetStatus { .. } => "setStatus",
            Self::SetWidget { .. } => "setWidget",
            Self::SetTitle { .. } => "setTitle",
            Self::SetEditorText { .. } => "set_editor_text",
        }
    }
}

impl Serialize for RpcExtensionUiRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Select {
                id,
                title,
                options,
                timeout,
            } => serialize_ui_select(serializer, id, title, options, *timeout),
            Self::Confirm {
                id,
                title,
                message,
                timeout,
            } => serialize_ui_confirm(serializer, id, title, message, *timeout),
            Self::Input {
                id,
                title,
                placeholder,
                timeout,
            } => serialize_ui_input(serializer, id, title, placeholder.as_deref(), *timeout),
            Self::Editor { id, title, prefill } => {
                serialize_ui_editor(serializer, id, title, prefill.as_deref())
            }
            Self::Notify {
                id,
                message,
                notify_type,
            } => serialize_ui_notify(serializer, id, message, *notify_type),
            Self::SetStatus {
                id,
                status_key,
                status_text,
            } => serialize_ui_set_status(serializer, id, status_key, status_text.as_deref()),
            Self::SetWidget {
                id,
                widget_key,
                widget_lines,
                widget_placement,
            } => serialize_ui_set_widget(
                serializer,
                id,
                widget_key,
                widget_lines.as_deref(),
                *widget_placement,
            ),
            Self::SetTitle { id, title } => serialize_ui_set_title(serializer, id, title),
            Self::SetEditorText { id, text } => serialize_ui_set_editor_text(serializer, id, text),
        }
    }
}

fn serialize_ui_select<S: Serializer>(
    serializer: S,
    id: &str,
    title: &str,
    options: &[String],
    timeout: Option<u64>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "select")?;
    map.serialize_entry("title", title)?;
    map.serialize_entry("options", options)?;
    if let Some(timeout) = timeout {
        map.serialize_entry("timeout", &timeout)?;
    }
    map.end()
}

fn serialize_ui_confirm<S: Serializer>(
    serializer: S,
    id: &str,
    title: &str,
    message: &str,
    timeout: Option<u64>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "confirm")?;
    map.serialize_entry("title", title)?;
    map.serialize_entry("message", message)?;
    if let Some(timeout) = timeout {
        map.serialize_entry("timeout", &timeout)?;
    }
    map.end()
}

fn serialize_ui_input<S: Serializer>(
    serializer: S,
    id: &str,
    title: &str,
    placeholder: Option<&str>,
    timeout: Option<u64>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "input")?;
    map.serialize_entry("title", title)?;
    if let Some(placeholder) = placeholder {
        map.serialize_entry("placeholder", placeholder)?;
    }
    if let Some(timeout) = timeout {
        map.serialize_entry("timeout", &timeout)?;
    }
    map.end()
}

fn serialize_ui_editor<S: Serializer>(
    serializer: S,
    id: &str,
    title: &str,
    prefill: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "editor")?;
    map.serialize_entry("title", title)?;
    if let Some(prefill) = prefill {
        map.serialize_entry("prefill", prefill)?;
    }
    map.end()
}

fn serialize_ui_notify<S: Serializer>(
    serializer: S,
    id: &str,
    message: &str,
    notify_type: Option<NotifyType>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "notify")?;
    map.serialize_entry("message", message)?;
    if let Some(notify_type) = notify_type {
        map.serialize_entry("notifyType", &notify_type)?;
    }
    map.end()
}

fn serialize_ui_set_status<S: Serializer>(
    serializer: S,
    id: &str,
    status_key: &str,
    status_text: Option<&str>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "setStatus")?;
    map.serialize_entry("statusKey", status_key)?;
    if let Some(text) = status_text {
        map.serialize_entry("statusText", text)?;
    } else {
        map.serialize_entry("statusText", &Value::Null)?;
    }
    map.end()
}

fn serialize_ui_set_widget<S: Serializer>(
    serializer: S,
    id: &str,
    widget_key: &str,
    widget_lines: Option<&[String]>,
    widget_placement: Option<WidgetPlacement>,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "setWidget")?;
    map.serialize_entry("widgetKey", widget_key)?;
    match widget_lines {
        Some(lines) => map.serialize_entry("widgetLines", lines)?,
        None => map.serialize_entry("widgetLines", &Value::Null)?,
    }
    if let Some(placement) = widget_placement {
        map.serialize_entry("widgetPlacement", &placement)?;
    }
    map.end()
}

fn serialize_ui_set_title<S: Serializer>(
    serializer: S,
    id: &str,
    title: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "setTitle")?;
    map.serialize_entry("title", title)?;
    map.end()
}

fn serialize_ui_set_editor_text<S: Serializer>(
    serializer: S,
    id: &str,
    text: &str,
) -> Result<S::Ok, S::Error> {
    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("type", "extension_ui_request")?;
    map.serialize_entry("id", id)?;
    map.serialize_entry("method", "set_editor_text")?;
    map.serialize_entry("text", text)?;
    map.end()
}

impl<'de> Deserialize<'de> for RpcExtensionUiRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| de::Error::custom("extension_ui_request must be a JSON object"))?;
        let type_field = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("extension_ui_request missing type"))?;
        if type_field != "extension_ui_request" {
            return Err(de::Error::custom(format!(
                "expected type \"extension_ui_request\", got {type_field:?}"
            )));
        }
        let id = required_string(obj, "id")?;
        let method = required_string(obj, "method")?;
        parse_ui_request(obj, id, &method).map_err(de::Error::custom)
    }
}

fn parse_ui_request(
    obj: &Map<String, Value>,
    id: String,
    method: &str,
) -> Result<RpcExtensionUiRequest, String> {
    match method {
        "select" => parse_ui_select(obj, id),
        "confirm" => Ok(RpcExtensionUiRequest::Confirm {
            id,
            title: required_string_owned(obj, "title")?,
            message: required_string_owned(obj, "message")?,
            timeout: optional_u64(obj, "timeout"),
        }),
        "input" => Ok(RpcExtensionUiRequest::Input {
            id,
            title: required_string_owned(obj, "title")?,
            placeholder: optional_string(obj, "placeholder"),
            timeout: optional_u64(obj, "timeout"),
        }),
        "editor" => Ok(RpcExtensionUiRequest::Editor {
            id,
            title: required_string_owned(obj, "title")?,
            prefill: optional_string(obj, "prefill"),
        }),
        "notify" => parse_ui_notify(obj, id),
        "setStatus" => parse_ui_set_status(obj, id),
        "setWidget" => parse_ui_set_widget(obj, id),
        "setTitle" => Ok(RpcExtensionUiRequest::SetTitle {
            id,
            title: required_string_owned(obj, "title")?,
        }),
        "set_editor_text" => Ok(RpcExtensionUiRequest::SetEditorText {
            id,
            text: required_string_owned(obj, "text")?,
        }),
        other => Err(format!("unknown extension_ui_request method: {other}")),
    }
}

fn parse_ui_select(obj: &Map<String, Value>, id: String) -> Result<RpcExtensionUiRequest, String> {
    let title = required_string_owned(obj, "title")?;
    let options = obj
        .get("options")
        .ok_or_else(|| "select missing options".to_owned())?;
    let options = Vec::<String>::deserialize(options).map_err(|e| e.to_string())?;
    Ok(RpcExtensionUiRequest::Select {
        id,
        title,
        options,
        timeout: optional_u64(obj, "timeout"),
    })
}

fn parse_ui_notify(obj: &Map<String, Value>, id: String) -> Result<RpcExtensionUiRequest, String> {
    let notify_type = match obj.get("notifyType") {
        None | Some(Value::Null) => None,
        Some(v) => Some(NotifyType::deserialize(v).map_err(|e| e.to_string())?),
    };
    Ok(RpcExtensionUiRequest::Notify {
        id,
        message: required_string_owned(obj, "message")?,
        notify_type,
    })
}

fn parse_ui_set_status(
    obj: &Map<String, Value>,
    id: String,
) -> Result<RpcExtensionUiRequest, String> {
    let status_text = match obj.get("statusText") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err(format!("statusText must be string or null, got {other}"));
        }
    };
    Ok(RpcExtensionUiRequest::SetStatus {
        id,
        status_key: required_string_owned(obj, "statusKey")?,
        status_text,
    })
}

fn parse_ui_set_widget(
    obj: &Map<String, Value>,
    id: String,
) -> Result<RpcExtensionUiRequest, String> {
    let widget_lines = match obj.get("widgetLines") {
        None | Some(Value::Null) => None,
        Some(v) => Some(Vec::<String>::deserialize(v).map_err(|e| e.to_string())?),
    };
    let widget_placement = match obj.get("widgetPlacement") {
        None | Some(Value::Null) => None,
        Some(v) => Some(WidgetPlacement::deserialize(v).map_err(|e| e.to_string())?),
    };
    Ok(RpcExtensionUiRequest::SetWidget {
        id,
        widget_key: required_string_owned(obj, "widgetKey")?,
        widget_lines,
        widget_placement,
    })
}

/// Response to an extension UI request (`type: "extension_ui_response"`).
#[derive(Clone, Debug, PartialEq)]
pub enum RpcExtensionUiResponse {
    /// Select/input/editor value.
    Value {
        /// Correlation id.
        id: String,
        /// Selected or entered value.
        value: String,
    },
    /// Confirm result.
    Confirmed {
        /// Correlation id.
        id: String,
        /// Whether confirmed.
        confirmed: bool,
    },
    /// User cancelled the dialog.
    Cancelled {
        /// Correlation id.
        id: String,
    },
}

impl RpcExtensionUiResponse {
    /// Correlation id.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Value { id, .. } | Self::Confirmed { id, .. } | Self::Cancelled { id } => {
                id.as_str()
            }
        }
    }
}

impl Serialize for RpcExtensionUiResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Value { id, value } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("type", "extension_ui_response")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("value", value)?;
                map.end()
            }
            Self::Confirmed { id, confirmed } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("type", "extension_ui_response")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("confirmed", confirmed)?;
                map.end()
            }
            Self::Cancelled { id } => {
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("type", "extension_ui_response")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("cancelled", &true)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RpcExtensionUiResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| de::Error::custom("extension_ui_response must be a JSON object"))?;

        let type_field = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("extension_ui_response missing type"))?;
        if type_field != "extension_ui_response" {
            return Err(de::Error::custom(format!(
                "expected type \"extension_ui_response\", got {type_field:?}"
            )));
        }

        let id = required_string(obj, "id")?;

        if let Some(Value::Bool(true)) = obj.get("cancelled") {
            return Ok(Self::Cancelled { id });
        }
        if let Some(Value::Bool(confirmed)) = obj.get("confirmed") {
            return Ok(Self::Confirmed {
                id,
                confirmed: *confirmed,
            });
        }
        if let Some(Value::String(value)) = obj.get("value") {
            return Ok(Self::Value {
                id,
                value: value.clone(),
            });
        }

        Err(de::Error::custom(
            "extension_ui_response must include value, confirmed, or cancelled:true",
        ))
    }
}

// ---------------------------------------------------------------------------
// Serde helpers
// ---------------------------------------------------------------------------

fn serialize_id<S>(map: &mut S, id: Option<&str>) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    if let Some(id) = id {
        map.serialize_entry("id", id)?;
    }
    Ok(())
}

fn serialize_type_only<S>(
    serializer: S,
    id: Option<&str>,
    type_name: &str,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut map = serializer.serialize_map(None)?;
    serialize_id(&mut map, id)?;
    map.serialize_entry("type", type_name)?;
    map.end()
}

fn optional_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn required_string<E: de::Error>(obj: &Map<String, Value>, key: &str) -> Result<String, E> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(E::custom(format!(
            "field {key} must be a string, got {other}"
        ))),
        None => Err(E::custom(format!("missing field {key}"))),
    }
}

fn optional_bool(obj: &Map<String, Value>, key: &str) -> Option<bool> {
    match obj.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn optional_u64(obj: &Map<String, Value>, key: &str) -> Option<u64> {
    match obj.get(key) {
        Some(Value::Number(n)) => n.as_u64(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{ModelCost, ModelInput};
    use serde_json::json;
    use std::collections::BTreeMap;

    type TestResult = Result<(), String>;

    fn fail(msg: impl Into<String>) -> String {
        msg.into()
    }

    fn sample_model() -> Model {
        Model {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
            extra: BTreeMap::default(),
        }
    }

    fn assert_json_eq(actual: &Value, expected: &Value) -> TestResult {
        if actual == expected {
            Ok(())
        } else {
            Err(fail(format!(
                "JSON mismatch\n actual: {actual}\n expected: {expected}"
            )))
        }
    }

    fn roundtrip_command(cmd: &RpcCommand) -> Result<RpcCommand, String> {
        let value = serde_json::to_value(cmd).map_err(|e| fail(e.to_string()))?;
        serde_json::from_value(value).map_err(|e| fail(e.to_string()))
    }

    fn roundtrip_response(resp: &RpcResponse) -> Result<RpcResponse, String> {
        let value = serde_json::to_value(resp).map_err(|e| fail(e.to_string()))?;
        serde_json::from_value(value).map_err(|e| fail(e.to_string()))
    }

    fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, String> {
        serde_json::to_value(v).map_err(|e| fail(e.to_string()))
    }

    fn from_value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, String> {
        serde_json::from_value(v).map_err(|e| fail(e.to_string()))
    }

    #[test]
    fn command_prompt_wire_fields() -> TestResult {
        let cmd = RpcCommand::Prompt {
            id: Some("1".into()),
            message: "hi".into(),
            images: None,
            streaming_behavior: Some(StreamingBehavior::FollowUp),
        };
        let value = to_value(&cmd)?;
        assert_json_eq(
            &value,
            &json!({
                "id": "1",
                "type": "prompt",
                "message": "hi",
                "streamingBehavior": "followUp"
            }),
        )?;
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("prompt roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn command_set_model_camel_case() -> TestResult {
        let cmd = RpcCommand::SetModel {
            id: None,
            provider: "openai".into(),
            model_id: "gpt-4o".into(),
        };
        let value = to_value(&cmd)?;
        assert_json_eq(
            &value,
            &json!({
                "type": "set_model",
                "provider": "openai",
                "modelId": "gpt-4o"
            }),
        )?;
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("set_model roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn command_queue_modes_use_kebab_wire() -> TestResult {
        let cmd = RpcCommand::SetSteeringMode {
            id: Some("q".into()),
            mode: QueueMode::OneAtATime,
        };
        let value = to_value(&cmd)?;
        if value["mode"] != "one-at-a-time" {
            return Err(fail(format!("mode wire: {}", value["mode"])));
        }
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("queue mode roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn command_set_thinking_level_includes_off() -> TestResult {
        let cmd = RpcCommand::SetThinkingLevel {
            id: None,
            level: ModelThinkingLevel::Off,
        };
        let value = to_value(&cmd)?;
        if value["level"] != "off" {
            return Err(fail(format!("level wire: {}", value["level"])));
        }
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("thinking level roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn command_bash_exclude_from_context() -> TestResult {
        let cmd = RpcCommand::Bash {
            id: Some("b".into()),
            command: "echo hi".into(),
            exclude_from_context: Some(true),
        };
        let value = to_value(&cmd)?;
        assert_json_eq(
            &value,
            &json!({
                "id": "b",
                "type": "bash",
                "command": "echo hi",
                "excludeFromContext": true
            }),
        )?;
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("bash roundtrip mismatch"));
        }
        Ok(())
    }

    fn sample_commands() -> Vec<RpcCommand> {
        vec![
            RpcCommand::Prompt {
                id: Some("1".into()),
                message: "m".into(),
                images: None,
                streaming_behavior: Some(StreamingBehavior::Steer),
            },
            RpcCommand::Steer {
                id: None,
                message: "s".into(),
                images: None,
            },
            RpcCommand::FollowUp {
                id: None,
                message: "f".into(),
                images: None,
            },
            RpcCommand::Abort { id: None },
            RpcCommand::NewSession {
                id: None,
                parent_session: Some("/tmp/s".into()),
            },
            RpcCommand::GetState { id: None },
            RpcCommand::SetModel {
                id: None,
                provider: "p".into(),
                model_id: "m".into(),
            },
            RpcCommand::CycleModel { id: None },
            RpcCommand::GetAvailableModels { id: None },
            RpcCommand::SetThinkingLevel {
                id: None,
                level: ModelThinkingLevel::High,
            },
            RpcCommand::CycleThinkingLevel { id: None },
            RpcCommand::SetSteeringMode {
                id: None,
                mode: QueueMode::All,
            },
            RpcCommand::SetFollowUpMode {
                id: None,
                mode: QueueMode::OneAtATime,
            },
            RpcCommand::Compact {
                id: None,
                custom_instructions: Some("x".into()),
            },
            RpcCommand::SetAutoCompaction {
                id: None,
                enabled: true,
            },
            RpcCommand::SetAutoRetry {
                id: None,
                enabled: false,
            },
            RpcCommand::AbortRetry { id: None },
            RpcCommand::Bash {
                id: None,
                command: "true".into(),
                exclude_from_context: None,
            },
            RpcCommand::AbortBash { id: None },
            RpcCommand::GetSessionStats { id: None },
            RpcCommand::ExportHtml {
                id: None,
                output_path: Some("out.html".into()),
            },
            RpcCommand::SwitchSession {
                id: None,
                session_path: "/s".into(),
            },
            RpcCommand::Fork {
                id: None,
                entry_id: "e1".into(),
            },
            RpcCommand::Clone { id: None },
            RpcCommand::GetForkMessages { id: None },
            RpcCommand::GetEntries {
                id: None,
                since: Some("e0".into()),
            },
            RpcCommand::GetTree { id: None },
            RpcCommand::GetLastAssistantText { id: None },
            RpcCommand::SetSessionName {
                id: None,
                name: "n".into(),
            },
            RpcCommand::GetMessages { id: None },
            RpcCommand::GetCommands { id: None },
        ]
    }

    #[test]
    fn all_31_known_command_types_roundtrip() -> TestResult {
        let samples = sample_commands();
        if samples.len() != 31 {
            return Err(fail(format!("expected 31 samples, got {}", samples.len())));
        }
        for cmd in &samples {
            let rt = roundtrip_command(cmd)?;
            if &rt != cmd {
                return Err(fail(format!("roundtrip failed for {}", cmd.command_type())));
            }
            if rt.command_type() != cmd.command_type() {
                return Err(fail(format!("type mismatch for {}", cmd.command_type())));
            }
        }
        Ok(())
    }

    #[test]
    fn unknown_command_preserves_type_id_and_payload() -> TestResult {
        let raw = json!({
            "id": "42",
            "type": "future_command",
            "foo": 1,
            "bar": "x"
        });
        let cmd: RpcCommand = from_value(raw)?;
        match &cmd {
            RpcCommand::Unknown {
                id,
                command_type,
                payload,
            } => {
                if id.as_deref() != Some("42") {
                    return Err(fail(format!("id={id:?}")));
                }
                if command_type != "future_command" {
                    return Err(fail(format!("type={command_type}")));
                }
                if payload.get("foo") != Some(&json!(1)) {
                    return Err(fail("missing foo"));
                }
                if payload.get("bar") != Some(&json!("x")) {
                    return Err(fail("missing bar"));
                }
                if payload.contains_key("type") || payload.contains_key("id") {
                    return Err(fail("payload should exclude type/id"));
                }
            }
            other => return Err(fail(format!("expected Unknown, got {other:?}"))),
        }
        let re = to_value(&cmd)?;
        if re["id"] != "42" || re["type"] != "future_command" || re["foo"] != 1 || re["bar"] != "x"
        {
            return Err(fail(format!("reserialized unknown: {re}")));
        }
        Ok(())
    }

    #[test]
    fn response_success_without_data() -> TestResult {
        let resp = RpcResponse::ok(Some("1".into()), "prompt");
        let value = to_value(&resp)?;
        assert_json_eq(
            &value,
            &json!({
                "id": "1",
                "type": "response",
                "command": "prompt",
                "success": true
            }),
        )?;
        if roundtrip_response(&resp)? != resp {
            return Err(fail("success response roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_error_wire() -> TestResult {
        let resp = RpcResponse::err(
            Some("9".into()),
            "future_command",
            "Unknown command: future_command",
        );
        let value = to_value(&resp)?;
        assert_json_eq(
            &value,
            &json!({
                "id": "9",
                "type": "response",
                "command": "future_command",
                "success": false,
                "error": "Unknown command: future_command"
            }),
        )?;
        if roundtrip_response(&resp)? != resp {
            return Err(fail("error response roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_cancelled_data() -> TestResult {
        let resp = RpcResponse::ok_data(
            None,
            "new_session",
            RpcResponseData::Cancelled { cancelled: true },
        );
        let value = to_value(&resp)?;
        if value["success"] != true || value["data"]["cancelled"] != true {
            return Err(fail(format!("cancelled wire: {value}")));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("cancelled roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_cycle_model_null_data() -> TestResult {
        let resp = RpcResponse::ok_data(None, "cycle_model", RpcResponseData::CycleModel(None));
        let value = to_value(&resp)?;
        if value["data"] != Value::Null {
            return Err(fail(format!("expected null data, got {}", value["data"])));
        }
        let de: RpcResponse = from_value(value)?;
        match de {
            RpcResponse::Success {
                data: Some(boxed), ..
            } if matches!(boxed.as_ref(), RpcResponseData::CycleModel(None)) => Ok(()),
            other => Err(fail(format!("expected CycleModel(None), got {other:?}"))),
        }
    }

    #[test]
    fn response_get_state_session_state_fields() -> TestResult {
        let state = RpcSessionState {
            model: Some(sample_model()),
            thinking_level: ModelThinkingLevel::Low,
            is_streaming: false,
            is_compacting: false,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::OneAtATime,
            session_file: Some("/tmp/s.jsonl".into()),
            session_id: "sid".into(),
            session_name: Some("work".into()),
            auto_compaction_enabled: true,
            message_count: 3,
            pending_message_count: 0,
        };
        let resp = RpcResponse::ok_data(
            Some("g".into()),
            "get_state",
            RpcResponseData::SessionState(state),
        );
        let value = to_value(&resp)?;
        if value["data"]["thinkingLevel"] != "low"
            || value["data"]["isStreaming"] != false
            || value["data"]["steeringMode"] != "all"
            || value["data"]["followUpMode"] != "one-at-a-time"
            || value["data"]["sessionId"] != "sid"
            || value["data"]["messageCount"] != 3
            || value["data"]["pendingMessageCount"] != 0
            || value["data"]["autoCompactionEnabled"] != true
        {
            return Err(fail(format!("get_state fields: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("get_state roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_bash_result_camel_case() -> TestResult {
        let data = RpcResponseData::Bash(BashResult {
            output: "ok".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
        });
        let resp = RpcResponse::ok_data(None, "bash", data);
        let value = to_value(&resp)?;
        if value["data"]["exitCode"] != 0
            || value["data"]["cancelled"] != false
            || value["data"]["truncated"] != false
            || value["data"].get("fullOutputPath").is_some()
        {
            return Err(fail(format!("bash data: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("bash response roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_session_stats_tokens() -> TestResult {
        let data = RpcResponseData::SessionStats(SessionStats {
            session_file: None,
            session_id: "s".into(),
            user_messages: 1,
            assistant_messages: 1,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 2,
            tokens: SessionStatsTokens {
                input: 10,
                output: 20,
                cache_read: 0,
                cache_write: 0,
                total: 30,
            },
            cost: 0.01,
            context_usage: Some(ContextUsage {
                tokens: Some(30),
                context_window: 128_000,
                percent: Some(0.02),
            }),
        });
        let resp = RpcResponse::ok_data(None, "get_session_stats", data);
        let value = to_value(&resp)?;
        if value["data"]["sessionId"] != "s"
            || value["data"]["userMessages"] != 1
            || value["data"]["tokens"]["cacheRead"] != 0
            || value["data"]["contextUsage"]["contextWindow"] != 128_000
        {
            return Err(fail(format!("stats data: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("stats roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_get_commands_slash_command() -> TestResult {
        let cmd = RpcSlashCommand {
            name: "skill:foo".into(),
            description: Some("Foo skill".into()),
            source: RpcSlashCommandSource::Skill,
            source_info: RpcSourceInfo {
                path: "/skills/foo".into(),
                source: "local".into(),
                scope: RpcSourceScope::Project,
                origin: RpcSourceOrigin::TopLevel,
                base_dir: None,
            },
        };
        let resp = RpcResponse::ok_data(
            None,
            "get_commands",
            RpcResponseData::Commands {
                commands: vec![cmd],
            },
        );
        let value = to_value(&resp)?;
        if value["data"]["commands"][0]["name"] != "skill:foo"
            || value["data"]["commands"][0]["source"] != "skill"
            || value["data"]["commands"][0]["sourceInfo"]["origin"] != "top-level"
            || value["data"]["commands"][0]["sourceInfo"]["scope"] != "project"
        {
            return Err(fail(format!("commands data: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("get_commands roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn extension_ui_request_select_wire() -> TestResult {
        let req = RpcExtensionUiRequest::Select {
            id: "ui1".into(),
            title: "Pick".into(),
            options: vec!["a".into(), "b".into()],
            timeout: Some(1000),
        };
        let value = to_value(&req)?;
        assert_json_eq(
            &value,
            &json!({
                "type": "extension_ui_request",
                "id": "ui1",
                "method": "select",
                "title": "Pick",
                "options": ["a", "b"],
                "timeout": 1000
            }),
        )?;
        let de: RpcExtensionUiRequest = from_value(value)?;
        if de != req {
            return Err(fail("select UI request roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn extension_ui_request_set_status_null_text() -> TestResult {
        let req = RpcExtensionUiRequest::SetStatus {
            id: "s1".into(),
            status_key: "k".into(),
            status_text: None,
        };
        let value = to_value(&req)?;
        if value["method"] != "setStatus"
            || value["statusKey"] != "k"
            || value["statusText"] != Value::Null
        {
            return Err(fail(format!("setStatus wire: {value}")));
        }
        let de: RpcExtensionUiRequest = from_value(value)?;
        if de != req {
            return Err(fail("setStatus roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn extension_ui_request_set_widget_and_editor_text() -> TestResult {
        let widget = RpcExtensionUiRequest::SetWidget {
            id: "w".into(),
            widget_key: "wk".into(),
            widget_lines: Some(vec!["l1".into()]),
            widget_placement: Some(WidgetPlacement::AboveEditor),
        };
        let value = to_value(&widget)?;
        if value["method"] != "setWidget" || value["widgetPlacement"] != "aboveEditor" {
            return Err(fail(format!("setWidget wire: {value}")));
        }
        let de: RpcExtensionUiRequest = from_value(value)?;
        if de != widget {
            return Err(fail("setWidget roundtrip mismatch"));
        }

        let editor = RpcExtensionUiRequest::SetEditorText {
            id: "e".into(),
            text: "hello".into(),
        };
        let value = to_value(&editor)?;
        if value["method"] != "set_editor_text" {
            return Err(fail(format!("set_editor_text wire: {value}")));
        }
        let de: RpcExtensionUiRequest = from_value(value)?;
        if de != editor {
            return Err(fail("set_editor_text roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn extension_ui_response_variants() -> TestResult {
        let cases = [
            RpcExtensionUiResponse::Value {
                id: "1".into(),
                value: "x".into(),
            },
            RpcExtensionUiResponse::Confirmed {
                id: "2".into(),
                confirmed: false,
            },
            RpcExtensionUiResponse::Cancelled { id: "3".into() },
        ];
        for case in &cases {
            let value = to_value(case)?;
            if value["type"] != "extension_ui_response" {
                return Err(fail(format!("ui response type: {value}")));
            }
            let de: RpcExtensionUiResponse = from_value(value)?;
            if &de != case {
                return Err(fail(format!("ui response roundtrip mismatch: {case:?}")));
            }
        }
        Ok(())
    }

    #[test]
    fn response_fork_messages_entry_id_camel_case() -> TestResult {
        let resp = RpcResponse::ok_data(
            None,
            "get_fork_messages",
            RpcResponseData::ForkMessages {
                messages: vec![ForkMessage {
                    entry_id: "e1".into(),
                    text: "hello".into(),
                }],
            },
        );
        let value = to_value(&resp)?;
        if value["data"]["messages"][0]["entryId"] != "e1" {
            return Err(fail(format!("fork messages: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("fork messages roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn response_last_assistant_text_null() -> TestResult {
        let resp = RpcResponse::ok_data(
            None,
            "get_last_assistant_text",
            RpcResponseData::LastAssistantText { text: None },
        );
        let value = to_value(&resp)?;
        if value["data"]["text"] != Value::Null {
            return Err(fail(format!("last assistant text: {}", value["data"])));
        }
        if roundtrip_response(&resp)? != resp {
            return Err(fail("last assistant text roundtrip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn command_id_and_type_accessors() -> TestResult {
        let cmd = RpcCommand::Unknown {
            id: Some("x".into()),
            command_type: "nope".into(),
            payload: Map::new(),
        };
        if cmd.id() != Some("x") || cmd.command_type() != "nope" {
            return Err(fail("unknown accessors mismatch"));
        }
        Ok(())
    }

    #[test]
    fn image_content_roundtrip_inside_prompt() -> TestResult {
        let img = ImageContent::new("AAAA", "image/png");
        let cmd = RpcCommand::Prompt {
            id: None,
            message: "see".into(),
            images: Some(vec![img]),
            streaming_behavior: None,
        };
        let value = to_value(&cmd)?;
        if value["images"][0]["type"] != "image" || value["images"][0]["mimeType"] != "image/png" {
            return Err(fail(format!("image wire: {}", value["images"])));
        }
        if roundtrip_command(&cmd)? != cmd {
            return Err(fail("image prompt roundtrip mismatch"));
        }
        Ok(())
    }
}
