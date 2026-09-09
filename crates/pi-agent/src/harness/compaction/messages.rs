//! Agent-message helpers for compaction and branch summarization.
//!
//! Ported from `.references/pi/packages/agent/src/harness/messages.ts`. These
//! helpers translate harness [`AgentMessage`] values into the LLM-facing
//! [`Message`] list and construct the virtual `branchSummary` /
//! `compactionSummary` custom messages that stand in for summarized history.

use pi_ai::{Message, TextContent, UserContent, UserMessage, UserMessageContent};
use serde_json::{Map, Value};
use std::fmt::Write as _;

use crate::message::{AgentMessage, CustomAgentMessage, now_millis};
use crate::session::EntryId;

/// Marker prepended to compaction summary text when it is re-injected as a
/// user message.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Marker appended to compaction summary text.
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Marker prepended to branch summary text when it is re-injected as a user
/// message.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// Marker appended to branch summary text.
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Renders a `bashExecution` custom message as user-visible text.
///
/// Mirrors `bashExecutionToText` in `messages.ts`.
#[must_use]
pub fn bash_execution_to_text(message: &CustomAgentMessage) -> String {
    let command = payload_str(message, "command");
    let output = payload_str(message, "output");
    let exit_code = message.payload.get("exitCode").and_then(Value::as_f64);
    let cancelled = payload_bool(message, "cancelled");
    let truncated = payload_bool(message, "truncated");
    let full_output_path = payload_str(message, "fullOutputPath");

    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        let _ = write!(text, "```\n{output}\n```");
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code
        && code != 0.0
    {
        let _ = write!(text, "\n\nCommand exited with code {code}");
    }
    if truncated && !full_output_path.is_empty() {
        let _ = write!(
            text,
            "\n\n[Output truncated. Full output: {full_output_path}]"
        );
    }
    text
}

/// Creates a virtual `branchSummary` custom message.
///
/// Mirrors `createBranchSummaryMessage` in `messages.ts`.
#[must_use]
pub fn create_branch_summary_message(
    summary: &str,
    from_id: Option<&EntryId>,
    timestamp: i64,
) -> AgentMessage {
    let mut payload = Map::new();
    payload.insert("summary".to_owned(), Value::String(summary.to_owned()));
    payload.insert(
        "fromId".to_owned(),
        from_id.map_or(Value::Null, |id| Value::String(id.as_str().to_owned())),
    );
    payload.insert("timestamp".to_owned(), Value::from(timestamp));
    AgentMessage::Custom(CustomAgentMessage::new("branchSummary", payload))
}

/// Creates a virtual `compactionSummary` custom message.
///
/// Mirrors `createCompactionSummaryMessage` in `messages.ts`.
#[must_use]
pub fn create_compaction_summary_message(
    summary: &str,
    tokens_before: u64,
    timestamp: i64,
) -> AgentMessage {
    let mut payload = Map::new();
    payload.insert("summary".to_owned(), Value::String(summary.to_owned()));
    payload.insert("tokensBefore".to_owned(), Value::from(tokens_before));
    payload.insert("timestamp".to_owned(), Value::from(timestamp));
    AgentMessage::Custom(CustomAgentMessage::new("compactionSummary", payload))
}

/// Converts agent messages to LLM messages, expanding custom roles.
///
/// Mirrors `convertToLlm` in `messages.ts`: `bashExecution` renders as text
/// (unless `excludeFromContext`), `custom` unwraps its `content` payload,
/// `branchSummary`/`compactionSummary` wrap their summary in the marker
/// prefixes, and unknown custom roles are dropped.
#[must_use]
pub fn convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Llm(message) => Some(message.as_ref().clone()),
            AgentMessage::Custom(custom) => convert_custom_to_llm(custom),
        })
        .collect()
}

fn convert_custom_to_llm(custom: &CustomAgentMessage) -> Option<Message> {
    let timestamp = message_timestamp(&AgentMessage::Custom(custom.clone()));
    match custom.role.as_str() {
        "bashExecution" => {
            if payload_bool(custom, "excludeFromContext") {
                return None;
            }
            Some(user_text_message(bash_execution_to_text(custom), timestamp))
        }
        "custom" => {
            let content = match custom.payload.get("content") {
                Some(Value::String(text)) => UserMessageContent::Blocks(vec![UserContent::Text(
                    TextContent::new(text.clone()),
                )]),
                Some(Value::Array(blocks)) => UserMessageContent::Blocks(
                    blocks
                        .iter()
                        .filter_map(|block| {
                            serde_json::from_value::<UserContent>(block.clone()).ok()
                        })
                        .collect(),
                ),
                _ => UserMessageContent::Blocks(Vec::new()),
            };
            Some(Message::User(UserMessage::new(content, timestamp)))
        }
        "branchSummary" => Some(user_text_message(
            format!(
                "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                payload_str(custom, "summary")
            ),
            timestamp,
        )),
        "compactionSummary" => Some(user_text_message(
            format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                payload_str(custom, "summary")
            ),
            timestamp,
        )),
        _ => None,
    }
}

/// Timestamp carried by an agent message, `0` when the custom payload lacks
/// one.
#[must_use]
pub fn message_timestamp(message: &AgentMessage) -> i64 {
    match message {
        AgentMessage::Llm(message) => match message.as_ref() {
            Message::User(message) => message.timestamp,
            Message::Assistant(message) => message.timestamp,
            Message::ToolResult(message) => message.timestamp,
        },
        AgentMessage::Custom(custom) => custom
            .payload
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    }
}

/// Builds a single user message wrapping `text`.
#[must_use]
pub fn user_text_message(text: String, timestamp: i64) -> Message {
    Message::User(UserMessage::new(
        UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(text))]),
        timestamp,
    ))
}

/// Builds the user message sent to the summarization model.
#[must_use]
pub fn summary_user_message(text: &str) -> Message {
    user_text_message(text.to_owned(), now_millis())
}

fn payload_str<'a>(message: &'a CustomAgentMessage, key: &str) -> &'a str {
    message
        .payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn payload_bool(message: &CustomAgentMessage, key: &str) -> bool {
    message
        .payload
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[expect(
    clippy::panic,
    reason = "test assertions use let-else panic for irrecoverable fixture mismatch"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantContent, AssistantMessage, StopReason, Usage};

    fn custom(role: &str, payload: Map<String, Value>) -> AgentMessage {
        AgentMessage::Custom(CustomAgentMessage::new(role, payload))
    }

    #[test]
    fn branch_summary_message_shape() {
        let message = create_branch_summary_message("sum", Some(&EntryId::new("e1")), 42);
        let AgentMessage::Custom(custom) = &message else {
            panic!("expected custom message");
        };
        assert_eq!(custom.role, "branchSummary");
        assert_eq!(custom.payload["summary"], Value::from("sum"));
        assert_eq!(custom.payload["fromId"], Value::from("e1"));
        assert_eq!(custom.payload["timestamp"], Value::from(42));
    }

    #[test]
    fn branch_summary_message_null_from_id() {
        let message = create_branch_summary_message("sum", None, 0);
        let AgentMessage::Custom(custom) = &message else {
            panic!("expected custom message");
        };
        assert_eq!(custom.payload["fromId"], Value::Null);
    }

    #[test]
    fn compaction_summary_message_shape() {
        let message = create_compaction_summary_message("sum", 1234, 7);
        let AgentMessage::Custom(custom) = &message else {
            panic!("expected custom message");
        };
        assert_eq!(custom.role, "compactionSummary");
        assert_eq!(custom.payload["tokensBefore"], Value::from(1234));
    }

    #[test]
    fn convert_to_llm_wraps_summaries() {
        let mut payload = Map::new();
        payload.insert("summary".to_owned(), Value::from("did things"));
        payload.insert("timestamp".to_owned(), Value::from(5));
        let messages = vec![custom("branchSummary", payload.clone()), {
            let mut p = Map::new();
            p.insert("summary".to_owned(), Value::from("older"));
            p.insert("timestamp".to_owned(), Value::from(3));
            custom("compactionSummary", p)
        }];
        let llm = convert_to_llm(&messages);
        assert_eq!(llm.len(), 2);
        let Message::User(first) = &llm[0] else {
            panic!("expected user message");
        };
        let UserMessageContent::Blocks(blocks) = &first.content else {
            panic!("expected blocks");
        };
        let UserContent::Text(text) = &blocks[0] else {
            panic!("expected text");
        };
        assert!(text.text.starts_with(BRANCH_SUMMARY_PREFIX));
        assert!(text.text.ends_with(BRANCH_SUMMARY_SUFFIX));
        assert!(text.text.contains("did things"));
    }

    #[test]
    fn convert_to_llm_bash_execution() {
        let mut payload = Map::new();
        payload.insert("command".to_owned(), Value::from("ls -la"));
        payload.insert("output".to_owned(), Value::from("file.txt"));
        payload.insert("exitCode".to_owned(), Value::from(1));
        payload.insert("timestamp".to_owned(), Value::from(9));
        let llm = convert_to_llm(&[custom("bashExecution", payload)]);
        assert_eq!(llm.len(), 1);
        let Message::User(user) = &llm[0] else {
            panic!("expected user message");
        };
        assert_eq!(user.timestamp, 9);
        let UserMessageContent::Blocks(blocks) = &user.content else {
            panic!("expected blocks");
        };
        let UserContent::Text(text) = &blocks[0] else {
            panic!("expected text");
        };
        assert!(text.text.contains("Ran `ls -la`"));
        assert!(text.text.contains("file.txt"));
        assert!(text.text.contains("exited with code 1"));
    }

    #[test]
    fn convert_to_llm_drops_excluded_and_unknown() {
        let mut excluded = Map::new();
        excluded.insert("command".to_owned(), Value::from("ls"));
        excluded.insert("excludeFromContext".to_owned(), Value::from(true));
        let unknown = Map::new();
        let llm = convert_to_llm(&[
            custom("bashExecution", excluded),
            custom("mystery", unknown),
        ]);
        assert!(llm.is_empty());
    }

    #[test]
    fn convert_to_llm_custom_content() {
        let mut payload = Map::new();
        payload.insert("content".to_owned(), Value::from("plain text"));
        let llm = convert_to_llm(&[custom("custom", payload)]);
        let Message::User(user) = &llm[0] else {
            panic!("expected user message");
        };
        let UserMessageContent::Blocks(blocks) = &user.content else {
            panic!("expected blocks");
        };
        let UserContent::Text(text) = &blocks[0] else {
            panic!("expected text");
        };
        assert_eq!(text.text, "plain text");
    }

    #[test]
    fn convert_to_llm_passes_llm_messages() {
        let mut value = AssistantMessage::new("k", "m", "p", 1);
        value.content = vec![AssistantContent::Text(TextContent::new("hi"))];
        value.stop_reason = StopReason::Stop;
        value.usage = Usage::default();
        let assistant = AgentMessage::Llm(Box::new(Message::Assistant(Box::new(value))));
        let llm = convert_to_llm(&[assistant]);
        assert_eq!(llm.len(), 1);
        assert!(matches!(llm[0], Message::Assistant(_)));
    }

    #[test]
    fn bash_execution_text_variants() {
        let mut payload = Map::new();
        payload.insert("command".to_owned(), Value::from("cmd"));
        payload.insert("cancelled".to_owned(), Value::from(true));
        let mut message = CustomAgentMessage::new("bashExecution", payload);
        let text = bash_execution_to_text(&message);
        assert!(text.contains("(no output)"));
        assert!(text.contains("(command cancelled)"));

        message
            .payload
            .insert("cancelled".to_owned(), Value::from(false));
        message
            .payload
            .insert("truncated".to_owned(), Value::from(true));
        message
            .payload
            .insert("fullOutputPath".to_owned(), Value::from("/tmp/out.log"));
        let text = bash_execution_to_text(&message);
        assert!(text.contains("Full output: /tmp/out.log"));
    }
}
