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
    Version,
    Busy,
    SessionLocked,
    NotFound,
    InvalidRequest,
    NotImplemented,
    InternalError,
}

/// A protocol-level error returned by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
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
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(IndexMap<String, JsonValue>),
}

impl JsonValue {
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
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

// ---------------------------------------------------------------------------
// Session phase
// ---------------------------------------------------------------------------

/// Lifecycle phase of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Turn,
    Compaction,
    BranchSummary,
    Retry,
}

// ---------------------------------------------------------------------------
// Model reference / metadata
// ---------------------------------------------------------------------------

/// A reference to a model by provider and id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

/// Cost breakdown for a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
}

/// Metadata about a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u64,
    pub cost: ModelCost,
    #[serde(rename = "supportedThinkingLevels")]
    pub supported_thinking_levels: Vec<ThinkingLevel>,
    pub authenticated: bool,
}

// ---------------------------------------------------------------------------
// Usage / token accounting
// ---------------------------------------------------------------------------

/// Token usage for a single model invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(rename = "cacheRead")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Cost breakdown for a usage entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub type_field: String,
    pub text: String,
}

/// Thinking content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub type_field: String,
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

/// Image content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub type_field: String,
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Tool call content block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub input: JsonValue,
}

/// User content block (text or image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

/// Assistant content block (text, thinking, or tool call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCallContent),
}

/// Tool content block (text or image).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolContent {
    Text(TextContent),
    Image(ImageContent),
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

/// User transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTranscriptItem {
    #[serde(rename = "type")]
    pub type_field: String,
    pub content: Vec<UserContent>,
}

/// Assistant transcript item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantTranscriptItem {
    #[serde(rename = "type")]
    pub type_field: String,
    pub content: Vec<AssistantContent>,
}

/// Tool transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTranscriptItem {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub content: Vec<ToolContent>,
}

/// A single transcript entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TranscriptItem {
    User(UserTranscriptItem),
    Assistant(AssistantTranscriptItem),
    Tool(ToolTranscriptItem),
}

/// Progress notification for a transcript item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptProgress {
    pub phase: SessionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Session metadata / snapshot
// ---------------------------------------------------------------------------

/// Metadata about a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub model: ModelRef,
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: ThinkingLevel,
    pub locked: bool,
    pub revision: u64,
    pub transcript: Vec<TranscriptItem>,
    #[serde(rename = "queuedSteer")]
    pub queued_steer: Vec<UserTranscriptItem>,
    #[serde(rename = "queuedSteerCount")]
    pub queued_steer_count: u64,
}

/// Full session snapshot (same as SessionMetadata in upstream).
pub type SessionSnapshot = SessionMetadata;

/// Server snapshot returned in the initial hello.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    #[serde(rename = "serverId")]
    pub server_id: String,
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    pub revision: u64,
    pub sessions: Vec<SessionMetadata>,
    pub models: Vec<ModelMetadata>,
}

// ---------------------------------------------------------------------------
// Commands (client → server)
// ---------------------------------------------------------------------------

/// A command sent from client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    List,
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "thinkingLevel")]
        thinking_level: Option<ThinkingLevel>,
    },
    Attach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Prompt {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    Steer {
        #[serde(rename = "sessionId")]
        session_id: String,
        text: String,
    },
    Abort {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "set_model")]
    SetModel {
        #[serde(rename = "sessionId")]
        session_id: String,
        model: ModelRef,
    },
    #[serde(rename = "set_thinking")]
    SetThinking {
        #[serde(rename = "sessionId")]
        session_id: String,
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
    List {
        sessions: Vec<SessionMetadata>,
    },
    Create {
        session: SessionSnapshot,
    },
    Attach {
        session: SessionSnapshot,
    },
    Detach {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Prompt {
        session: SessionSnapshot,
    },
    Steer {
        session: SessionSnapshot,
    },
    Abort {
        session: SessionSnapshot,
    },
    #[serde(rename = "set_model")]
    SetModel {
        session: SessionSnapshot,
    },
    #[serde(rename = "set_thinking")]
    SetThinking {
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
    #[serde(rename = "server_snapshot")]
    ServerSnapshot { snapshot: ServerSnapshot },
    #[serde(rename = "session_snapshot")]
    SessionSnapshot { snapshot: SessionSnapshot },
    #[serde(rename = "session_progress")]
    SessionProgress {
        #[serde(rename = "sessionId")]
        session_id: String,
        progress: TranscriptProgress,
    },
    #[serde(rename = "session_removed")]
    SessionRemoved {
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
    Hello { version: u32 },
    Request { id: String, request: Command },
}

/// A message from the server to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        version: u32,
        #[serde(rename = "connectionId")]
        connection_id: String,
        snapshot: ServerSnapshot,
    },
    HelloError {
        error: ProtocolError,
    },
    Response {
        id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CommandResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ProtocolError>,
    },
    Event {
        event: ServerEvent,
    },
}
