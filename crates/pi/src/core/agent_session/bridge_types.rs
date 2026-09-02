//! Product-owned bridge types for the extension session-control surface.
//!
//! These types replace the `pi_ext::protocol` wire vocabulary in
//! `agent_session/**` production code. The [`crate::core::extension_host`]
//! adapter owns every product↔wire conversion; `agent_session` names zero
//! `pi_ext` symbols.
//!
//! # Type locations reused here
//!
//! - [`ForkPosition`] — `crate::core::agent_session_runtime::ForkPosition`
//! - [`NavigateTreeOptions`] — `super::tree::NavigateTreeOptions`
//! - [`ToolInfo`] — `super::tools::ToolInfo`
//! - [`SlashCommandInfo`] — `crate::core::resources::slash::SlashCommandInfo`
//! - [`ScopedModel`] — `super::ScopedModel`
//! - [`ContextUsage`] — `super::stats::ContextUsage`
//! - [`Model`] — `pi_ai::Model`
//! - [`ModelThinkingLevel`] — `pi_ai::ModelThinkingLevel`

use serde_json::Value;

use pi_ai::{Model, ModelThinkingLevel};

use crate::core::agent_session_runtime::ForkPosition;
use crate::core::resources::slash::SlashCommandInfo;

use super::ScopedModel;
use super::stats::ContextUsage;
use super::tools::ToolInfo;
use super::tree::NavigateTreeOptions;

// ---------------------------------------------------------------------------
// Correlation id + method identity
// ---------------------------------------------------------------------------

/// Correlation id for bridge requests flowing between the host and product.
///
/// Replaces `pi_ext::protocol::FrameId` in `agent_session` production code.
/// The [`crate::core::extension_host`] adapter converts between this and the
/// wire `FrameId` at the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BridgeRequestId(pub u64);

/// Pure identity enum for the eight correlated bridge methods.
///
/// Carries no wire strings — the [`crate::core::extension_host`] adapter maps
/// each variant to the corresponding `pi_ext` method name. This lets
/// `agent_session` reason about *which* bridge operation failed without
/// importing the wire vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BridgeMethod {
    /// `session.newSession` — correlated new-session request.
    NewSession,
    /// `session.fork` — correlated fork request.
    Fork,
    /// `session.switchSession` — correlated switch-session request.
    SwitchSession,
    /// `session.navigateTree` — correlated navigate-tree request.
    NavigateTree,
    /// `session.reload` — correlated reload request.
    Reload,
    /// `session.setupEntries` — correlated setup-entries snapshot request.
    SetupEntries,
    /// `session.setModel` — correlated set-model request.
    SetModel,
    /// `session.compact` — correlated compact request.
    Compact,
}

// ---------------------------------------------------------------------------
// Fire-and-forget session commands
// ---------------------------------------------------------------------------

/// Envelope wrapping a [`SessionCommand`] with an optional replacement token.
///
/// When `replacement_token` is `Some`, the command is scoped to a pending
/// replacement session rather than the active one.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCommandEnvelope {
    /// Token identifying the pending replacement session, when scoped.
    pub replacement_token: Option<String>,
    /// The fire-and-forget session action.
    pub command: SessionCommand,
}

/// Fire-and-forget extension session action (host → product).
///
/// Mirrors `pi_ext::protocol::SessionCommand` but lives entirely in the
/// product crate. The adapter converts at the seam.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    /// `pi.sendMessage(message, options)`.
    SendMessage {
        /// `Pick<CustomMessage, "customType" | "content" | "display" | "details">`.
        message: Value,
        /// `{ triggerTurn?, deliverAs? }`.
        options: Option<Value>,
    },
    /// `pi.sendUserMessage(content, options)`.
    SendUserMessage {
        /// String or `(TextContent | ImageContent)[]`.
        content: Value,
        /// `{ deliverAs? }`.
        options: Option<Value>,
    },
    /// `pi.appendEntry(customType, data)`.
    AppendEntry {
        /// Custom entry type discriminant.
        custom_type: String,
        /// Arbitrary payload.
        data: Option<Value>,
    },
    /// `pi.setSessionName(name)`.
    SetSessionName {
        /// New display name.
        name: String,
    },
    /// `pi.setLabel(entryId, label)`.
    SetLabel {
        /// Target session entry id.
        entry_id: String,
        /// New label (`None` clears).
        label: Option<String>,
    },
    /// `pi.setActiveTools(toolNames)`.
    SetActiveTools {
        /// Requested active tool names.
        tool_names: Vec<String>,
    },
    /// `pi.refreshTools()`.
    RefreshTools,
    /// `pi.setThinkingLevel(level)`.
    SetThinkingLevel {
        /// Requested level discriminant.
        level: String,
    },
    /// `ctx.abort()`.
    Abort,
    /// `ctx.shutdown()`.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Correlated request payloads
// ---------------------------------------------------------------------------

/// `session.setModel` request payload (host → product).
#[derive(Debug, Clone, PartialEq)]
pub struct SetModelRequest {
    /// Serialized `Model` the extension asked to switch to.
    pub model: Value,
}

/// `session.compact` request payload (host → product).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompactRequest {
    /// Optional custom compaction instructions.
    pub custom_instructions: Option<String>,
}

/// `session.newSession` request payload (host → product).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSessionRequest {
    /// Optional parent session id / path for the new session.
    pub parent_session: Option<String>,
}

/// `session.fork` request payload (host → product).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRequest {
    /// Entry id to fork from.
    pub entry_id: String,
    /// Cut position; omitted maps to [`ForkPosition::Before`] downstream.
    pub position: Option<ForkPosition>,
}

/// `session.switchSession` request payload (host → product).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchSessionRequest {
    /// Filesystem path of the session to switch to.
    pub session_path: String,
}

/// `session.navigateTree` request payload (host → product).
#[derive(Debug, Clone)]
pub struct NavigateTreeRequest {
    /// Target entry / branch id.
    pub target_id: String,
    /// Navigation options (summarize, custom instructions, label, …).
    pub options: NavigateTreeOptions,
}

/// `session.setupEntries` request payload (host → product).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupEntriesRequest {
    /// Token returned by the initiating pending session replacement.
    pub replacement_token: String,
}

// ---------------------------------------------------------------------------
// Product SessionState mirror
// ---------------------------------------------------------------------------

/// Product-owned session state snapshot pushed to extensions.
///
/// Replaces `pi_ext::protocol::SessionStateWire` construction in
/// `agent_session`. The [`crate::core::extension_host`] adapter serializes
/// this to the wire shape at the seam. Every field reuses an existing product
/// type rather than a wire-shaped `Value`.
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Session display name (`None` until set).
    pub session_name: Option<String>,
    /// Current thinking level.
    pub thinking_level: ModelThinkingLevel,
    /// Active tool names.
    pub active_tools: Vec<String>,
    /// All registered tools.
    pub all_tools: Vec<ToolInfo>,
    /// Extension/prompt/skill slash-command catalog.
    pub commands: Vec<SlashCommandInfo>,
    /// Active model, if any.
    pub model: Option<Model>,
    /// Models scoped to this session (`--models` / `enabledModels`).
    pub scoped_models: Vec<ScopedModel>,
    /// Whether the session has no active agent run.
    pub is_idle: bool,
    /// Whether steering/follow-up messages are queued.
    pub has_pending_messages: bool,
    /// Context usage, when computable.
    pub context_usage: Option<ContextUsage>,
    /// Current effective system prompt.
    pub system_prompt: String,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_name: None,
            thinking_level: ModelThinkingLevel::Off,
            active_tools: Vec::new(),
            all_tools: Vec::new(),
            commands: Vec::new(),
            model: None,
            scoped_models: Vec::new(),
            is_idle: true,
            has_pending_messages: false,
            context_usage: None,
            system_prompt: String::new(),
        }
    }
}

/// One extension-registered slash command as the session sees it: wire
/// registry metadata already converted to product values by the host
/// adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCatalogEntry {
    /// Registered command name.
    pub name: String,
    /// Registered description.
    pub description: String,
    /// Owning extension source path, when the host reported one.
    pub source: Option<String>,
    /// Resolved product provenance (resource-discovered or host-reported),
    /// if any.
    pub source_info: Option<crate::core::resources::source_info::SourceInfo>,
}

// ---------------------------------------------------------------------------
// Product error for the reload path
// ---------------------------------------------------------------------------

/// Minimal product error carrying message + retryable semantics.
///
/// Replaces `pi_ext::client::HostClientError` in
/// `ReloadPreAcceptError::Response`. The adapter converts wire errors into
/// this product type at the seam; `agent_session` never names
/// `HostClientError` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionHostError {
    /// Handshake versions did not match (non-retryable).
    Handshake {
        /// Failure detail.
        message: String,
    },
    /// A request did not complete before its deadline (retryable).
    Timeout {
        /// Human-readable timeout detail.
        message: String,
    },
    /// A request was cancelled by the caller (non-retryable).
    Cancelled {
        /// Human-readable cancellation detail.
        message: String,
    },
    /// Host stream closed — EOF or write failure (non-retryable).
    Closed {
        /// Why the stream closed.
        message: String,
        /// Retained stderr tail.
        stderr: String,
    },
    /// Host emitted a malformed frame (non-retryable).
    Protocol {
        /// Decode/validation failure.
        message: String,
        /// Retained stderr tail.
        stderr: String,
    },
    /// Host returned a structured error frame.
    Remote {
        /// Stable error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// Spawning the host process failed (retryable).
    Spawn {
        /// OS or io detail.
        message: String,
    },
    /// The host is no longer running (non-retryable).
    NotRunning,
    /// A payload failed to (de)serialize (non-retryable).
    Payload {
        /// Serialization failure detail.
        message: String,
    },
}

impl ExtensionHostError {
    /// Whether the caller may retry the operation that produced this error.
    ///
    /// Timeouts and spawn failures are retryable; all other variants are
    /// terminal.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Timeout { .. } | Self::Spawn { .. })
    }

    /// Human-readable message for logging and diagnostics.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Handshake { message }
            | Self::Timeout { message }
            | Self::Cancelled { message }
            | Self::Closed { message, .. }
            | Self::Protocol { message, .. }
            | Self::Spawn { message }
            | Self::Payload { message } => message.clone(),
            Self::Remote { code, message } => format!("{code}: {message}"),
            Self::NotRunning => "host not running".to_owned(),
        }
    }
}

impl std::fmt::Display for ExtensionHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ExtensionHostError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_default_is_idle() {
        // A default-constructed mirror describes a quiescent session:
        // hydration and tests rely on idle-by-default, and the adapter
        // serializes `isIdle` from it.
        let state = SessionState::default();
        assert!(state.is_idle);
        assert!(!state.has_pending_messages);
        assert_eq!(state.thinking_level, ModelThinkingLevel::Off);
    }
}
