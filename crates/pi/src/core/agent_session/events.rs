//! Product-level session events (superset of `pi_agent::AgentEvent`).
//!
//! Serialized RAW (tagged `type`, `snake_case` variants, camelCase payload
//! fields) for json/rpc parity. `agent_end` is rewritten with `willRetry`.

use pi_agent::{AgentEvent, AgentMessage, AgentToolResult};
use pi_ai::{AssistantMessageEvent, ModelThinkingLevel, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::compaction::CompactionResult;
use crate::core::sessions::SessionEntry;

/// Compaction trigger reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// User-initiated `/compact`.
    Manual,
    /// Threshold-based auto compaction.
    Threshold,
    /// Context-overflow recovery.
    Overflow,
}

/// Session-specific events that extend the core agent event surface.
///
/// Wire tags and field names match TypeScript `AgentSessionEvent`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentSessionEvent {
    /// A new agent run has started.
    AgentStart,
    /// The agent run finished; `will_retry` is true when auto-retry will continue.
    AgentEnd {
        /// Messages produced by this run.
        messages: Vec<AgentMessage>,
        /// Whether session-level auto-retry will continue the run.
        #[serde(rename = "willRetry")]
        will_retry: bool,
    },
    /// A turn is about to begin.
    TurnStart,
    /// A turn finished with an assistant message and any tool results.
    TurnEnd {
        /// Assistant message that completed the turn.
        message: AgentMessage,
        /// Tool-result messages for this turn.
        #[serde(rename = "toolResults")]
        tool_results: Vec<ToolResultMessage>,
    },
    /// A transcript message is starting.
    MessageStart {
        /// Message snapshot at start.
        message: AgentMessage,
    },
    /// An assistant message was updated during streaming.
    MessageUpdate {
        /// Latest assistant message snapshot.
        message: AgentMessage,
        /// Underlying provider stream event.
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: Box<AssistantMessageEvent>,
    },
    /// A transcript message has ended.
    MessageEnd {
        /// Final message snapshot.
        message: AgentMessage,
    },
    /// Tool execution is starting for one tool call.
    ToolExecutionStart {
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Validated tool arguments.
        args: Map<String, Value>,
    },
    /// Tool execution produced a partial result update.
    ToolExecutionUpdate {
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Validated tool arguments.
        args: Map<String, Value>,
        /// Partial tool result.
        #[serde(rename = "partialResult")]
        partial_result: AgentToolResult,
    },
    /// Tool execution finished for one tool call.
    ToolExecutionEnd {
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Final tool result.
        result: AgentToolResult,
        /// Whether the result is treated as an error.
        #[serde(rename = "isError")]
        is_error: bool,
    },
    /// Session-level idle after retries / compaction / queued continuations.
    AgentSettled,
    /// Mirror of pending steering / follow-up queue text for UI.
    QueueUpdate {
        /// Pending steering message texts.
        steering: Vec<String>,
        /// Pending follow-up message texts.
        #[serde(rename = "followUp")]
        follow_up: Vec<String>,
    },
    /// Compaction is starting.
    CompactionStart {
        /// Why compaction was triggered.
        reason: CompactionReason,
    },
    /// Compaction finished.
    CompactionEnd {
        /// Why compaction was triggered.
        reason: CompactionReason,
        /// Result when compaction produced a summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<CompactionResult>,
        /// Whether compaction was aborted.
        aborted: bool,
        /// Whether the session will retry after this compaction.
        #[serde(rename = "willRetry")]
        will_retry: bool,
        /// Error text when compaction failed.
        #[serde(
            rename = "errorMessage",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        error_message: Option<String>,
    },
    /// A session entry was appended.
    EntryAppended {
        /// Appended entry.
        entry: SessionEntry,
    },
    /// Session display name changed.
    SessionInfoChanged {
        /// New name (`None` clears).
        name: Option<String>,
    },
    /// Thinking level changed.
    ThinkingLevelChanged {
        /// New thinking level.
        level: ModelThinkingLevel,
    },
    /// Auto-retry backoff is starting.
    AutoRetryStart {
        /// Current attempt (1-based).
        attempt: u32,
        /// Configured max attempts.
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        /// Backoff delay in milliseconds.
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        /// Error that triggered the retry.
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// Auto-retry finished (success, exhaustion, or cancel).
    AutoRetryEnd {
        /// Whether a later assistant response succeeded.
        success: bool,
        /// Attempt count at end.
        attempt: u32,
        /// Final error when unsuccessful.
        #[serde(
            rename = "finalError",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        final_error: Option<String>,
    },
}

impl AgentSessionEvent {
    /// Convert a core agent event into a session event.
    ///
    /// `agent_end` requires `will_retry` from the session retry policy.
    #[must_use]
    pub fn from_agent_event(event: AgentEvent, will_retry: bool) -> Self {
        match event {
            AgentEvent::AgentStart => Self::AgentStart,
            AgentEvent::AgentEnd { messages } => Self::AgentEnd {
                messages,
                will_retry,
            },
            AgentEvent::TurnStart => Self::TurnStart,
            AgentEvent::TurnEnd {
                message,
                tool_results,
            } => Self::TurnEnd {
                message,
                tool_results,
            },
            AgentEvent::MessageStart { message } => Self::MessageStart { message },
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => Self::MessageUpdate {
                message,
                assistant_message_event,
            },
            AgentEvent::MessageEnd { message } => Self::MessageEnd { message },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => Self::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Self::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Self::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            },
        }
    }

    /// Wire `type` discriminant.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
            Self::AgentSettled => "agent_settled",
            Self::QueueUpdate { .. } => "queue_update",
            Self::CompactionStart { .. } => "compaction_start",
            Self::CompactionEnd { .. } => "compaction_end",
            Self::EntryAppended { .. } => "entry_appended",
            Self::SessionInfoChanged { .. } => "session_info_changed",
            Self::ThinkingLevelChanged { .. } => "thinking_level_changed",
            Self::AutoRetryStart { .. } => "auto_retry_start",
            Self::AutoRetryEnd { .. } => "auto_retry_end",
        }
    }
}

/// Listener invoked for every public session event.
pub type AgentSessionEventListener = Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>;

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::user_text;
    use serde_json::json;

    #[test]
    fn agent_end_wire_includes_will_retry() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::AgentEnd {
            messages: vec![user_text("hi", std::iter::empty())],
            will_retry: true,
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(value["type"], json!("agent_end"));
        assert_eq!(value["willRetry"], json!(true));
        assert!(value["messages"].is_array());
        Ok(())
    }

    #[test]
    fn queue_update_wire_camel_case() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::QueueUpdate {
            steering: vec!["a".into()],
            follow_up: vec!["b".into()],
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(value["type"], json!("queue_update"));
        assert_eq!(value["steering"], json!(["a"]));
        assert_eq!(value["followUp"], json!(["b"]));
        Ok(())
    }

    #[test]
    fn agent_settled_is_tag_only() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::AgentSettled;
        let value = serde_json::to_value(&event)?;
        assert_eq!(value, json!({"type": "agent_settled"}));
        Ok(())
    }
}
