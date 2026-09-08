//! File-operation extraction and conversation serialization helpers.
//!
//! Ported from `.references/pi/packages/agent/src/harness/compaction/utils.ts`.

use std::collections::BTreeSet;

use pi_ai::{
    AssistantContent, Message, ToolResultContent, UserContent, UserMessageContent,
};
use serde_json::{Map, Value};

use crate::message::AgentMessage;

/// File paths observed while processing a transcript span.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOperations {
    /// Paths read by a `read` tool call.
    pub read: BTreeSet<String>,
    /// Paths written by a full-file `write` tool call.
    pub written: BTreeSet<String>,
    /// Paths modified by an `edit` tool call.
    pub edited: BTreeSet<String>,
}

/// Creates an empty file-operation accumulator.
#[must_use]
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

impl FileOperations {
    /// Converts live sets into the sorted durable representation.
    #[must_use]
    pub fn to_durable(&self) -> crate::session::DurableFileOperations {
        crate::session::DurableFileOperations {
            read: self.read.iter().cloned().collect(),
            written: self.written.iter().cloned().collect(),
            edited: self.edited.iter().cloned().collect(),
        }
    }

    /// Rebuilds live sets from a durable representation.
    #[must_use]
    pub fn from_durable(value: crate::session::DurableFileOperations) -> Self {
        Self {
            read: value.read.into_iter().collect(),
            written: value.written.into_iter().collect(),
            edited: value.edited.into_iter().collect(),
        }
    }

    /// Merges another accumulator into this one.
    pub fn extend(&mut self, other: &Self) {
        self.read.extend(other.read.iter().cloned());
        self.written.extend(other.written.iter().cloned());
        self.edited.extend(other.edited.iter().cloned());
    }
}

/// Extracts file operations from assistant tool calls.
///
/// Only `read`, `write`, and `edit` tool calls with a string `path` argument
/// contribute. Tool calls from user/custom/tool-result messages are ignored.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Llm(message) = message else {
        return;
    };
    let Message::Assistant(assistant) = message.as_ref() else {
        return;
    };

    for block in &assistant.content {
        let AssistantContent::ToolCall(call) = block else {
            continue;
        };
        let Some(path) = call.arguments.get("path").and_then(Value::as_str) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
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

/// Sorted read-only and modified file lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileLists {
    /// Read paths not subsequently modified.
    pub read_files: Vec<String>,
    /// Union of written and edited paths.
    pub modified_files: Vec<String>,
}

/// Computes sorted file lists from accumulated operations.
#[must_use]
pub fn compute_file_lists(file_ops: &FileOperations) -> FileLists {
    let modified: BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .cloned()
        .collect();
    let read_files = file_ops
        .read
        .iter()
        .filter(|path| !modified.contains(*path))
        .cloned()
        .collect();
    FileLists {
        read_files,
        modified_files: modified.into_iter().collect(),
    }
}

/// Formats file lists as summary metadata tags.
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
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// Serializes LLM messages to plain text for a summarization prompt.
#[must_use]
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(message) => {
                let content = user_content_text(&message.content, "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(message) => {
                let mut thinking = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        AssistantContent::Thinking(block) => thinking.push(block.thinking.clone()),
                        AssistantContent::ToolCall(call) => {
                            let args = call
                                .arguments
                                .iter()
                                .map(|(key, value)| format!("{key}={}", safe_json_stringify(value)))
                                .collect::<Vec<_>>()
                                .join(", ");
                            tool_calls.push(format!("{}({args})", call.name));
                        }
                        AssistantContent::Text(_) => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if message
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::Text(_)))
                {
                    let content = assistant_content_text(&message.content, "\n");
                    if !content.is_empty() {
                        parts.push(format!("[Assistant]: {content}"));
                    }
                }
                if !tool_calls.is_empty() {
                    parts.push(format!(
                        "[Assistant tool calls]: {}",
                        tool_calls.join("; ")
                    ));
                }
            }
            Message::ToolResult(message) => {
                let content = tool_result_content_text(&message.content, "");
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

/// Counts UTF-16 code units, matching JavaScript's `String.length`.
#[must_use]
pub fn js_string_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

/// Truncates by UTF-16 code-unit count and records omitted units.
#[must_use]
pub fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let length = text.encode_utf16().count();
    if length <= max_chars {
        return text.to_owned();
    }
    let truncated_chars = length.saturating_sub(max_chars);
    let mut units = Vec::with_capacity(max_chars);
    'characters: for character in text.chars() {
        let mut buffer = [0_u16; 2];
        for unit in character.encode_utf16(&mut buffer) {
            if units.len() == max_chars {
                break 'characters;
            }
            units.push(*unit);
        }
    }
    let prefix = String::from_utf16_lossy(&units);
    format!("{prefix}\n\n[... {truncated_chars} more characters truncated]")
}

/// Text from user content blocks, with a caller-selected separator.
#[must_use]
pub fn user_content_text(content: &UserMessageContent, separator: &str) -> String {
    match content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                UserContent::Text(text) => Some(text.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join(separator),
    }
}

/// Text from assistant content blocks, with a caller-selected separator.
#[must_use]
pub fn assistant_content_text(
    content: &[AssistantContent],
    separator: &str,
) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

/// Text from tool-result content blocks, with a caller-selected separator.
#[must_use]
pub fn tool_result_content_text(content: &[ToolResultContent], separator: &str) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

/// Best-effort JSON serialization used in tool-call prompt rendering.
#[must_use]
pub fn safe_json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

/// Counts text/image payload characters as the model estimator does.
#[must_use]
pub fn estimate_text_and_image_content_chars(content: &UserMessageContent) -> u64 {
    match content {
        UserMessageContent::Text(text) => js_string_len(text),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => js_string_len(&text.text),
                UserContent::Image(_) => 4_800,
            })
            .sum(),
    }
}

/// Counts text/image payload characters in tool results.
#[must_use]
pub fn estimate_tool_result_content_chars(content: &[ToolResultContent]) -> u64 {
    content
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => js_string_len(&text.text),
            ToolResultContent::Image(_) => 4_800,
        })
        .sum()
}

/// Counts custom-content blocks represented in a raw JSON payload.
#[must_use]
pub fn estimate_custom_content_chars(content: &Value) -> u64 {
    match content {
        Value::String(text) => js_string_len(text),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let Some(object) = block.as_object() else {
                    return 0;
                };
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => object
                        .get("text")
                        .and_then(Value::as_str)
                        .map_or(0, js_string_len),
                    Some("image") => 4_800,
                    _ => 0,
                }
            })
            .sum(),
        _ => 0,
    }
}

/// Creates a JSON object suitable for tests and callers constructing tool
/// arguments without depending on private pi-ai fields.
#[must_use]
pub fn object(entries: impl IntoIterator<Item = (String, Value)>) -> Map<String, Value> {
    entries.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantContent, TextContent, ToolCall, ToolResultMessage, UserMessage};

    use crate::message::CustomAgentMessage;

    fn assistant_with_call(name: &str, path: &str) -> AgentMessage {
        let arguments = Map::from_iter([(String::from("path"), Value::from(path))]);
        let mut assistant = pi_ai::AssistantMessage::new("api", "provider", "model", 1);
        assistant.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call", name, arguments,
        ))];
        AgentMessage::Llm(Box::new(Message::Assistant(Box::new(assistant))))
    }

    #[test]
    fn file_operations_merge_and_sort() {
        let mut operations = create_file_ops();
        operations.read.insert("z".to_owned());
        operations.read.insert("a".to_owned());
        operations.written.insert("z".to_owned());
        operations.edited.insert("m".to_owned());
        let lists = compute_file_lists(&operations);
        assert_eq!(lists.read_files, vec!["a"]);
        assert_eq!(lists.modified_files, vec!["m", "z"]);
        assert_eq!(
            format_file_operations(&lists.read_files, &lists.modified_files),
            "\n\n<read-files>\na\n</read-files>\n\n<modified-files>\nm\nz\n</modified-files>"
        );
    }

    #[test]
    fn extracts_only_known_assistant_tools() {
        let mut operations = create_file_ops();
        extract_file_ops_from_message(&assistant_with_call("read", "a"), &mut operations);
        extract_file_ops_from_message(&assistant_with_call("write", "b"), &mut operations);
        extract_file_ops_from_message(&assistant_with_call("edit", "c"), &mut operations);
        extract_file_ops_from_message(&assistant_with_call("other", "d"), &mut operations);
        assert!(operations.read.contains("a"));
        assert!(operations.written.contains("b"));
        assert!(operations.edited.contains("c"));
        assert!(!operations.read.contains("d"));
    }

    #[test]
    fn serializes_messages_and_truncates_tool_results() {
        let user = Message::User(UserMessage::new(
            UserMessageContent::Text("hello".to_owned()),
            1,
        ));
        let tool = Message::ToolResult(ToolResultMessage::new(
            "call",
            "read",
            vec![ToolResultContent::Text(TextContent::new("x".repeat(2_100)))],
            false,
            2,
        ));
        let text = serialize_conversation(&[user, tool]);
        assert!(text.starts_with("[User]: hello"));
        assert!(text.contains("[Tool result]:"));
        assert!(text.contains("more characters truncated"));
    }

    #[test]
    fn utf16_length_preserves_non_ascii() {
        assert_eq!(js_string_len("é"), 1);
        assert_eq!(js_string_len("😀"), 2);
        let truncated = truncate_for_summary("😀abc", 2);
        assert!(truncated.starts_with("😀"));
    }

    #[test]
    fn custom_content_estimate() {
        let value = Value::Array(vec![
            serde_json::json!({"type":"text", "text":"é"}),
            serde_json::json!({"type":"image", "data":"x"}),
        ]);
        assert_eq!(estimate_custom_content_chars(&value), 4_801);
    }

    #[test]
    fn custom_payload_round_trip_not_treated_as_llm() {
        let message = AgentMessage::Custom(CustomAgentMessage::new(
            "unknown",
            Map::from_iter([(String::from("content"), Value::from("x"))]),
        ));
        let mut operations = create_file_ops();
        extract_file_ops_from_message(&message, &mut operations);
        assert!(operations.read.is_empty());
    }
}
