//! Observable agent lifecycle and tool-execution events.

use pi_ai::{AssistantMessageEvent, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::message::AgentMessage;
use crate::tool::AgentToolResult;

/// Events emitted by the agent runtime for UI, session, and extension consumers.
///
/// Wire tags and field names match the TypeScript `AgentEvent` contract.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A new agent run has started.
    AgentStart,
    /// The agent run finished with the messages produced by this invocation.
    AgentEnd {
        /// Messages produced by this run (prompt runs include injected prompts).
        messages: Vec<AgentMessage>,
    },
    /// A turn is about to begin.
    TurnStart,
    /// A turn finished with an assistant message and any tool results.
    TurnEnd {
        /// Assistant message that completed the turn.
        message: AgentMessage,
        /// Tool-result messages emitted for this turn, in assistant source order.
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
        /// Tool-call identifier from the assistant message.
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
        /// Tool-call identifier from the assistant message.
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
        /// Tool-call identifier from the assistant message.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantMessage, Message, TextContent, UserMessage, UserMessageContent};
    use serde_json::{Value, json};

    use crate::tool::{AgentToolResult, error_tool_result};

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
            UserMessageContent::Text(text.to_owned()),
            1,
        ))))
    }

    fn user_json(text: &str) -> Value {
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": text }],
            "timestamp": 1
        })
    }

    fn assistant_message() -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::Assistant(AssistantMessage::new(
            "api", "provider", "model", 2,
        ))))
    }

    fn assistant_json() -> Value {
        json!({
            "role": "assistant",
            "content": [],
            "api": "api",
            "provider": "provider",
            "model": "model",
            "usage": {
                "input": 0,
                "output": 0,
                "cacheRead": 0,
                "cacheWrite": 0,
                "totalTokens": 0,
                "cost": {
                    "input": 0.0,
                    "output": 0.0,
                    "cacheRead": 0.0,
                    "cacheWrite": 0.0,
                    "total": 0.0
                }
            },
            "stopReason": "stop",
            "timestamp": 2
        })
    }

    fn assert_wire(event: &AgentEvent, expected: Value) -> Result<(), serde_json::Error> {
        let encoded = serde_json::to_value(event)?;
        assert_eq!(encoded, expected);
        let decoded: AgentEvent = serde_json::from_value(expected)?;
        assert_eq!(serde_json::to_value(decoded)?, encoded);
        Ok(())
    }

    #[test]
    fn lifecycle_event_wire_contracts() -> Result<(), serde_json::Error> {
        assert_wire(&AgentEvent::AgentStart, json!({ "type": "agent_start" }))?;
        assert_wire(
            &AgentEvent::AgentEnd {
                messages: vec![user_message("done")],
            },
            json!({
                "type": "agent_end",
                "messages": [user_json("done")]
            }),
        )?;
        assert_wire(&AgentEvent::TurnStart, json!({ "type": "turn_start" }))?;
        assert_wire(
            &AgentEvent::TurnEnd {
                message: user_message("turn"),
                tool_results: Vec::new(),
            },
            json!({
                "type": "turn_end",
                "message": user_json("turn"),
                "toolResults": []
            }),
        )
    }

    #[test]
    fn legacy_user_message_event_reads_as_canonical_wire() -> Result<(), serde_json::Error> {
        let event: AgentEvent = serde_json::from_value(json!({
            "type": "message_start",
            "message": { "role": "user", "content": "legacy", "timestamp": 1 }
        }))?;

        assert_eq!(
            serde_json::to_value(event)?,
            json!({ "type": "message_start", "message": user_json("legacy") })
        );
        Ok(())
    }

    #[test]
    fn message_event_wire_contracts() -> Result<(), serde_json::Error> {
        assert_wire(
            &AgentEvent::MessageStart {
                message: user_message("start"),
            },
            json!({
                "type": "message_start",
                "message": user_json("start")
            }),
        )?;
        let assistant = AssistantMessage::new("api", "provider", "model", 2);
        assert_wire(
            &AgentEvent::MessageUpdate {
                message: assistant_message(),
                assistant_message_event: Box::new(AssistantMessageEvent::Start {
                    partial: assistant,
                }),
            },
            json!({
                "type": "message_update",
                "message": assistant_json(),
                "assistantMessageEvent": {
                    "type": "start",
                    "partial": assistant_json()
                }
            }),
        )?;
        assert_wire(
            &AgentEvent::MessageEnd {
                message: user_message("end"),
            },
            json!({
                "type": "message_end",
                "message": user_json("end")
            }),
        )
    }

    #[test]
    fn tool_event_wire_contracts() -> Result<(), serde_json::Error> {
        let args = Map::from_iter([("path".to_owned(), json!("a.rs"))]);
        assert_wire(
            &AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                args: args.clone(),
            },
            json!({
                "type": "tool_execution_start",
                "toolCallId": "call-1",
                "toolName": "read",
                "args": { "path": "a.rs" }
            }),
        )?;
        assert_wire(
            &AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                args,
                partial_result: AgentToolResult {
                    content: vec![pi_ai::ToolResultContent::Text(TextContent::new("partial"))],
                    details: json!({}),
                    added_tool_names: None,
                    terminate: None,
                },
            },
            json!({
                "type": "tool_execution_update",
                "toolCallId": "call-1",
                "toolName": "read",
                "args": { "path": "a.rs" },
                "partialResult": {
                    "content": [{ "type": "text", "text": "partial" }],
                    "details": {}
                }
            }),
        )?;
        assert_wire(
            &AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                result: error_tool_result("boom"),
                is_error: true,
            },
            json!({
                "type": "tool_execution_end",
                "toolCallId": "call-1",
                "toolName": "read",
                "result": {
                    "content": [{ "type": "text", "text": "boom" }],
                    "details": {}
                },
                "isError": true
            }),
        )
    }
}
