//! Agent runtime state and event-driven reducer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pi_ai::{Message, Model, ModelCost, ModelThinkingLevel};

use crate::event::AgentEvent;
use crate::message::AgentMessage;
use crate::tool::AgentTool;

/// Default placeholder model used when no model has been configured yet.
///
/// Matches the TypeScript `DEFAULT_MODEL` sentinel in `agent.ts`.
#[must_use]
pub fn default_model() -> Model {
    Model {
        id: "unknown".to_owned(),
        name: "unknown".to_owned(),
        api: "unknown".to_owned(),
        provider: "unknown".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    }
}

/// Mutable runtime state owned by the agent wrapper.
///
/// Transcript and tool bookkeeping update only through [`AgentState::reduce`],
/// which mirrors TypeScript `processEvents`. Lifecycle flags such as
/// [`AgentState::is_streaming`] are owned by the agent wrapper and are not
/// toggled by the reducer.
pub struct AgentState {
    /// System prompt sent with each model request.
    pub system_prompt: String,
    /// Active model used for future turns.
    pub model: Model,
    /// Requested reasoning level for future turns.
    pub thinking_level: ModelThinkingLevel,
    /// Available tools.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Conversation transcript.
    pub messages: Vec<AgentMessage>,
    /// True while the agent is processing a prompt or continuation.
    pub is_streaming: bool,
    /// Partial assistant message for the current streamed response, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: BTreeSet<String>,
    /// Error message from the most recent failed or aborted assistant turn.
    pub error_message: Option<String>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentState {
    /// Creates an empty idle state with the default unknown model.
    #[must_use]
    pub fn new() -> Self {
        Self {
            system_prompt: String::new(),
            model: default_model(),
            thinking_level: ModelThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
        }
    }

    /// Creates state from an initial system prompt, model, thinking level,
    /// tools, and transcript. Runtime fields start idle.
    #[must_use]
    pub fn with_initial(
        system_prompt: impl Into<String>,
        model: Model,
        thinking_level: ModelThinkingLevel,
        tools: Vec<Arc<dyn AgentTool>>,
        messages: Vec<AgentMessage>,
    ) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            model,
            thinking_level,
            tools,
            messages,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
        }
    }

    /// Returns a cheap snapshot suitable for external readers.
    #[must_use]
    pub fn snapshot(&self) -> AgentStateSnapshot {
        AgentStateSnapshot {
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            tools: self.tools.clone(),
            messages: self.messages.clone(),
            is_streaming: self.is_streaming,
            streaming_message: self.streaming_message.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
            error_message: self.error_message.clone(),
        }
    }

    /// Applies a single agent event using the TypeScript `processEvents` rules.
    ///
    /// Only state fields that `processEvents` mutates are touched:
    /// - `message_start` / `message_update` set `streaming_message`
    /// - `message_end` clears `streaming_message` and appends the message
    /// - `tool_execution_start` / `tool_execution_end` update `pending_tool_calls`
    /// - `turn_end` records assistant `errorMessage` into `error_message`
    /// - `agent_end` clears `streaming_message`
    ///
    /// `is_streaming` is intentionally left alone; the agent wrapper owns it.
    pub fn reduce(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
                self.streaming_message = Some(message.clone());
            }
            AgentEvent::MessageEnd { message } => {
                self.streaming_message = None;
                self.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                self.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                self.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(Message::Assistant(assistant)) = message.as_llm()
                    && let Some(error) = assistant.error_message.as_ref()
                {
                    self.error_message = Some(error.clone());
                }
            }
            AgentEvent::AgentEnd { .. } => {
                self.streaming_message = None;
            }
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::ToolExecutionUpdate { .. } => {}
        }
    }

    /// Clears runtime-only fields after a run finishes.
    ///
    /// Mirrors `finishRun`: streaming flags and pending tool calls reset, while
    /// the transcript and configuration remain.
    pub fn finish_run(&mut self) {
        self.is_streaming = false;
        self.streaming_message = None;
        self.pending_tool_calls.clear();
    }

    /// Resets transcript and runtime fields while keeping model configuration.
    pub fn reset_transcript(&mut self) {
        self.messages.clear();
        self.streaming_message = None;
        self.pending_tool_calls.clear();
        self.error_message = None;
        self.is_streaming = false;
    }
}

/// Immutable view of [`AgentState`] for concurrent readers.
#[derive(Clone)]
pub struct AgentStateSnapshot {
    /// System prompt sent with each model request.
    pub system_prompt: String,
    /// Active model used for future turns.
    pub model: Model,
    /// Requested reasoning level for future turns.
    pub thinking_level: ModelThinkingLevel,
    /// Available tools (shared via `Arc`).
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Conversation transcript.
    pub messages: Vec<AgentMessage>,
    /// True while the agent is processing a prompt or continuation.
    pub is_streaming: bool,
    /// Partial assistant message for the current streamed response, if any.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: BTreeSet<String>,
    /// Error message from the most recent failed or aborted assistant turn.
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::user_text;
    use crate::tool::AgentToolResult;
    use pi_ai::{AssistantMessage, Message, ToolResultMessage};
    use serde_json::{Map, Value};

    fn user(text: &str) -> AgentMessage {
        user_text(text, std::iter::empty())
    }

    fn assistant_with_error(error: &str) -> AgentMessage {
        let mut message = AssistantMessage::new("unknown", "unknown", "unknown", 1);
        message.error_message = Some(error.to_owned());
        AgentMessage::Llm(Box::new(Message::Assistant(message)))
    }

    fn assistant_ok() -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::Assistant(AssistantMessage::new(
            "unknown", "unknown", "unknown", 1,
        ))))
    }

    #[test]
    fn reduce_message_lifecycle() {
        let mut state = AgentState::new();
        let start = user("hi");

        state.reduce(&AgentEvent::MessageStart {
            message: start.clone(),
        });
        assert_eq!(state.streaming_message.as_ref(), Some(&start));
        assert!(state.messages.is_empty());

        state.reduce(&AgentEvent::MessageUpdate {
            message: start.clone(),
            assistant_message_event: Box::new(pi_ai::AssistantMessageEvent::Start {
                partial: AssistantMessage::new("unknown", "unknown", "unknown", 1),
            }),
        });
        assert_eq!(state.streaming_message.as_ref(), Some(&start));

        state.reduce(&AgentEvent::MessageEnd {
            message: start.clone(),
        });
        assert!(state.streaming_message.is_none());
        assert_eq!(state.messages, vec![start]);
    }

    #[test]
    fn reduce_tool_execution_pending_set() {
        let mut state = AgentState::new();
        let args = Map::<String, Value>::new();

        state.reduce(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".to_owned(),
            tool_name: "read".to_owned(),
            args: args.clone(),
        });
        state.reduce(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-2".to_owned(),
            tool_name: "bash".to_owned(),
            args: args.clone(),
        });
        assert_eq!(
            state.pending_tool_calls.iter().cloned().collect::<Vec<_>>(),
            vec!["call-1".to_owned(), "call-2".to_owned()]
        );

        state.reduce(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".to_owned(),
            tool_name: "read".to_owned(),
            result: AgentToolResult::default(),
            is_error: false,
        });
        assert_eq!(
            state.pending_tool_calls.iter().cloned().collect::<Vec<_>>(),
            vec!["call-2".to_owned()]
        );

        state.reduce(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-2".to_owned(),
            tool_name: "bash".to_owned(),
            result: AgentToolResult::default(),
            is_error: false,
        });
        assert!(state.pending_tool_calls.is_empty());
    }

    #[test]
    fn reduce_turn_end_records_assistant_error() {
        let mut state = AgentState::new();
        let failed = assistant_with_error("provider failed");

        state.reduce(&AgentEvent::TurnEnd {
            message: failed,
            tool_results: Vec::<ToolResultMessage>::new(),
        });
        assert_eq!(state.error_message.as_deref(), Some("provider failed"));
    }

    #[test]
    fn reduce_turn_end_ignores_non_error_assistant() {
        let mut state = AgentState::new();
        state.error_message = Some("stale".to_owned());

        state.reduce(&AgentEvent::TurnEnd {
            message: assistant_ok(),
            tool_results: Vec::new(),
        });
        // No errorMessage on the assistant => state.error_message is unchanged.
        assert_eq!(state.error_message.as_deref(), Some("stale"));
    }

    #[test]
    fn reduce_agent_end_clears_streaming_message() {
        let mut state = AgentState::new();
        state.streaming_message = Some(user("partial"));

        state.reduce(&AgentEvent::AgentEnd {
            messages: vec![user("done")],
        });
        assert!(state.streaming_message.is_none());
        // Transcript is not rewritten by agent_end; message_end owns appends.
        assert!(state.messages.is_empty());
    }

    #[test]
    fn reduce_does_not_toggle_is_streaming() {
        let mut state = AgentState::new();
        state.is_streaming = true;

        state.reduce(&AgentEvent::AgentStart);
        state.reduce(&AgentEvent::TurnStart);
        state.reduce(&AgentEvent::AgentEnd {
            messages: Vec::new(),
        });
        assert!(state.is_streaming);
    }

    #[test]
    fn finish_run_clears_runtime_fields() {
        let mut state = AgentState::new();
        state.is_streaming = true;
        state.streaming_message = Some(user("partial"));
        state.pending_tool_calls.insert("call".to_owned());
        state.messages.push(user("kept"));

        state.finish_run();
        assert!(!state.is_streaming);
        assert!(state.streaming_message.is_none());
        assert!(state.pending_tool_calls.is_empty());
        assert_eq!(state.messages.len(), 1);
    }

    #[test]
    fn snapshot_clones_current_fields() {
        let mut state = AgentState::new();
        state.system_prompt = "sys".to_owned();
        state.messages.push(user("a"));
        state.pending_tool_calls.insert("t1".to_owned());

        let snap = state.snapshot();
        assert_eq!(snap.system_prompt, "sys");
        assert_eq!(snap.messages.len(), 1);
        assert!(snap.pending_tool_calls.contains("t1"));
        assert!(!snap.is_streaming);
    }
}
