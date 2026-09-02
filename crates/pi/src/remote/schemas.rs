//! Protocol message schemas — portable mirror of upstream `schemas.ts`.
//!
//! Every type uses serde tags matching the upstream CBOR map key order.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Protocol version (currently 1).
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// Typed error codes exchanged in `ProtocolError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    /// Protocol version mismatch.
    Version,
    /// Server is busy and cannot accept the request.
    Busy,
    /// Session is locked by another client.
    SessionLocked,
    /// Referenced session was not found.
    NotFound,
    /// Request was malformed or invalid.
    InvalidRequest,
    /// Requested feature is not implemented.
    NotImplemented,
    /// Internal server error.
    InternalError,
}

/// A protocol-level error returned by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    /// Machine-readable error code.
    pub code: ProtocolErrorCode,
    /// Human-readable error description.
    pub message: String,
    /// Optional structured details about the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
}

// ---------------------------------------------------------------------------
// Common value types
// ---------------------------------------------------------------------------

/// A JSON value, preserving insertion order for maps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonValue {
    /// JSON null.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON integer.
    Int(i64),
    /// JSON floating-point number.
    Float(f64),
    /// JSON string.
    String(String),
    /// JSON array.
    Array(Vec<JsonValue>),
    /// JSON object with insertion-ordered keys.
    Object(IndexMap<String, JsonValue>),
}

impl JsonValue {
    /// Returns the JSON null value.
    #[must_use]
    pub fn null() -> Self {
        Self::Null
    }
}

// ---------------------------------------------------------------------------
// Thinking level
// ---------------------------------------------------------------------------

/// Verbosity of chain-of-thought exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// No chain-of-thought.
    Off,
    /// Minimal chain-of-thought.
    Minimal,
    /// Low chain-of-thought.
    Low,
    /// Medium chain-of-thought.
    Medium,
    /// High chain-of-thought.
    High,
    /// Extra-high chain-of-thought.
    Xhigh,
    /// Maximum chain-of-thought.
    Max,
}

// ---------------------------------------------------------------------------
// Session phase
// ---------------------------------------------------------------------------

/// Lifecycle phase of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    /// Session is idle, waiting for input.
    Idle,
    /// Session is processing a turn.
    Turn,
    /// Session is compacting its context.
    Compaction,
    /// Session is generating a branch summary.
    BranchSummary,
    /// Session is retrying a failed request.
    Retry,
}

// ---------------------------------------------------------------------------
// Model reference / metadata
// ---------------------------------------------------------------------------

/// A reference to a model by provider and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider identifier (e.g. `"anthropic"`).
    pub provider: String,
    /// Model identifier within the provider.
    pub id: String,
}

/// Cost breakdown for a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Cost per input token in USD.
    pub input: f64,
    /// Cost per output token in USD.
    pub output: f64,
    /// Cost per cache-read token in USD.
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    /// Cost per cache-write token in USD.
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
}

/// Metadata about a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Provider identifier.
    pub provider: String,
    /// Model identifier within the provider.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// API wire format identifier.
    pub api: String,
    /// Whether the model supports reasoning/thinking.
    pub reasoning: bool,
    /// Accepted input modalities (e.g. `"text"`, `"image"`).
    pub input: Vec<String>,
    /// Maximum context window in tokens.
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
    /// Maximum output tokens per response.
    #[serde(rename = "maxTokens")]
    pub max_tokens: u64,
    /// Per-token cost breakdown.
    pub cost: ModelCost,
    /// Thinking levels the model supports.
    #[serde(rename = "supportedThinkingLevels")]
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    /// Whether the provider credentials are valid.
    pub authenticated: bool,
}

// ---------------------------------------------------------------------------
// Usage / token accounting
// ---------------------------------------------------------------------------

/// Token usage for a single model invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens consumed.
    pub input: u64,
    /// Output tokens generated.
    pub output: u64,
    /// Cache-read tokens.
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    /// Cache-write tokens.
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    /// Reasoning tokens consumed (if supported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// Total tokens (input + output + cache).
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    /// Dollar-cost breakdown for this invocation.
    pub cost: UsageCost,
}

/// Cost breakdown for a usage entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageCost {
    /// Cost of input tokens in USD.
    pub input: f64,
    /// Cost of output tokens in USD.
    pub output: f64,
    /// Cost of cache-read tokens in USD.
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    /// Cost of cache-write tokens in USD.
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    /// Total cost in USD.
    pub total: f64,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    /// Block type tag (always `"text"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// The text payload.
    pub text: String,
}

/// Thinking content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingContent {
    /// Block type tag (always `"thinking"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// The chain-of-thought text.
    pub thinking: String,
    /// Whether the thinking was redacted by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// Image content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    /// Block type tag (always `"image"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type of the image (e.g. `"image/png"`).
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Tool call content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContent {
    /// Block type tag (always `"tool_call"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Unique identifier for the tool call.
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    /// Name of the tool being called.
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// Tool input arguments as a JSON value.
    pub input: JsonValue,
}

/// User content block (text or image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Text content from the user.
    Text(TextContent),
    /// Image content from the user.
    Image(ImageContent),
}

/// Assistant content block (text, thinking, or tool call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    /// Text generated by the assistant.
    Text(TextContent),
    /// Chain-of-thought from the assistant.
    Thinking(ThinkingContent),
    /// Tool call initiated by the assistant.
    ToolCall(ToolCallContent),
}

/// Tool content block (text or image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolContent {
    /// Text output from a tool.
    Text(TextContent),
    /// Image output from a tool.
    Image(ImageContent),
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

/// User transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTranscriptItem {
    /// Item type tag (always `"user"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Content blocks in this user message.
    pub content: Vec<UserContent>,
}

/// Assistant transcript item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTranscriptItem {
    /// Item type tag (always `"assistant"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Content blocks in this assistant response.
    pub content: Vec<AssistantContent>,
}

/// Tool transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTranscriptItem {
    /// Item type tag (always `"tool"`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Identifier of the tool call this result corresponds to.
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    /// Content blocks returned by the tool.
    pub content: Vec<ToolContent>,
}

/// A single transcript entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptItem {
    /// User message.
    User(UserTranscriptItem),
    /// Assistant response.
    Assistant(AssistantTranscriptItem),
    /// Tool result.
    Tool(ToolTranscriptItem),
}

/// Progress notification for a transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptProgress {
    /// Current session phase.
    pub phase: SessionPhase,
    /// Optional human-readable progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Session metadata / snapshot
// ---------------------------------------------------------------------------

/// Metadata about a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Unique session identifier.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Current model reference.
    pub model: ModelRef,
    /// Current thinking level.
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: ThinkingLevel,
    /// Whether the session is locked by a client.
    pub locked: bool,
    /// Monotonic revision number for optimistic concurrency.
    pub revision: u64,
    /// Full transcript of the session.
    pub transcript: Vec<TranscriptItem>,
    /// Queued steer messages awaiting processing.
    #[serde(rename = "queuedSteer")]
    pub queued_steer: Vec<UserTranscriptItem>,
    /// Number of queued steer messages.
    #[serde(rename = "queuedSteerCount")]
    pub queued_steer_count: u64,
}

/// Full session snapshot (same as `SessionMetadata` in upstream).
pub type SessionSnapshot = SessionMetadata;

/// Server snapshot returned in the initial hello.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    /// Unique server instance identifier.
    #[serde(rename = "serverId")]
    pub server_id: String,
    /// Protocol version the server speaks.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    /// Server-wide monotonic revision counter.
    pub revision: u64,
    /// All sessions currently managed by the server.
    pub sessions: Vec<SessionMetadata>,
    /// Available models on the server.
    pub models: Vec<ModelMetadata>,
}

// ---------------------------------------------------------------------------
// Commands (client → server)
// ---------------------------------------------------------------------------

/// A command sent from client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// List all sessions.
    List,
    /// Create a new session.
    Create {
        /// Optional working directory for the session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Optional human-readable name for the session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Optional initial model reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        /// Optional initial thinking level.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "thinkingLevel")]
        thinking_level: Option<ThinkingLevel>,
    },
    /// Attach to an existing session.
    Attach {
        /// Session to attach to.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Detach from a session.
    Detach {
        /// Session to detach from.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Send a prompt to a session.
    Prompt {
        /// Target session.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Prompt text.
        text: String,
    },
    /// Steer (interject into) a running session.
    Steer {
        /// Target session.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Steer text.
        text: String,
    },
    /// Abort the current turn of a session.
    Abort {
        /// Target session.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Change the model of a session.
    #[serde(rename = "set_model")]
    SetModel {
        /// Target session.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// New model reference.
        model: ModelRef,
    },
    /// Change the thinking level of a session.
    #[serde(rename = "set_thinking")]
    SetThinking {
        /// Target session.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// New thinking level.
        #[serde(rename = "thinkingLevel")]
        thinking_level: ThinkingLevel,
    },
}

// ---------------------------------------------------------------------------
// Command results (server → client)
// ---------------------------------------------------------------------------

/// Result of a command execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CommandResult {
    /// Result of `List` — all sessions.
    List {
        /// All sessions on the server.
        sessions: Vec<SessionMetadata>,
    },
    /// Result of `Create` — the new session.
    Create {
        /// The created session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `Attach` — the attached session.
    Attach {
        /// The attached session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `Detach`.
    Detach {
        /// Session that was detached.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    /// Result of `Prompt` — updated session.
    Prompt {
        /// Updated session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `Steer` — updated session.
    Steer {
        /// Updated session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `Abort` — updated session.
    Abort {
        /// Updated session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `SetModel` — updated session.
    #[serde(rename = "set_model")]
    SetModel {
        /// Updated session snapshot.
        session: SessionSnapshot,
    },
    /// Result of `SetThinking` — updated session.
    #[serde(rename = "set_thinking")]
    SetThinking {
        /// Updated session snapshot.
        session: SessionSnapshot,
    },
}

// ---------------------------------------------------------------------------
// Server events (server → client, unsolicited)
// ---------------------------------------------------------------------------

/// An unsolicited event from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// Initial server snapshot sent on connection.
    #[serde(rename = "server_snapshot")]
    ServerSnapshot {
        /// Full server state snapshot.
        snapshot: ServerSnapshot,
    },
    /// Updated session snapshot.
    #[serde(rename = "session_snapshot")]
    SessionSnapshot {
        /// Updated session state.
        snapshot: SessionSnapshot,
    },
    /// Progress update for a session.
    #[serde(rename = "session_progress")]
    SessionProgress {
        /// Session that produced the progress.
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Progress details.
        progress: TranscriptProgress,
    },
    /// Session was removed.
    #[serde(rename = "session_removed")]
    SessionRemoved {
        /// Session that was removed.
        #[serde(rename = "sessionId")]
        session_id: String,
    },
}

// ---------------------------------------------------------------------------
// Top-level messages
// ---------------------------------------------------------------------------

/// A message from the client to the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Initial handshake.
    Hello {
        /// Client protocol version.
        version: u32,
    },
    /// Command request.
    Request {
        /// Unique request identifier.
        id: String,
        /// The command to execute.
        request: Command,
    },
}

/// A message from the server to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Successful handshake response.
    Hello {
        /// Server protocol version.
        version: u32,
        /// Unique connection identifier.
        #[serde(rename = "connectionId")]
        connection_id: String,
        /// Initial server state.
        snapshot: ServerSnapshot,
    },
    /// Failed handshake response.
    HelloError {
        /// Error details.
        error: ProtocolError,
    },
    /// Response to a command request.
    Response {
        /// Request identifier this response corresponds to.
        id: String,
        /// Whether the command succeeded.
        ok: bool,
        /// Command result on success.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CommandResult>,
        /// Error on failure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    /// Unsolicited server event.
    Event {
        /// The event payload.
        event: ServerEvent,
    },
}
