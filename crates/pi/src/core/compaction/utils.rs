//! Shared utilities for compaction and branch summarization.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/compaction/utils.ts`.

use std::collections::BTreeSet;

use pi_agent::AgentMessage;
use pi_ai::{AssistantContent, Message, ToolResultContent, UserContent, UserMessageContent};
use serde_json::Value;

/// Maximum characters for a tool result in serialized summaries.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// System prompt shared by every summarization request.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Cumulative file-operation tracking for compaction and branch summaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOperations {
    /// Paths read by the `read` tool.
    pub read: BTreeSet<String>,
    /// Paths written by the `write` tool.
    pub written: BTreeSet<String>,
    /// Paths edited by the `edit` tool.
    pub edited: BTreeSet<String>,
}

/// Create an empty [`FileOperations`] set.
#[must_use]
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// Extract file operations from tool calls in an assistant message.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Llm(llm) = message else {
        return;
    };
    let Message::Assistant(assistant) = llm.as_ref() else {
        return;
    };

    for block in &assistant.content {
        let AssistantContent::ToolCall(call) = block else {
            continue;
        };
        let Some(path) = call.arguments.get("path").and_then(Value::as_str) else {
            continue;
        };
        match call.name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_owned());
            }
            "write" => {
                file_ops.written.insert(path.to_owned());
            }
            "edit" => {
                file_ops.edited.insert(path.to_owned());
            }
            _ => {}
        }
    }
}

/// Compute final file lists from file operations.
///
/// Returns read-only files (read and never modified) and modified files
/// (edited ∪ written), both sorted lexicographically.
#[must_use]
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .cloned()
        .collect();
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|path| !modified.contains(*path))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_files, modified_files)
}

/// Format file operations as XML tags for summary append.
#[must_use]
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let total_units = text.encode_utf16().count();
    if total_units <= max_chars {
        return text.to_owned();
    }

    let mut retained_units = 0usize;
    let head_end = text
        .char_indices()
        .take_while(|(_, ch)| {
            let next = retained_units.saturating_add(ch.len_utf16());
            if next > max_chars {
                false
            } else {
                retained_units = next;
                true
            }
        })
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    let truncated_units = total_units.saturating_sub(retained_units);
    format!(
        "{}\n\n[... {truncated_units} more characters truncated]",
        &text[..head_end]
    )
}

/// Serialize LLM messages to text for summarization.
///
/// Call [`crate::core::messages::convert_to_llm`] first so custom product roles
/// become ordinary user text. Tool results are truncated to
/// [`TOOL_RESULT_MAX_CHARS`].
#[must_use]
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    UserMessageContent::Text(text) => text.clone(),
                    UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            UserContent::Text(text) => Some(text.text.as_str()),
                            UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut text_parts = Vec::new();
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => text_parts.push(text.text.as_str()),
                        AssistantContent::Thinking(thinking) => {
                            thinking_parts.push(thinking.thinking.as_str());
                        }
                        AssistantContent::ToolCall(call) => {
                            let args_str = call
                                .arguments
                                .iter()
                                .map(|(k, v)| {
                                    format!(
                                        "{k}={}",
                                        serde_json::to_string(v)
                                            .unwrap_or_else(|_| "null".to_owned())
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            tool_calls.push(format!("{}({args_str})", call.name));
                        }
                    }
                }

                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if !text_parts.is_empty() {
                    parts.push(format!("[Assistant]: {}", text_parts.join("\n")));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let content = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ToolResultContent::Text(text) => Some(text.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{TextContent, ToolResultMessage, UserMessage};

    fn tool_result(text: &str) -> Message {
        Message::ToolResult(ToolResultMessage::new(
            "tc1",
            "read",
            vec![ToolResultContent::Text(TextContent::new(text))],
            false,
            1,
        ))
    }

    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(5000);
        let result = serialize_conversation(&[tool_result(&long)]);
        assert!(result.contains("[Tool result]:"));
        assert!(result.contains("[... 3000 more characters truncated]"));
        assert!(result.contains(&"x".repeat(2000)));
        assert!(!result.contains(&"x".repeat(3000)));
    }

    #[test]
    fn truncates_emoji_on_utf16_units_without_splitting() -> Result<(), &'static str> {
        // 😀 is one scalar / two UTF-16 units. 1000 emoji = 2000 units → no truncate.
        let exact = "😀".repeat(1000);
        let exact_result = serialize_conversation(&[tool_result(&exact)]);
        assert_eq!(exact_result, format!("[Tool result]: {exact}"));
        assert!(!exact_result.contains("truncated"));

        // 1001 emoji = 2002 units → keep 1000 emoji (2000 units), marker says 2.
        let over = "😀".repeat(1001);
        let over_result = serialize_conversation(&[tool_result(&over)]);
        assert!(over_result.contains("[... 2 more characters truncated]"));
        assert!(over_result.contains(&"😀".repeat(1000)));
        // Must not split a surrogate pair / scalar: head ends on a full emoji.
        let head = over_result
            .strip_prefix("[Tool result]: ")
            .and_then(|s| s.split("\n\n[...").next())
            .ok_or("missing expected tool result structure")?;
        assert_eq!(head.chars().count(), 1000);
        assert_eq!(head.encode_utf16().count(), 2000);
        Ok(())
    }

    #[test]
    fn does_not_truncate_short_tool_results() {
        let short = "x".repeat(1500);
        let result = serialize_conversation(&[tool_result(&short)]);
        assert_eq!(result, format!("[Tool result]: {short}"));
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn does_not_truncate_user_or_assistant() {
        let long = "y".repeat(5000);
        let messages = [
            Message::User(UserMessage::new(
                UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(long.clone()))]),
                1,
            )),
            Message::Assistant({
                let mut msg = pi_ai::AssistantMessage::new("anthropic", "anthropic", "test", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new(long.clone()))];
                msg
            }),
        ];
        let result = serialize_conversation(&messages);
        assert!(!result.contains("truncated"));
        assert!(result.contains(&long));
        assert!(result.contains("[User]:"));
        assert!(result.contains("[Assistant]:"));
    }

    #[test]
    fn file_ops_read_modified_sorted() {
        let mut ops = create_file_ops();
        extract_file_ops_from_message(
            &AgentMessage::Llm(Box::new(Message::Assistant({
                let mut msg = pi_ai::AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![
                    AssistantContent::ToolCall(pi_ai::ToolCall::new(
                        "1",
                        "read",
                        serde_json::Map::from_iter([(
                            "path".into(),
                            Value::String("b.txt".into()),
                        )]),
                    )),
                    AssistantContent::ToolCall(pi_ai::ToolCall::new(
                        "2",
                        "read",
                        serde_json::Map::from_iter([(
                            "path".into(),
                            Value::String("a.txt".into()),
                        )]),
                    )),
                    AssistantContent::ToolCall(pi_ai::ToolCall::new(
                        "3",
                        "edit",
                        serde_json::Map::from_iter([(
                            "path".into(),
                            Value::String("b.txt".into()),
                        )]),
                    )),
                    AssistantContent::ToolCall(pi_ai::ToolCall::new(
                        "4",
                        "write",
                        serde_json::Map::from_iter([(
                            "path".into(),
                            Value::String("c.txt".into()),
                        )]),
                    )),
                ];
                msg
            }))),
            &mut ops,
        );
        let (read_files, modified_files) = compute_file_lists(&ops);
        assert_eq!(read_files, vec!["a.txt".to_owned()]);
        assert_eq!(modified_files, vec!["b.txt".to_owned(), "c.txt".to_owned()]);
        let formatted = format_file_operations(&read_files, &modified_files);
        assert_eq!(
            formatted,
            "\n\n<read-files>\na.txt\n</read-files>\n\n<modified-files>\nb.txt\nc.txt\n</modified-files>"
        );
    }
}
