//! Custom product message types and LLM conversion for the coding agent.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/messages.ts`. Typed
//! product roles sit on top of [`pi_agent::CustomAgentMessage`]; unknown custom
//! roles are skipped by [`convert_to_llm`] the same way the TypeScript exhaustive
//! `default` branch drops them.

use pi_agent::{AgentMessage, CustomAgentMessage};
use pi_ai::{Message, TextContent, UserContent, UserMessage, UserMessageContent};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Prefix wrapped around a compaction summary when it enters LLM context.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Suffix wrapped around a compaction summary when it enters LLM context.
///
/// Includes a leading newline; intentionally asymmetric with
/// [`BRANCH_SUMMARY_SUFFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Prefix wrapped around a branch summary when it enters LLM context.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";

/// Suffix wrapped around a branch summary when it enters LLM context.
///
/// No leading newline; intentionally asymmetric with
/// [`COMPACTION_SUMMARY_SUFFIX`].
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// Errors produced while parsing product custom messages or ISO timestamps.
#[derive(Debug, Error)]
pub enum MessageConversionError {
    /// An ISO-8601 / RFC-3339 timestamp string could not be parsed.
    #[error("invalid ISO timestamp `{timestamp}`: {source}")]
    InvalidTimestamp {
        /// The original timestamp string.
        timestamp: String,
        /// Underlying jiff parse failure.
        source: jiff::Error,
    },
    /// A known product custom role was present but its payload was incomplete
    /// or malformed.
    #[error("invalid {role} message payload: {source}")]
    InvalidPayload {
        /// Wire role that failed validation.
        role: &'static str,
        /// Underlying serde failure.
        source: serde_json::Error,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum BashExecutionRole {
    #[serde(rename = "bashExecution")]
    BashExecution,
}

/// Message type for bash executions via the `!` command.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    role: BashExecutionRole,
    /// Shell command that was executed.
    pub command: String,
    /// Captured command output (possibly truncated).
    pub output: String,
    /// Process exit code when the command finished; absent when cancelled or
    /// otherwise unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Whether the command was cancelled before completion.
    pub cancelled: bool,
    /// Whether [`Self::output`] is a truncated view of the full stream.
    pub truncated: bool,
    /// Path to the full output spill file when truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// When true, this message is excluded from LLM context (`!!` prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CustomRole {
    #[serde(rename = "custom")]
    Custom,
}

/// Content accepted by a product `custom` message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CustomMessageContent {
    /// Plain text payload.
    Text(String),
    /// Structured text and image blocks.
    Blocks(Vec<UserContent>),
}

/// Extension-injected message via `sendMessage()`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    role: CustomRole,
    /// Extension-defined custom type discriminant.
    pub custom_type: String,
    /// User-visible / LLM content.
    pub content: CustomMessageContent,
    /// Whether the interactive UI should render this message.
    pub display: bool,
    /// Opaque extension details preserved on the transcript entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum BranchSummaryRole {
    #[serde(rename = "branchSummary")]
    BranchSummary,
}

/// Summary of a conversation branch the session returned from.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    role: BranchSummaryRole,
    /// Branch summary text.
    pub summary: String,
    /// Entry id the branch forked from (`"root"` when branching from null).
    pub from_id: String,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CompactionSummaryRole {
    #[serde(rename = "compactionSummary")]
    CompactionSummary,
}

/// Compaction summary injected into model context after history is compacted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    role: CompactionSummaryRole,
    /// Compaction summary text.
    pub summary: String,
    /// Token count observed before compaction.
    pub tokens_before: i64,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// Inputs for [`BashExecutionMessage::from_fields`].
///
/// Mirrors every field of [`BashExecutionMessage`] except the wire-role
/// discriminant, which the constructor sets. Callers construct one of these
/// when they cannot reach the private `role` field directly (i.e. from outside
/// this module), then hand it to [`BashExecutionMessage::from_fields`] to
/// produce the message.
#[derive(Clone, Debug)]
pub struct BashExecutionFields {
    /// Shell command that was executed.
    pub command: String,
    /// Captured command output (possibly truncated).
    pub output: String,
    /// Process exit code when the command finished; absent when cancelled or
    /// otherwise unavailable.
    pub exit_code: Option<i64>,
    /// Whether the command was cancelled before completion.
    pub cancelled: bool,
    /// Whether [`Self::output`] is a truncated view of the full stream.
    pub truncated: bool,
    /// Path to the full output spill file when truncated.
    pub full_output_path: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// When true, this message is excluded from LLM context (`!!` prefix).
    pub exclude_from_context: Option<bool>,
}

impl BashExecutionMessage {
    /// Construct a bash-execution message from a [`BashExecutionFields`] record.
    ///
    /// The wire `role` discriminant is fixed to `bashExecution`. The timestamp,
    /// command, output, exit code, cancellation, truncation, spill path, and
    /// `exclude_from_context` flag are taken verbatim from `fields`.
    #[must_use]
    pub fn from_fields(fields: BashExecutionFields) -> Self {
        Self {
            role: BashExecutionRole::BashExecution,
            command: fields.command,
            output: fields.output,
            exit_code: fields.exit_code,
            cancelled: fields.cancelled,
            truncated: fields.truncated,
            full_output_path: fields.full_output_path,
            timestamp: fields.timestamp,
            exclude_from_context: fields.exclude_from_context,
        }
    }
}

/// Convert a [`BashExecutionMessage`] to user message text for LLM context.
#[must_use]
pub fn bash_execution_to_text(msg: &BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", msg.command);
    if msg.output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(&msg.output);
        text.push_str("\n```");
    }
    if msg.cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = msg.exit_code
        && exit_code != 0
    {
        text.push_str("\n\nCommand exited with code ");
        text.push_str(&exit_code.to_string());
    }
    if msg.truncated
        && let Some(path) = msg.full_output_path.as_deref()
    {
        text.push_str("\n\n[Output truncated. Full output: ");
        text.push_str(path);
        text.push(']');
    }
    text
}

/// Build a [`BranchSummaryMessage`] from an ISO-8601 timestamp string.
///
/// # Errors
///
/// Returns [`MessageConversionError::InvalidTimestamp`] when `timestamp` is
/// not a valid ISO-8601 timestamp.
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: &str,
) -> Result<BranchSummaryMessage, MessageConversionError> {
    Ok(BranchSummaryMessage {
        role: BranchSummaryRole::BranchSummary,
        summary: summary.into(),
        from_id: from_id.into(),
        timestamp: parse_iso_to_millis(timestamp)?,
    })
}

/// Build a [`CompactionSummaryMessage`] from an ISO-8601 timestamp string.
///
/// # Errors
///
/// Returns [`MessageConversionError::InvalidTimestamp`] when `timestamp` is
/// not a valid ISO-8601 timestamp.
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: i64,
    timestamp: &str,
) -> Result<CompactionSummaryMessage, MessageConversionError> {
    Ok(CompactionSummaryMessage {
        role: CompactionSummaryRole::CompactionSummary,
        summary: summary.into(),
        tokens_before,
        timestamp: parse_iso_to_millis(timestamp)?,
    })
}

/// Build a [`CustomMessage`] from an ISO-8601 timestamp string.
///
/// # Errors
///
/// Returns [`MessageConversionError::InvalidTimestamp`] when `timestamp` is
/// not a valid ISO-8601 timestamp.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: CustomMessageContent,
    display: bool,
    details: Option<Value>,
    timestamp: &str,
) -> Result<CustomMessage, MessageConversionError> {
    Ok(CustomMessage {
        role: CustomRole::Custom,
        custom_type: custom_type.into(),
        content,
        display,
        details,
        timestamp: parse_iso_to_millis(timestamp)?,
    })
}

/// Transform agent messages (including product custom roles) to LLM messages.
///
/// Known product roles are validated and converted. Unknown custom roles are
/// skipped, matching the TypeScript exhaustive-check default branch.
///
/// # Errors
///
/// Returns [`MessageConversionError::InvalidPayload`] when a known custom
/// role has a missing or malformed required payload.
pub fn convert_to_llm(messages: &[AgentMessage]) -> Result<Vec<Message>, MessageConversionError> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            AgentMessage::Llm(llm) => out.push(llm.as_ref().clone()),
            AgentMessage::Custom(custom) => {
                if let Some(converted) = convert_custom_to_llm(custom)? {
                    out.push(converted);
                }
            }
        }
    }
    Ok(out)
}

fn convert_custom_to_llm(
    custom: &CustomAgentMessage,
) -> Result<Option<Message>, MessageConversionError> {
    match custom.role.as_str() {
        "bashExecution" => {
            let msg: BashExecutionMessage = deserialize_known(custom, "bashExecution")?;
            if msg.exclude_from_context.unwrap_or(false) {
                return Ok(None);
            }
            let text = bash_execution_to_text(&msg);
            Ok(Some(Message::User(UserMessage::new(
                UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(text))]),
                msg.timestamp,
            ))))
        }
        "custom" => {
            let msg: CustomMessage = deserialize_known(custom, "custom")?;
            let content = match msg.content {
                CustomMessageContent::Text(text) => {
                    UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(text))])
                }
                CustomMessageContent::Blocks(blocks) => UserMessageContent::Blocks(blocks),
            };
            Ok(Some(Message::User(UserMessage::new(
                content,
                msg.timestamp,
            ))))
        }
        "branchSummary" => {
            let msg: BranchSummaryMessage = deserialize_known(custom, "branchSummary")?;
            let text = format!(
                "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                msg.summary
            );
            Ok(Some(Message::User(UserMessage::new(
                UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(text))]),
                msg.timestamp,
            ))))
        }
        "compactionSummary" => {
            let msg: CompactionSummaryMessage = deserialize_known(custom, "compactionSummary")?;
            let text = format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                msg.summary
            );
            Ok(Some(Message::User(UserMessage::new(
                UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(text))]),
                msg.timestamp,
            ))))
        }
        _ => Ok(None),
    }
}

fn deserialize_known<T: DeserializeOwned>(
    custom: &CustomAgentMessage,
    role: &'static str,
) -> Result<T, MessageConversionError> {
    let mut object = Map::with_capacity(custom.payload.len().saturating_add(1));
    object.insert("role".to_owned(), Value::String(custom.role.clone()));
    for (key, value) in &custom.payload {
        object.insert(key.clone(), value.clone());
    }
    serde_json::from_value(Value::Object(object))
        .map_err(|source| MessageConversionError::InvalidPayload { role, source })
}

fn parse_iso_to_millis(timestamp: &str) -> Result<i64, MessageConversionError> {
    timestamp
        .parse::<jiff::Timestamp>()
        .map(jiff::Timestamp::as_millisecond)
        .map_err(|source| MessageConversionError::InvalidTimestamp {
            timestamp: timestamp.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::ImageContent;
    use serde_json::json;

    fn agent_from_json(value: Value) -> Result<AgentMessage, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn user_text_block(message: &Message) -> Result<&str, String> {
        let Message::User(user) = message else {
            return Err("expected user message".to_owned());
        };
        match &user.content {
            UserMessageContent::Blocks(blocks) => match blocks.as_slice() {
                [UserContent::Text(text)] => Ok(text.text.as_str()),
                _ => Err(format!("expected single text block, got {blocks:?}")),
            },
            UserMessageContent::Text(text) => Ok(text.as_str()),
        }
    }

    #[test]
    fn summary_delimiters_are_exact_and_asymmetric() {
        assert_eq!(
            COMPACTION_SUMMARY_PREFIX,
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
        );
        assert_eq!(COMPACTION_SUMMARY_SUFFIX, "\n</summary>");
        assert_eq!(
            BRANCH_SUMMARY_PREFIX,
            "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n"
        );
        assert_eq!(BRANCH_SUMMARY_SUFFIX, "</summary>");
        assert_ne!(COMPACTION_SUMMARY_SUFFIX, BRANCH_SUMMARY_SUFFIX);
        assert!(COMPACTION_SUMMARY_SUFFIX.starts_with('\n'));
        assert!(!BRANCH_SUMMARY_SUFFIX.starts_with('\n'));
    }

    #[test]
    fn bash_execution_to_text_suffixes() {
        let base = BashExecutionMessage {
            role: BashExecutionRole::BashExecution,
            command: "ls".to_owned(),
            output: String::new(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1,
            exclude_from_context: None,
        };
        assert_eq!(bash_execution_to_text(&base), "Ran `ls`\n(no output)");

        let with_output = BashExecutionMessage {
            output: "a\nb".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            bash_execution_to_text(&with_output),
            "Ran `ls`\n```\na\nb\n```"
        );

        let cancelled = BashExecutionMessage {
            cancelled: true,
            exit_code: None,
            output: "partial".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            bash_execution_to_text(&cancelled),
            "Ran `ls`\n```\npartial\n```\n\n(command cancelled)"
        );

        let nonzero = BashExecutionMessage {
            exit_code: Some(2),
            output: "err".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            bash_execution_to_text(&nonzero),
            "Ran `ls`\n```\nerr\n```\n\nCommand exited with code 2"
        );

        let truncated = BashExecutionMessage {
            truncated: true,
            full_output_path: Some("/tmp/pi-bash-abc.log".to_owned()),
            output: "head".to_owned(),
            ..base
        };
        assert_eq!(
            bash_execution_to_text(&truncated),
            "Ran `ls`\n```\nhead\n```\n\n[Output truncated. Full output: /tmp/pi-bash-abc.log]"
        );
    }

    #[test]
    fn convert_all_four_roles_and_passthrough() -> Result<(), String> {
        let user = agent_from_json(json!({
            "role": "user",
            "content": "hello",
            "timestamp": 10,
        }))?;
        let bash = agent_from_json(json!({
            "role": "bashExecution",
            "command": "echo hi",
            "output": "hi",
            "exitCode": 0,
            "cancelled": false,
            "truncated": false,
            "timestamp": 20,
        }))?;
        let custom_string = agent_from_json(json!({
            "role": "custom",
            "customType": "note",
            "content": "string-body",
            "display": true,
            "details": { "keep": true },
            "timestamp": 30,
        }))?;
        let custom_blocks = agent_from_json(json!({
            "role": "custom",
            "customType": "rich",
            "content": [
                { "type": "text", "text": "part" },
                { "type": "image", "data": "abc", "mimeType": "image/png" }
            ],
            "display": false,
            "timestamp": 40,
        }))?;
        let branch = agent_from_json(json!({
            "role": "branchSummary",
            "summary": "branch-body",
            "fromId": "root",
            "timestamp": 50,
        }))?;
        let compaction = agent_from_json(json!({
            "role": "compactionSummary",
            "summary": "compact-body",
            "tokensBefore": 1234,
            "timestamp": 60,
        }))?;

        let converted =
            convert_to_llm(&[user, bash, custom_string, custom_blocks, branch, compaction])
                .map_err(|error| error.to_string())?;
        assert_eq!(converted.len(), 6);

        let Message::User(passthrough) = &converted[0] else {
            return Err("expected passthrough user".to_owned());
        };
        assert_eq!(passthrough.timestamp, 10);
        assert_eq!(
            passthrough.content,
            UserMessageContent::Text("hello".to_owned())
        );

        assert_eq!(
            user_text_block(&converted[1])?,
            "Ran `echo hi`\n```\nhi\n```"
        );
        assert_eq!(converted[1].timestamp_ms(), 20);

        assert_eq!(user_text_block(&converted[2])?, "string-body");
        assert_eq!(converted[2].timestamp_ms(), 30);

        let Message::User(rich) = &converted[3] else {
            return Err("expected rich custom user".to_owned());
        };
        assert_eq!(rich.timestamp, 40);
        assert_eq!(
            rich.content,
            UserMessageContent::Blocks(vec![
                UserContent::Text(TextContent::new("part")),
                UserContent::Image(ImageContent::new("abc", "image/png")),
            ])
        );

        let branch_text = user_text_block(&converted[4])?;
        assert_eq!(
            branch_text,
            format!("{BRANCH_SUMMARY_PREFIX}branch-body{BRANCH_SUMMARY_SUFFIX}")
        );
        assert!(branch_text.ends_with("</summary>"));
        assert!(!branch_text.ends_with("\n</summary>"));

        let compaction_text = user_text_block(&converted[5])?;
        assert_eq!(
            compaction_text,
            format!("{COMPACTION_SUMMARY_PREFIX}compact-body{COMPACTION_SUMMARY_SUFFIX}")
        );
        assert!(compaction_text.ends_with("\n</summary>"));
        Ok(())
    }

    #[test]
    fn convert_skips_excluded_bash_and_unknown_roles() -> Result<(), String> {
        let excluded = agent_from_json(json!({
            "role": "bashExecution",
            "command": "secret",
            "output": "x",
            "exitCode": 0,
            "cancelled": false,
            "truncated": false,
            "timestamp": 1,
            "excludeFromContext": true,
        }))?;
        let unknown = agent_from_json(json!({
            "role": "futureThing",
            "payload": 1,
            "timestamp": 2,
        }))?;
        let kept = agent_from_json(json!({
            "role": "user",
            "content": "only",
            "timestamp": 3,
        }))?;

        let converted =
            convert_to_llm(&[excluded, unknown, kept]).map_err(|error| error.to_string())?;
        assert_eq!(converted.len(), 1);
        let Message::User(user) = &converted[0] else {
            return Err("expected user".to_owned());
        };
        assert_eq!(user.content, UserMessageContent::Text("only".to_owned()));
        Ok(())
    }

    #[test]
    fn malformed_known_payload_errors() -> Result<(), String> {
        let malformed = agent_from_json(json!({
            "role": "bashExecution",
            "command": "ls",
            "timestamp": 1,
        }))?;
        let Err(error) = convert_to_llm(&[malformed]) else {
            return Err("expected malformed payload error".to_owned());
        };
        match error {
            MessageConversionError::InvalidPayload { role, .. } => {
                assert_eq!(role, "bashExecution");
            }
            other @ MessageConversionError::InvalidTimestamp { .. } => {
                return Err(format!("unexpected error: {other}"));
            }
        }
        Ok(())
    }

    #[test]
    fn iso_millisecond_constructors() -> Result<(), String> {
        let iso = "2025-12-08T22:55:54.170Z";
        let expected = iso
            .parse::<jiff::Timestamp>()
            .map_err(|error| error.to_string())?
            .as_millisecond();
        assert_eq!(expected, 1_765_234_554_170);

        let branch = create_branch_summary_message("s", "root", iso).map_err(|e| e.to_string())?;
        assert_eq!(branch.timestamp, expected);
        assert_eq!(branch.summary, "s");
        assert_eq!(branch.from_id, "root");

        let compaction =
            create_compaction_summary_message("c", 99, iso).map_err(|e| e.to_string())?;
        assert_eq!(compaction.timestamp, expected);
        assert_eq!(compaction.tokens_before, 99);

        let custom = create_custom_message(
            "t",
            CustomMessageContent::Text("body".to_owned()),
            true,
            Some(json!({ "a": 1 })),
            iso,
        )
        .map_err(|e| e.to_string())?;
        assert_eq!(custom.timestamp, expected);
        assert_eq!(custom.custom_type, "t");
        assert_eq!(custom.details, Some(json!({ "a": 1 })));

        let err = create_branch_summary_message("s", "root", "not-a-date")
            .err()
            .ok_or_else(|| "expected timestamp error".to_owned())?;
        assert!(matches!(
            err,
            MessageConversionError::InvalidTimestamp { .. }
        ));
        Ok(())
    }

    trait MessageTimestamp {
        fn timestamp_ms(&self) -> i64;
    }

    impl MessageTimestamp for Message {
        fn timestamp_ms(&self) -> i64 {
            match self {
                Self::User(message) => message.timestamp,
                Self::Assistant(message) => message.timestamp,
                Self::ToolResult(message) => message.timestamp,
            }
        }
    }
}
