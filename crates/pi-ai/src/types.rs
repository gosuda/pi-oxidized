//! Wire-compatible model, message, tool, and streaming event contracts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// Open API identifier used to select a provider transport implementation.
pub type Api = String;

/// Open provider identifier used to select credentials and model catalogs.
pub type ProviderId = String;

/// User-selectable reasoning effort.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    Xhigh,
    /// Maximum reasoning effort.
    Max,
}

/// Reasoning effort supported by a model, including disabled reasoning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelThinkingLevel {
    /// Reasoning is disabled.
    Off,
    /// Minimal reasoning effort.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort.
    Xhigh,
    /// Maximum reasoning effort.
    Max,
}

/// Provider-specific values for model thinking levels.
///
/// A missing key uses the provider default, while a present `None` value marks
/// that level as unsupported and is encoded as JSON `null`.
pub type ThinkingLevelMap = BTreeMap<ModelThinkingLevel, Option<String>>;

/// Prompt-cache retention preference.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    /// Disable prompt caching.
    None,
    /// Request short-lived prompt caching.
    Short,
    /// Request long-lived prompt caching.
    Long,
}

/// Streaming transport preference.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Server-sent events.
    Sse,
    /// A fresh WebSocket connection.
    Websocket,
    /// A cached WebSocket connection.
    WebsocketCached,
    /// Let the provider choose the transport.
    Auto,
}

/// Input modality accepted by a model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    /// Plain text input.
    Text,
    /// Image input.
    Image,
}

/// Terminal reason recorded on an assistant message.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum StopReason {
    /// The provider completed normally.
    #[serde(rename = "stop")]
    Stop,
    /// The provider reached its output limit.
    #[serde(rename = "length")]
    Length,
    /// The provider requested one or more tools.
    #[serde(rename = "toolUse")]
    ToolUse,
    /// The provider failed.
    #[serde(rename = "error")]
    Error,
    /// The request was cancelled.
    #[serde(rename = "aborted")]
    Aborted,
}

/// Successful stream termination reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DoneReason {
    /// The provider completed normally.
    #[serde(rename = "stop")]
    Stop,
    /// The provider reached its output limit.
    #[serde(rename = "length")]
    Length,
    /// The provider requested one or more tools.
    #[serde(rename = "toolUse")]
    ToolUse,
}

/// Failed stream termination reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorReason {
    /// The request was cancelled.
    Aborted,
    /// The provider failed.
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum TextContentType {
    #[serde(rename = "text")]
    Text,
}

/// A text block in a message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    #[serde(rename = "type")]
    kind: TextContentType,
    /// UTF-8 text carried by the block.
    pub text: String,
    /// Provider-specific text signature or response metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

impl TextContent {
    /// Creates a text block with no provider signature.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: TextContentType::Text,
            text: text.into(),
            text_signature: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ThinkingContentType {
    #[serde(rename = "thinking")]
    Thinking,
}

/// A provider reasoning block.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    kind: ThinkingContentType,
    /// Human-readable or redacted reasoning text.
    pub thinking: String,
    /// Provider-specific reasoning signature or encrypted payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// Whether safety filters redacted the reasoning text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
}

impl ThinkingContent {
    /// Creates a reasoning block with no signature or redaction marker.
    #[must_use]
    pub fn new(thinking: impl Into<String>) -> Self {
        Self {
            kind: ThinkingContentType::Thinking,
            thinking: thinking.into(),
            thinking_signature: None,
            redacted: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ImageContentType {
    #[serde(rename = "image")]
    Image,
}

/// A base64-encoded image block.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    #[serde(rename = "type")]
    kind: ImageContentType,
    /// Base64-encoded image bytes.
    pub data: String,
    /// MIME type of the encoded image.
    pub mime_type: String,
}

impl ImageContent {
    /// Creates an image block.
    #[must_use]
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            kind: ImageContentType::Image,
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ToolCallType {
    #[serde(rename = "toolCall")]
    ToolCall,
}

/// A tool invocation emitted by an assistant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    kind: ToolCallType,
    /// Provider-assigned invocation identifier.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// JSON object passed to the tool.
    pub arguments: Map<String, Value>,
    /// Google-specific opaque signature for reusing thought context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl ToolCall {
    /// Creates a tool invocation with object arguments and no thought signature.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Map<String, Value>,
    ) -> Self {
        Self {
            kind: ToolCallType::ToolCall,
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
        }
    }
}

/// Monetary cost associated with one usage record.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    /// Input-token cost in US dollars.
    #[serde(default)]
    pub input: f64,
    /// Output-token cost in US dollars.
    #[serde(default)]
    pub output: f64,
    /// Cache-read cost in US dollars.
    #[serde(default)]
    pub cache_read: f64,
    /// Cache-write cost in US dollars.
    #[serde(default)]
    pub cache_write: f64,
    /// Total request cost in US dollars.
    #[serde(default)]
    pub total: f64,
}

/// Token usage and monetary cost for an assistant response.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Input tokens consumed.
    #[serde(default)]
    pub input: u64,
    /// Output tokens produced, including reasoning tokens.
    #[serde(default)]
    pub output: u64,
    /// Cached input tokens read.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to cache.
    #[serde(default)]
    pub cache_write: u64,
    /// Cache-write tokens stored with one-hour retention, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write1h: Option<u64>,
    /// Reasoning tokens, as a subset of output tokens, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// Total tokens reported by the provider.
    #[serde(default)]
    pub total_tokens: u64,
    /// Monetary cost for the request.
    #[serde(default)]
    pub cost: UsageCost,
}

/// String-or-number error code included in a diagnostic.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum DiagnosticCode {
    /// Textual error code.
    String(String),
    /// Numeric error code.
    Number(Number),
}

/// Redacted details about an error associated with an assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DiagnosticErrorInfo {
    /// Error class or runtime name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable error message.
    pub message: String,
    /// Redacted stack trace, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// Provider or runtime error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<DiagnosticCode>,
}

/// Redacted provider or runtime diagnostic attached to an assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AssistantMessageDiagnostic {
    /// Diagnostic category.
    #[serde(rename = "type")]
    pub kind: String,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// Structured error information, when the diagnostic represents an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DiagnosticErrorInfo>,
    /// Additional diagnostic properties.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

/// A user message array element.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Text input.
    Text(TextContent),
    /// Image input.
    Image(ImageContent),
}

/// Content accepted by a user message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum UserMessageContent {
    /// A plain text prompt.
    Text(String),
    /// Structured text and image blocks.
    Blocks(Vec<UserContent>),
}

/// Content emitted by an assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AssistantContent {
    /// Text output.
    Text(TextContent),
    /// Provider reasoning output.
    Thinking(ThinkingContent),
    /// A requested tool invocation.
    ToolCall(ToolCall),
}

/// Content returned by a tool.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    /// Text output from a tool.
    Text(TextContent),
    /// Image output from a tool.
    Image(ImageContent),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum UserRole {
    #[serde(rename = "user")]
    User,
}

/// A user-authored conversation message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UserMessage {
    role: UserRole,
    /// User prompt content.
    pub content: UserMessageContent,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

impl UserMessage {
    /// Creates a user message with the required literal role.
    #[must_use]
    pub fn new(content: UserMessageContent, timestamp: i64) -> Self {
        Self {
            role: UserRole::User,
            content,
            timestamp,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum AssistantRole {
    #[serde(rename = "assistant")]
    Assistant,
}

/// A provider-produced assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    role: AssistantRole,
    /// Ordered assistant content blocks.
    pub content: Vec<AssistantContent>,
    /// API shape used for the request.
    pub api: Api,
    /// Provider used for the request.
    pub provider: ProviderId,
    /// Requested model identifier.
    pub model: String,
    /// Concrete response model when it differs from the requested model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// Provider-specific response or message identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Redacted provider and runtime diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<AssistantMessageDiagnostic>>,
    /// Token usage and cost.
    pub usage: Usage,
    /// Terminal response reason.
    pub stop_reason: StopReason,
    /// Error description for failed or aborted responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssistantMessageMetadata<'a> {
    role: &'a AssistantRole,
    api: &'a str,
    provider: &'a str,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<&'a [AssistantMessageDiagnostic]>,
    usage: &'a Usage,
    stop_reason: &'a StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<&'a str>,
    timestamp: i64,
}

impl AssistantMessage {
    /// Creates an assistant message with the required literal role.
    #[must_use]
    pub fn new(
        api: impl Into<Api>,
        provider: impl Into<ProviderId>,
        model: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            role: AssistantRole::Assistant,
            content: Vec::new(),
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp,
        }
    }

    /// Returns a borrowed wire view without the growing content array.
    ///
    /// Streaming hook payloads use this view so each delta serializes in
    /// constant time with respect to accumulated assistant content.
    #[must_use]
    pub fn metadata_view(&self) -> impl Serialize + '_ {
        let Self {
            role,
            content: _,
            api,
            provider,
            model,
            response_model,
            response_id,
            diagnostics,
            usage,
            stop_reason,
            error_message,
            timestamp,
        } = self;
        AssistantMessageMetadata {
            role,
            api,
            provider,
            model,
            response_model: response_model.as_deref(),
            response_id: response_id.as_deref(),
            diagnostics: diagnostics.as_deref(),
            usage,
            stop_reason,
            error_message: error_message.as_deref(),
            timestamp: *timestamp,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ToolResultRole {
    #[serde(rename = "toolResult")]
    ToolResult,
}

/// A tool execution result added to the conversation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    role: ToolResultRole,
    /// Identifier of the corresponding tool call.
    pub tool_call_id: String,
    /// Name of the invoked tool.
    pub tool_name: String,
    /// Text and image output returned by the tool.
    pub content: Vec<ToolResultContent>,
    /// Tool-specific structured details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Tool names made available after this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    /// Whether tool execution failed.
    pub is_error: bool,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

impl ToolResultMessage {
    /// Creates a tool result with the required literal role.
    #[must_use]
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: Vec<ToolResultContent>,
        is_error: bool,
        timestamp: i64,
    ) -> Self {
        Self {
            role: ToolResultRole::ToolResult,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content,
            details: None,
            added_tool_names: None,
            is_error,
            timestamp,
        }
    }
}

/// A conversation message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Message {
    /// User-authored message.
    User(UserMessage),
    /// Provider-produced assistant message.
    Assistant(AssistantMessage),
    /// Tool execution result.
    ToolResult(ToolResultMessage),
}

/// Tool definition made available to a provider.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Tool {
    /// Unique tool name.
    pub name: String,
    /// Human-readable tool description.
    pub description: String,
    /// TypeBox-compatible JSON Schema for tool arguments.
    pub parameters: Value,
}

/// Complete provider input context.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// Optional system instruction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Ordered conversation history.
    pub messages: Vec<Message>,
    /// Optional tool definitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

/// Per-million-token model prices.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    /// Input-token price in US dollars per million tokens.
    pub input: f64,
    /// Output-token price in US dollars per million tokens.
    pub output: f64,
    /// Cache-read price in US dollars per million tokens.
    pub cache_read: f64,
    /// Cache-write price in US dollars per million tokens.
    pub cache_write: f64,
}

/// Request-wide model pricing tier.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    /// Input-token price in US dollars per million tokens.
    pub input: f64,
    /// Output-token price in US dollars per million tokens.
    pub output: f64,
    /// Cache-read price in US dollars per million tokens.
    pub cache_read: f64,
    /// Cache-write price in US dollars per million tokens.
    pub cache_write: f64,
    /// Input-token threshold above which this tier applies.
    pub input_tokens_above: u64,
}

/// Model pricing, optionally including request-wide tiers.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    /// Input-token price in US dollars per million tokens.
    pub input: f64,
    /// Output-token price in US dollars per million tokens.
    pub output: f64,
    /// Cache-read price in US dollars per million tokens.
    pub cache_read: f64,
    /// Cache-write price in US dollars per million tokens.
    pub cache_write: f64,
    /// Request-wide pricing tiers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<ModelCostTier>>,
}

/// Provider model metadata, including preserved compatibility extensions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// Provider model identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// API shape used by the model.
    pub api: Api,
    /// Provider identifier.
    pub provider: ProviderId,
    /// Provider endpoint base URL.
    pub base_url: String,
    /// Whether the model supports reasoning.
    pub reasoning: bool,
    /// Provider-specific mapping of supported thinking levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// Accepted input modalities.
    pub input: Vec<ModelInput>,
    /// Model pricing.
    pub cost: ModelCost,
    /// Context-window size in tokens.
    pub context_window: u64,
    /// Maximum output tokens.
    pub max_tokens: u64,
    /// Additional static request headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// API-specific compatibility settings preserved without reshaping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    /// Unknown catalog fields preserved across round trips.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Semantic events emitted while assembling an assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum AssistantMessageEvent {
    /// Begins a response stream.
    #[serde(rename = "start")]
    Start {
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Begins a text block.
    #[serde(rename = "text_start")]
    TextStart {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Appends text to a text block.
    #[serde(rename = "text_delta")]
    TextDelta {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Newly emitted text.
        delta: String,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Completes a text block.
    #[serde(rename = "text_end")]
    TextEnd {
        /// Index of the completed content block.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Complete text content.
        content: String,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Begins a reasoning block.
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Appends text to a reasoning block.
    #[serde(rename = "thinking_delta")]
    ThinkingDelta {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Newly emitted reasoning text.
        delta: String,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Completes a reasoning block.
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        /// Index of the completed content block.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Complete reasoning text.
        content: String,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Begins a tool call block.
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Appends serialized arguments to a tool call block.
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta {
        /// Index of the content block being updated.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Newly emitted serialized argument fragment.
        delta: String,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Completes a tool call block.
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        /// Index of the completed content block.
        #[serde(rename = "contentIndex")]
        content_index: u64,
        /// Completed tool call.
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
        /// Current assistant message snapshot.
        partial: AssistantMessage,
    },
    /// Completes a successful response stream.
    #[serde(rename = "done")]
    Done {
        /// Successful termination reason.
        reason: DoneReason,
        /// Final assistant message.
        message: AssistantMessage,
    },
    /// Completes a failed or cancelled response stream.
    #[serde(rename = "error")]
    Error {
        /// Failure termination reason.
        reason: ErrorReason,
        /// Final assistant message.
        error: AssistantMessage,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant() -> AssistantMessage {
        AssistantMessage::new("custom-api", "custom-provider", "model", 1_700_000_000_000)
    }

    #[test]
    fn sibling_content_tags_are_literal() -> Result<(), Box<dyn std::error::Error>> {
        let text = TextContent::new("hello");
        assert_eq!(
            serde_json::to_value(text)?,
            json!({"type": "text", "text": "hello"})
        );
        assert!(
            serde_json::from_value::<TextContent>(json!({
                "type": "image",
                "text": "hello"
            }))
            .is_err()
        );

        let image = ImageContent::new("AA==", "image/png");
        assert_eq!(
            serde_json::to_value(image)?,
            json!({"type": "image", "data": "AA==", "mimeType": "image/png"})
        );
        Ok(())
    }

    #[test]
    fn sibling_message_roles_are_literal() -> Result<(), Box<dyn std::error::Error>> {
        let message = UserMessage::new(UserMessageContent::Text("hi".into()), 7);
        assert_eq!(
            serde_json::to_value(message)?,
            json!({"role": "user", "content": "hi", "timestamp": 7})
        );
        assert!(
            serde_json::from_value::<UserMessage>(json!({
                "role": "assistant",
                "content": "hi",
                "timestamp": 7
            }))
            .is_err()
        );

        let assistant = Message::Assistant(assistant());
        let assistant_json = serde_json::to_value(&assistant)?;
        assert_eq!(assistant_json["role"], "assistant");
        assert_eq!(
            serde_json::from_value::<Message>(assistant_json)?,
            assistant
        );

        let tool_result = Message::ToolResult(ToolResultMessage::new(
            "call-1",
            "read",
            Vec::new(),
            false,
            8,
        ));
        let tool_result_json = serde_json::to_value(&tool_result)?;
        assert_eq!(tool_result_json["role"], "toolResult");
        assert_eq!(
            serde_json::from_value::<Message>(tool_result_json)?,
            tool_result
        );
        Ok(())
    }

    #[test]
    fn events_use_exact_tags_fields_and_tool_use() -> Result<(), Box<dyn std::error::Error>> {
        let events = [
            AssistantMessageEvent::Start {
                partial: assistant(),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: assistant(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: assistant(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "x".into(),
                partial: assistant(),
            },
            AssistantMessageEvent::ThinkingStart {
                content_index: 1,
                partial: assistant(),
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 1,
                delta: "x".into(),
                partial: assistant(),
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 1,
                content: "x".into(),
                partial: assistant(),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 2,
                partial: assistant(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 2,
                delta: "{}".into(),
                partial: assistant(),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 2,
                tool_call: ToolCall::new("call-1", "read", Map::new()),
                partial: assistant(),
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::ToolUse,
                message: assistant(),
            },
            AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: assistant(),
            },
        ];
        let encoded = events
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        let tags = encoded
            .iter()
            .map(|event| &event["type"])
            .collect::<Vec<_>>();
        assert_eq!(
            tags,
            [
                "start",
                "text_start",
                "text_delta",
                "text_end",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_end",
                "done",
                "error",
            ]
        );
        assert_eq!(encoded[9]["contentIndex"], 2);
        assert_eq!(encoded[9]["toolCall"]["type"], "toolCall");
        assert_eq!(encoded[10]["reason"], "toolUse");
        assert_eq!(encoded[10]["message"]["role"], "assistant");
        assert!(encoded[10].get("error").is_none());
        assert_eq!(encoded[11]["error"]["role"], "assistant");
        assert!(encoded[11].get("message").is_none());
        Ok(())
    }

    #[test]
    fn optional_fields_are_omitted() -> Result<(), Box<dyn std::error::Error>> {
        let value = serde_json::to_value(assistant())?;
        for key in ["responseModel", "responseId", "diagnostics", "errorMessage"] {
            assert!(value.get(key).is_none(), "unexpected field {key}");
        }
        assert!(value["usage"].get("cacheWrite1h").is_none());
        assert!(value["usage"].get("reasoning").is_none());
        Ok(())
    }

    #[test]
    fn thinking_level_map_preserves_null_values() -> Result<(), Box<dyn std::error::Error>> {
        let map: ThinkingLevelMap = serde_json::from_value(json!({
            "off": null,
            "high": "high"
        }))?;
        assert_eq!(
            serde_json::to_value(map)?,
            json!({"off": null, "high": "high"})
        );
        Ok(())
    }

    #[test]
    fn model_preserves_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
        let input = json!({
            "id": "m",
            "name": "Model",
            "api": "future-api",
            "provider": "future-provider",
            "baseUrl": "https://example.test",
            "reasoning": false,
            "input": ["text"],
            "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
            "contextWindow": 1000,
            "maxTokens": 100,
            "futureField": {"nested": true}
        });
        let model: Model = serde_json::from_value(input.clone())?;
        assert_eq!(serde_json::to_value(model)?, input);
        Ok(())
    }

    #[test]
    fn tool_arguments_must_be_objects() -> Result<(), Box<dyn std::error::Error>> {
        let input = json!({
            "type": "toolCall",
            "id": "1",
            "name": "read",
            "arguments": {"path": "a.txt"}
        });
        let valid: ToolCall = serde_json::from_value(input.clone())?;
        assert_eq!(serde_json::to_value(valid)?, input);

        for invalid in [json!(null), json!([]), json!("x"), json!(1)] {
            assert!(
                serde_json::from_value::<ToolCall>(json!({
                    "type": "toolCall",
                    "id": "1",
                    "name": "read",
                    "arguments": invalid
                }))
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn done_and_error_reasons_reject_the_other_domain() -> Result<(), Box<dyn std::error::Error>> {
        assert!(serde_json::from_value::<DoneReason>(json!("error")).is_err());
        assert!(serde_json::from_value::<DoneReason>(json!("aborted")).is_err());
        assert!(serde_json::from_value::<ErrorReason>(json!("stop")).is_err());
        assert!(serde_json::from_value::<ErrorReason>(json!("toolUse")).is_err());

        let mut invalid_done = serde_json::to_value(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: assistant(),
        })?;
        invalid_done["reason"] = json!("error");
        assert!(serde_json::from_value::<AssistantMessageEvent>(invalid_done).is_err());
        Ok(())
    }
}
