//! Context token estimation used by simple-stream max-token clamping.
//!
//! Port of `.references/pi/packages/ai/src/utils/estimate.ts`.

use serde_json::Value;

use crate::types::{
    AssistantContent, Context, Message, StopReason, TextContent, Tool, ToolResultContent, Usage,
    UserContent, UserMessageContent,
};

/// Context-token estimate anchored on the last valid assistant usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens (`usage_tokens + trailing_tokens`).
    pub tokens: u64,
    /// Tokens reported by the most recent applicable assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the usage anchor (or all messages when none).
    pub trailing_tokens: u64,
    /// Index of the applicable message that provided usage, or `None` when none exists.
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: u64 = 4;
const ESTIMATED_IMAGE_CHARS: u64 = 4_800;

/// Token total from a usage record.
///
/// Prefers `total_tokens` when non-zero, else sums input/output/cache fields.
#[must_use]
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

/// Upstream `.length` counts UTF-16 code units, not bytes; parity requires
/// the same unit so the max-token clamp and overflow thresholds agree on
/// non-ASCII text.
fn utf16_len(text: &str) -> u64 {
    text.chars().map(|c| c.len_utf16() as u64).sum()
}

fn safe_json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

fn estimate_text_and_image_content_chars_blocks(blocks: &[UserContent]) -> u64 {
    let mut chars = 0_u64;
    for block in blocks {
        match block {
            UserContent::Text(TextContent { text, .. }) => {
                chars = chars.saturating_add(utf16_len(text));
            }
            UserContent::Image(_) => {
                chars = chars.saturating_add(ESTIMATED_IMAGE_CHARS);
            }
        }
    }
    chars
}

fn estimate_tool_result_content_chars(blocks: &[ToolResultContent]) -> u64 {
    let mut chars = 0_u64;
    for block in blocks {
        match block {
            ToolResultContent::Text(TextContent { text, .. }) => {
                chars = chars.saturating_add(utf16_len(text));
            }
            ToolResultContent::Image(_) => {
                chars = chars.saturating_add(ESTIMATED_IMAGE_CHARS);
            }
        }
    }
    chars
}

/// Estimate tokens for plain text (`ceil(len / 4)`).
#[must_use]
pub fn estimate_text_tokens(text: &str) -> u64 {
    utf16_len(text).div_ceil(CHARS_PER_TOKEN)
}

/// Estimate tokens for user/tool text-or-image content.
#[must_use]
pub fn estimate_text_and_image_content_tokens(content: &UserMessageContent) -> u64 {
    let chars = match content {
        UserMessageContent::Text(text) => utf16_len(text),
        UserMessageContent::Blocks(blocks) => estimate_text_and_image_content_chars_blocks(blocks),
    };
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Estimate tokens for a single message.
#[must_use]
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => estimate_text_and_image_content_tokens(&user.content),
        Message::ToolResult(result) => {
            let chars = estimate_tool_result_content_chars(&result.content);
            chars.div_ceil(CHARS_PER_TOKEN)
        }
        Message::Assistant(assistant) => {
            let mut chars = 0_u64;
            for block in &assistant.content {
                match block {
                    AssistantContent::Text(text) => {
                        chars = chars.saturating_add(utf16_len(&text.text));
                    }
                    AssistantContent::Thinking(thinking) => {
                        chars = chars.saturating_add(utf16_len(&thinking.thinking));
                    }
                    AssistantContent::ToolCall(call) => {
                        chars = chars.saturating_add(utf16_len(&call.name));
                        let args = Value::Object(call.arguments.clone());
                        chars = chars.saturating_add(utf16_len(&safe_json_stringify(&args)));
                    }
                }
            }
            chars.div_ceil(CHARS_PER_TOKEN)
        }
    }
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Option<(Usage, usize)> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info: Option<(Usage, usize)> = None;

    for (index, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::User(user) => user.timestamp,
            Message::Assistant(assistant) => {
                // A newer prefix message was inserted after this response (for example, a
                // compaction summary), so its usage cannot describe the current prefix.
                let usage_applies_to_prefix = assistant.timestamp >= latest_prefix_timestamp;
                if usage_applies_to_prefix
                    && !matches!(
                        assistant.stop_reason,
                        StopReason::Aborted | StopReason::Error
                    )
                    && calculate_context_tokens(&assistant.usage) > 0
                {
                    usage_info = Some((assistant.usage.clone(), index));
                }
                assistant.timestamp
            }
            Message::ToolResult(result) => result.timestamp,
        };
        latest_prefix_timestamp = latest_prefix_timestamp.max(timestamp);
    }

    usage_info
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some((usage, index)) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage);
        let trailing_tokens: u64 = messages
            .iter()
            .skip(index.saturating_add(1))
            .map(estimate_message_tokens)
            .sum();
        return ContextUsageEstimate {
            tokens: usage_tokens.saturating_add(trailing_tokens),
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        };
    }

    let tokens: u64 = messages.iter().map(estimate_message_tokens).sum();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: Option<&[Tool]>) -> u64 {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else {
        return 0;
    };
    let value = serde_json::to_value(tools).unwrap_or(Value::Null);
    estimate_text_tokens(&safe_json_stringify(&value))
}

/// Estimate context tokens for a [`Context`] or bare message list.
///
/// When a prior assistant usage block is available, trailing messages (and
/// tools newly added after that usage) are estimated on top of the reported
/// usage total. Otherwise the full prompt is estimated from scratch.
#[must_use]
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages(&context.messages);
    if let Some(last_usage_index) = estimate.last_usage_index {
        let added_names: std::collections::BTreeSet<&str> = context
            .messages
            .iter()
            .skip(last_usage_index.saturating_add(1))
            .filter_map(|message| match message {
                Message::ToolResult(result) => result.added_tool_names.as_deref(),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        let added_tools: Vec<Tool> = context
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter(|tool| added_names.contains(tool.name.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let added_tool_tokens = estimate_tools_tokens(Some(added_tools.as_slice()));
        return ContextUsageEstimate {
            tokens: estimate.tokens.saturating_add(added_tool_tokens),
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens.saturating_add(added_tool_tokens),
            last_usage_index: estimate.last_usage_index,
        };
    }

    let prefix_tokens = context
        .system_prompt
        .as_deref()
        .map_or(0, estimate_text_tokens)
        .saturating_add(estimate_tools_tokens(context.tools.as_deref()));

    ContextUsageEstimate {
        tokens: estimate.tokens.saturating_add(prefix_tokens),
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens.saturating_add(prefix_tokens),
        last_usage_index: estimate.last_usage_index,
    }
}

/// Estimate context tokens for a bare message list (no system prompt / tools).
#[must_use]
pub fn estimate_messages_tokens(messages: &[Message]) -> ContextUsageEstimate {
    estimate_messages(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, TextContent, UserMessage};

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            total_tokens: input.saturating_add(output),
            ..Usage::default()
        }
    }

    #[test]
    fn calculate_context_tokens_prefers_total() {
        let mut u = usage(100, 50);
        assert_eq!(calculate_context_tokens(&u), 150);
        u.total_tokens = 0;
        assert_eq!(calculate_context_tokens(&u), 150);
    }

    #[test]
    fn estimate_text_tokens_ceil_divides_by_four() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("a"), 1);
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
    }

    #[test]
    fn estimate_text_tokens_counts_utf16_units_like_upstream() {
        // "中" = 3 UTF-8 bytes but 1 UTF-16 unit; "😀" = 4 bytes but 2 units.
        // Upstream .length counts units: 4 CJK chars = 4 units = 1 token
        // (bytes would give 3), and three emoji = 6 units = 2 tokens
        // (bytes would give 3, scalar chars would give 1).
        assert_eq!(estimate_text_tokens("中中中中"), 1);
        assert_eq!(estimate_text_tokens("😀😀😀"), 2);
        assert_eq!(estimate_text_tokens("中中中中中"), 2);
    }

    #[test]
    fn estimate_context_tokens_anchors_usage() {
        let mut assistant = AssistantMessage::new("api", "provider", "model", 2);
        assistant.usage = usage(100, 50);
        assistant.content = vec![AssistantContent::Text(TextContent::new("Hi"))];
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage::new(
                    UserMessageContent::Text("Hello".into()),
                    1,
                )),
                Message::Assistant(assistant),
                Message::User(UserMessage::new(
                    UserMessageContent::Text("continue".into()),
                    3,
                )),
            ],
            tools: None,
        };
        let estimate = estimate_context_tokens(&context);
        assert_eq!(estimate.usage_tokens, 150);
        assert_eq!(estimate.last_usage_index, Some(1));
        assert!(estimate.trailing_tokens > 0);
        assert_eq!(
            estimate.tokens,
            150_u64.saturating_add(estimate.trailing_tokens)
        );
    }

    #[test]
    fn estimate_context_tokens_includes_system_and_tools_without_usage() {
        let context = Context {
            system_prompt: Some("system".into()),
            messages: vec![Message::User(UserMessage::new(
                UserMessageContent::Text("hi".into()),
                1,
            ))],
            tools: Some(vec![Tool {
                name: "bash".into(),
                description: "run".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
        };
        let estimate = estimate_context_tokens(&context);
        assert!(estimate.tokens > estimate_text_tokens("hi"));
        assert_eq!(estimate.last_usage_index, None);
    }
}
