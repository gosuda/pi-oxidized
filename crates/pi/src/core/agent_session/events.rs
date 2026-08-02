//! Product-level session events (superset of `pi_agent::AgentEvent`).
//!
//! Serialized RAW (tagged `type`, `snake_case` variants, camelCase payload
//! fields) for json/rpc parity. `agent_end` is rewritten with `willRetry`.

use pi_agent::{AgentEvent, AgentMessage, AgentToolResult};
use pi_ai::{AssistantMessageEvent, Model, ModelThinkingLevel, ToolResultMessage};
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

/// Source of a summarization retry attempt (mirrors TS `_summarizationRetryCallbacks`).
///
/// `branchSummary` carries no reason; `compaction` carries the trigger reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "camelCase")]
pub enum SummarizationRetrySource {
    /// Branch-summary summarization retry.
    BranchSummary,
    /// Compaction summarization retry.
    Compaction {
        /// Why compaction was triggered.
        reason: CompactionReason,
    },
}

/// Why a session replacement is about to occur.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBeforeSwitchReason {
    /// A new session is being created.
    New,
    /// An existing or imported session is being resumed.
    Resume,
}

/// Reason passed to `session_start`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    /// First bind for this session.
    #[default]
    Startup,
    /// Bind after `/reload`.
    Reload,
    /// Bind after a new-session replacement.
    New,
    /// Bind after resume/switch/import.
    Resume,
    /// Bind after a fork replacement.
    Fork,
}

impl SessionStartReason {
    /// Wire discriminant matching TS.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Reload => "reload",
            Self::New => "new",
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

/// Reason passed to `session_shutdown`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionShutdownReason {
    /// New session replacing this one.
    New,
    /// Resume/switch/import replacing this one.
    Resume,
    /// Fork replacing this one.
    Fork,
    /// `/reload`.
    Reload,
    /// Runtime disposal.
    Quit,
}

impl SessionShutdownReason {
    /// Wire discriminant matching TS.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Reload => "reload",
            Self::Quit => "quit",
        }
    }
}

/// Session-start metadata stored at construction and emitted on first bind.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionStartEvent {
    /// Why this session started.
    pub reason: SessionStartReason,
    /// Previously active session file (new/resume/fork).
    pub previous_session_file: Option<String>,
}

/// Position used for a session fork.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBeforeForkPosition {
    /// Fork from the selected user message's parent.
    Before,
    /// Fork at the selected entry.
    At,
}

/// Source of a model selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSelectSource {
    /// Explicit model selection.
    Set,
    /// Model cycling.
    Cycle,
    /// Session/runtime restoration.
    Restore,
}

/// Session-specific events that extend the core agent event surface.
///
/// Wire tags and field names match TypeScript `AgentSessionEvent`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentSessionEvent {
    /// Extensions may cancel a pending session switch.
    SessionBeforeSwitch {
        /// Whether this creates a new session or resumes one.
        reason: SessionBeforeSwitchReason,
        /// Target session file for resume/import operations.
        #[serde(
            rename = "targetSessionFile",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        target_session_file: Option<String>,
    },
    /// Extensions may cancel a pending session fork.
    SessionBeforeFork {
        /// Selected session entry.
        #[serde(rename = "entryId")]
        entry_id: String,
        /// Whether the fork starts before or at the selected entry.
        position: SessionBeforeForkPosition,
    },
    /// Extension-host-facing session start (never routed through
    /// `emit_public`; the public event stream excludes it, matching TS).
    SessionStart {
        /// Why this session started.
        reason: SessionStartReason,
        /// Previously active session file (new/resume/fork).
        #[serde(
            rename = "previousSessionFile",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        previous_session_file: Option<String>,
    },
    /// Extension-host-facing session shutdown (never routed through
    /// `emit_public`).
    SessionShutdown {
        /// Why this session is shutting down.
        reason: SessionShutdownReason,
        /// Session file replacing this one (new/resume/fork).
        #[serde(
            rename = "targetSessionFile",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        target_session_file: Option<String>,
    },
    /// A new model was selected.
    ModelSelect {
        /// Newly selected model (boxed to keep the enum small; wire unchanged).
        model: Box<Model>,
        /// Previously selected model, absent during initial restoration.
        #[serde(
            rename = "previousModel",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        previous_model: Option<Box<Model>>,
        /// Selection source.
        source: ModelSelectSource,
    },
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
        /// New name (`None` clears; omitted from wire when absent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// Summarization retry scheduled (backoff about to start).
    SummarizationRetryScheduled {
        /// Current retry attempt (1-based).
        attempt: u32,
        /// Configured max retry attempts.
        #[serde(rename = "maxAttempts")]
        max_attempts: u32,
        /// Backoff delay in milliseconds.
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        /// Error that triggered the retry.
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// Summarization retry attempt is starting (after backoff).
    SummarizationRetryAttemptStart {
        /// Source + optional reason for the retry (flattened into the event).
        #[serde(flatten)]
        source: SummarizationRetrySource,
    },
    /// Summarization retry finished (success, exhaustion, or cancel).
    SummarizationRetryFinished,
    /// Bash execution produced a streaming output delta.
    BashExecutionUpdate {
        /// Optional tool-call / execution id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Output delta text.
        delta: String,
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
            Self::SessionBeforeSwitch { .. } => "session_before_switch",
            Self::SessionBeforeFork { .. } => "session_before_fork",
            Self::SessionStart { .. } => "session_start",
            Self::SessionShutdown { .. } => "session_shutdown",
            Self::ModelSelect { .. } => "model_select",
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
            Self::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
            Self::SummarizationRetryAttemptStart { .. } => "summarization_retry_attempt_start",
            Self::SummarizationRetryFinished => "summarization_retry_finished",
            Self::BashExecutionUpdate { .. } => "bash_execution_update",
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
    fn lifecycle_events_use_reference_wire_payloads() -> Result<(), serde_json::Error> {
        let switch = serde_json::to_value(AgentSessionEvent::SessionBeforeSwitch {
            reason: SessionBeforeSwitchReason::Resume,
            target_session_file: Some("/tmp/session.jsonl".into()),
        })?;
        assert_eq!(
            switch,
            json!({
                "type": "session_before_switch",
                "reason": "resume",
                "targetSessionFile": "/tmp/session.jsonl"
            })
        );

        let fork = serde_json::to_value(AgentSessionEvent::SessionBeforeFork {
            entry_id: "entry-1".into(),
            position: SessionBeforeForkPosition::Before,
        })?;
        assert_eq!(
            fork,
            json!({
                "type": "session_before_fork",
                "entryId": "entry-1",
                "position": "before"
            })
        );

        let model = pi_agent::state::default_model();
        let selected = serde_json::to_value(AgentSessionEvent::ModelSelect {
            model: Box::new(model.clone()),
            previous_model: Some(Box::new(model)),
            source: ModelSelectSource::Cycle,
        })?;
        assert_eq!(selected["type"], json!("model_select"));
        assert_eq!(selected["source"], json!("cycle"));
        assert!(selected.get("previousModel").is_some());
        Ok(())
    }

    #[test]
    fn optional_lifecycle_fields_are_omitted() -> Result<(), serde_json::Error> {
        let switch = serde_json::to_value(AgentSessionEvent::SessionBeforeSwitch {
            reason: SessionBeforeSwitchReason::New,
            target_session_file: None,
        })?;
        assert!(switch.get("targetSessionFile").is_none());
        Ok(())
    }

    #[test]
    fn agent_settled_is_tag_only() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::AgentSettled;
        let value = serde_json::to_value(&event)?;
        assert_eq!(value, json!({"type": "agent_settled"}));
        Ok(())
    }

    #[test]
    fn session_start_wire_shape_and_round_trip() -> Result<(), serde_json::Error> {
        let startup = AgentSessionEvent::SessionStart {
            reason: SessionStartReason::Startup,
            previous_session_file: None,
        };
        let value = serde_json::to_value(&startup)?;
        assert_eq!(value, json!({"type": "session_start", "reason": "startup"}));
        assert!(value.get("previousSessionFile").is_none());
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, startup);

        let resume = AgentSessionEvent::SessionStart {
            reason: SessionStartReason::Resume,
            previous_session_file: Some("/tmp/prev.jsonl".into()),
        };
        let value = serde_json::to_value(&resume)?;
        assert_eq!(
            value,
            json!({
                "type": "session_start",
                "reason": "resume",
                "previousSessionFile": "/tmp/prev.jsonl"
            })
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, resume);
        Ok(())
    }

    #[test]
    fn session_shutdown_wire_shape_and_round_trip() -> Result<(), serde_json::Error> {
        let quit = AgentSessionEvent::SessionShutdown {
            reason: SessionShutdownReason::Quit,
            target_session_file: None,
        };
        let value = serde_json::to_value(&quit)?;
        assert_eq!(value, json!({"type": "session_shutdown", "reason": "quit"}));
        assert!(value.get("targetSessionFile").is_none());
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, quit);

        let new = AgentSessionEvent::SessionShutdown {
            reason: SessionShutdownReason::New,
            target_session_file: Some("/tmp/next.jsonl".into()),
        };
        let value = serde_json::to_value(&new)?;
        assert_eq!(
            value,
            json!({
                "type": "session_shutdown",
                "reason": "new",
                "targetSessionFile": "/tmp/next.jsonl"
            })
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, new);
        Ok(())
    }

    #[test]
    fn lifecycle_reason_strings_match_wire_contract() {
        assert_eq!(
            [
                SessionStartReason::Startup.as_str(),
                SessionStartReason::Reload.as_str(),
                SessionStartReason::New.as_str(),
                SessionStartReason::Resume.as_str(),
                SessionStartReason::Fork.as_str(),
            ],
            ["startup", "reload", "new", "resume", "fork"]
        );
        assert_eq!(
            [
                SessionShutdownReason::Quit.as_str(),
                SessionShutdownReason::Reload.as_str(),
                SessionShutdownReason::New.as_str(),
                SessionShutdownReason::Resume.as_str(),
                SessionShutdownReason::Fork.as_str(),
            ],
            ["quit", "reload", "new", "resume", "fork"]
        );
        assert_eq!(SessionStartReason::default(), SessionStartReason::Startup);
    }

    #[test]
    fn session_info_changed_clear_omits_name() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SessionInfoChanged { name: None };
        let value = serde_json::to_value(&event)?;
        assert_eq!(value, json!({"type": "session_info_changed"}));
        assert!(value.get("name").is_none());
        // Round-trip: None deserializes back.
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn session_info_changed_rename_includes_name() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SessionInfoChanged {
            name: Some("hello".into()),
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({"type": "session_info_changed", "name": "hello"})
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn session_info_changed_deserializes_name_null() -> Result<(), serde_json::Error> {
        // TS sends `name: null` on clear; must deserialize to None.
        let value = json!({"type": "session_info_changed", "name": null});
        let event: AgentSessionEvent = serde_json::from_value(value)?;
        assert_eq!(event, AgentSessionEvent::SessionInfoChanged { name: None });
        Ok(())
    }

    #[test]
    fn summarization_retry_scheduled_wire_shape() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SummarizationRetryScheduled {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "overloaded".into(),
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({
                "type": "summarization_retry_scheduled",
                "attempt": 1,
                "maxAttempts": 3,
                "delayMs": 2000,
                "errorMessage": "overloaded"
            })
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn summarization_retry_attempt_start_branch_summary_wire() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::BranchSummary,
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({
                "type": "summarization_retry_attempt_start",
                "source": "branchSummary"
            })
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn summarization_retry_attempt_start_compaction_wire() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::Compaction {
                reason: CompactionReason::Manual,
            },
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({
                "type": "summarization_retry_attempt_start",
                "source": "compaction",
                "reason": "manual"
            })
        );
        // Also verify threshold/overflow reasons.
        let threshold = AgentSessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::Compaction {
                reason: CompactionReason::Threshold,
            },
        };
        assert_eq!(
            serde_json::to_value(&threshold)?["reason"],
            json!("threshold")
        );
        let overflow = AgentSessionEvent::SummarizationRetryAttemptStart {
            source: SummarizationRetrySource::Compaction {
                reason: CompactionReason::Overflow,
            },
        };
        assert_eq!(
            serde_json::to_value(&overflow)?["reason"],
            json!("overflow")
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn summarization_retry_finished_wire_shape() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::SummarizationRetryFinished;
        let value = serde_json::to_value(&event)?;
        assert_eq!(value, json!({"type": "summarization_retry_finished"}));
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn bash_execution_update_wire_with_id() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::BashExecutionUpdate {
            id: Some("call-1".into()),
            delta: "hello\n".into(),
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({
                "type": "bash_execution_update",
                "id": "call-1",
                "delta": "hello\n"
            })
        );
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }

    #[test]
    fn bash_execution_update_wire_without_id() -> Result<(), serde_json::Error> {
        let event = AgentSessionEvent::BashExecutionUpdate {
            id: None,
            delta: "world".into(),
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(
            value,
            json!({
                "type": "bash_execution_update",
                "delta": "world"
            })
        );
        assert!(value.get("id").is_none());
        assert_eq!(serde_json::from_value::<AgentSessionEvent>(value)?, event);
        Ok(())
    }
}
