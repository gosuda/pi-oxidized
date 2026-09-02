//! Context compaction for long sessions.
//!
//! Pure functions for compaction logic. Session I/O lives in
//! [`crate::core::sessions`]. Ports
//! `.references/pi/packages/coding-agent/src/core/compaction/compaction.ts`.

mod branch;
mod utils;

pub use branch::{
    BRANCH_SUMMARY_PREAMBLE, BRANCH_SUMMARY_PROMPT, BranchPreparation, BranchSummaryDetails,
    BranchSummaryResult, CollectEntriesResult, DEFAULT_BRANCH_CONTEXT_WINDOW,
    DEFAULT_BRANCH_MAX_TOKENS, DEFAULT_BRANCH_RESERVE_TOKENS, GenerateBranchSummaryOptions,
    collect_entries_for_branch_summary, generate_branch_summary, prepare_branch_entries,
};
pub use utils::{
    FileOperations, SUMMARIZATION_SYSTEM_PROMPT, TOOL_RESULT_MAX_CHARS, compute_file_lists,
    create_file_ops, extract_file_ops_from_message, format_file_operations, serialize_conversation,
};

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use pi_agent::AgentMessage;
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Model,
    ModelThinkingLevel, ProviderError, StopReason, StreamOptionKey, StreamOptions, TextContent,
    Usage, UserContent, UserMessage, UserMessageContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::core::messages::{MessageConversionError, convert_to_llm};
use crate::core::sessions::{
    LeafRef, SessionEntry, build_session_context, session_entry_to_context_messages,
};

// ---------------------------------------------------------------------------
// Constants / settings
// ---------------------------------------------------------------------------

/// Default compaction settings (settings.json + pure defaults).
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

/// Estimated character weight of one image when estimating tokens.
const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Initial history summarization prompt (exact TS text).
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Update history summarization prompt when a previous summary exists.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Turn-prefix summarization prompt used when a cut splits a turn.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Compaction settings (`enabled`, reserve, keep-recent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    /// Whether automatic compaction is enabled.
    pub enabled: bool,
    /// Tokens reserved for the prompt + summary response.
    pub reserve_tokens: u64,
    /// Approximate tokens of recent context to keep after the cut.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

/// Details stored on a compaction entry for cumulative file tracking.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    /// Paths that were only read (not modified).
    pub read_files: Vec<String>,
    /// Paths that were edited or written.
    pub modified_files: Vec<String>,
}

/// Result returned by [`compact`] before `SessionManager` assigns ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    /// Structured summary text (with optional file-op appendices).
    pub summary: String,
    /// First kept entry id after the cut.
    pub first_kept_entry_id: String,
    /// Estimated/observed context tokens before compaction.
    pub tokens_before: u64,
    /// Optional estimated tokens after compaction (extension-filled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<u64>,
    /// File tracking / extension details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// `true` when an extension hook supplied this result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// LLM usage from the summarization call(s), when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_ai::Usage>,
}

/// Context-token estimate anchored on the last valid assistant usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// `usage_tokens + trailing_tokens`.
    pub tokens: u64,
    /// Tokens from the last valid assistant usage (0 when none).
    pub usage_tokens: u64,
    /// Estimated tokens after the usage anchor (or all messages when none).
    pub trailing_tokens: u64,
    /// Index of the last valid assistant usage, or `None`.
    pub last_usage_index: Option<usize>,
}

/// Cut-point selection result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry to keep.
    pub first_kept_entry_index: usize,
    /// Index of the turn-start entry when splitting, else `usize::MAX` (`-1`).
    pub turn_start_index: usize,
    /// Whether the cut splits a multi-message turn.
    pub is_split_turn: bool,
}

/// Pure preparation produced by [`prepare_compaction`] for hooks / [`compact`].
#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    /// UUID of the first kept entry.
    pub first_kept_entry_id: String,
    /// Messages that will be summarized and discarded.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// Turn-prefix messages when splitting a turn.
    pub turn_prefix_messages: Vec<AgentMessage>,
    /// Whether this is a split-turn cut.
    pub is_split_turn: bool,
    /// Context tokens before compaction.
    pub tokens_before: u64,
    /// Previous compaction summary for iterative update.
    pub previous_summary: Option<String>,
    /// File operations extracted from messages / previous details.
    pub file_ops: FileOperations,
    /// Compaction settings used for this preparation.
    pub settings: CompactionSettings,
}

/// Errors from pure compaction / summarization.
#[derive(Debug, Error)]
pub enum CompactionError {
    /// Last path entry is already a compaction summary.
    #[error("Already compacted")]
    AlreadyCompacted,
    /// Session is too small to compact (nothing outside the keep window).
    #[error("Nothing to compact (session too small)")]
    NothingToCompact,
    /// First kept entry is missing an id (pre-migration session).
    #[error("First kept entry has no UUID - session may need migration")]
    MissingFirstKeptId,
    /// History summarization failed.
    #[error("Summarization failed: {0}")]
    SummarizationFailed(String),
    /// Turn-prefix summarization failed.
    #[error("Turn prefix summarization failed: {0}")]
    TurnPrefixSummarizationFailed(String),
    /// Summarization was cancelled.
    #[error("Summarization cancelled")]
    Cancelled,
    /// Message conversion / session context projection failed.
    #[error(transparent)]
    MessageConversion(#[from] MessageConversionError),
    /// Provider stream failed before producing a terminal message.
    #[error("Summarization failed: {0}")]
    Provider(#[from] ProviderError),
}

/// Outcome of a [`CompactionHooks::before_compact`] call.
#[derive(Clone, Debug, Default)]
pub struct BeforeCompactResult {
    /// When true, compaction is cancelled by the extension.
    pub cancel: bool,
    /// When set, replaces the LLM-generated compaction result (`fromHook`).
    pub compaction: Option<CompactionResult>,
}

/// Injected summarizer stream: `(model, context, options) -> stream of events`.
pub type SummarizeStreamFn = Arc<
    dyn Fn(
            Model,
            Context,
            StreamOptions,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Pin<
                            Box<
                                dyn futures::Stream<
                                        Item = Result<AssistantMessageEvent, ProviderError>,
                                    > + Send,
                            >,
                        >,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Upper bound on one summarization backoff, in milliseconds. Prevents a
/// saturated `checked_shl` from turning a retry delay into an unbounded sleep.
const MAX_RETRY_BACKOFF_MS: u64 = 60_000;

/// Bounded retry settings for one standalone summarization request.
///
/// The initial request is not a retry. Enabled retries use
/// `base_delay_ms * 2^(attempt - 1)` for 1-based attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SummarizationRetryPolicy {
    /// Whether transient assistant errors may be retried.
    pub enabled: bool,
    /// Maximum retry attempts after the initial request.
    pub max_retries: u32,
    /// Base exponential-backoff delay in milliseconds.
    pub base_delay_ms: u64,
}

type RetryScheduledFn = Arc<dyn Fn(u32, u32, u64, String) + Send + Sync>;

/// Synchronous hooks around a summarization retry cycle.
///
/// `AgentSession` uses these to publish the TypeScript-compatible retry events.
#[derive(Clone, Default)]
pub struct SummarizationRetryCallbacks {
    /// Called before each retry backoff. Arguments are attempt, maximum, delay, error.
    pub on_retry_scheduled: Option<RetryScheduledFn>,
    /// Called after backoff immediately before the retried request.
    pub on_retry_attempt_start: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called once after at least one retry was scheduled and the cycle ends.
    pub on_retry_finished: Option<Arc<dyn Fn() + Send + Sync>>,
}

// ---------------------------------------------------------------------------
// Token calculation
// ---------------------------------------------------------------------------

/// Calculate total context tokens from usage.
///
/// Uses `total_tokens` when non-zero, else `input + output + cache_read + cache_write`.
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

fn get_assistant_usage(msg: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Llm(llm) = msg else {
        return None;
    };
    let Message::Assistant(assistant) = llm.as_ref() else {
        return None;
    };
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    if calculate_context_tokens(&assistant.usage) == 0 {
        return None;
    }
    Some(&assistant.usage)
}

/// Find the last valid assistant message usage from session entries.
#[must_use]
pub fn get_last_assistant_usage(entries: &[&SessionEntry]) -> Option<Usage> {
    for entry in entries.iter().rev() {
        if let SessionEntry::Message(m) = entry
            && let Some(usage) = get_assistant_usage(&m.message)
        {
            return Some(usage.clone());
        }
    }
    None
}

fn get_last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(Usage, usize)> {
    for (i, msg) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(msg) {
            return Some((usage.clone(), i));
        }
    }
    None
}

/// Estimate context tokens from messages, using the last assistant usage when available.
#[must_use]
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    match get_last_assistant_usage_info(messages) {
        None => {
            let estimated: u64 = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((usage, index)) => {
            let usage_tokens = calculate_context_tokens(&usage);
            let trailing_tokens: u64 = messages
                .iter()
                .skip(index.saturating_add(1))
                .map(estimate_tokens)
                .sum();
            ContextUsageEstimate {
                tokens: usage_tokens.saturating_add(trailing_tokens),
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

/// Check if compaction should trigger based on context usage.
///
/// Threshold is strict `>`: `context_tokens > context_window - reserve_tokens`.
#[must_use]
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    i128::from(context_tokens) > i128::from(context_window) - i128::from(settings.reserve_tokens)
}

// ---------------------------------------------------------------------------
// Token estimation (chars / 4)
// ---------------------------------------------------------------------------

fn js_string_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn estimate_text_and_image_user_content(content: &UserMessageContent) -> usize {
    match content {
        UserMessageContent::Text(text) => js_string_len(text),
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => js_string_len(&text.text),
                UserContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
    }
}

fn estimate_tool_result_chars(content: &[pi_ai::ToolResultContent]) -> usize {
    content
        .iter()
        .map(|block| match block {
            pi_ai::ToolResultContent::Text(text) => js_string_len(&text.text),
            pi_ai::ToolResultContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        })
        .sum()
}

fn ceil_div4(chars: usize) -> u64 {
    if chars == 0 {
        0
    } else {
        u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX)
    }
}

/// Estimate token count for a message using the chars/4 heuristic.
#[must_use]
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Llm(llm) => match llm.as_ref() {
            Message::User(user) => ceil_div4(estimate_text_and_image_user_content(&user.content)),
            Message::Assistant(assistant) => {
                let mut chars = 0usize;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => chars += js_string_len(&text.text),
                        AssistantContent::Thinking(thinking) => {
                            chars += js_string_len(&thinking.thinking);
                        }
                        AssistantContent::ToolCall(call) => {
                            chars += js_string_len(&call.name);
                            chars += serde_json::to_string(&call.arguments)
                                .map_or(0, |serialized| js_string_len(&serialized));
                        }
                    }
                }
                ceil_div4(chars)
            }
            Message::ToolResult(result) => ceil_div4(estimate_tool_result_chars(&result.content)),
        },
        AgentMessage::Custom(custom) => match custom.role.as_str() {
            "custom" => {
                let chars = custom
                    .payload
                    .get("content")
                    .map_or(0, estimate_custom_content_chars);
                ceil_div4(chars)
            }
            "bashExecution" => {
                let command = custom
                    .payload
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let output = custom
                    .payload
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                ceil_div4(js_string_len(command) + js_string_len(output))
            }
            "branchSummary" | "compactionSummary" => {
                let summary = custom
                    .payload
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                ceil_div4(js_string_len(summary))
            }
            _ => 0,
        },
    }
}

fn estimate_custom_content_chars(content: &Value) -> usize {
    match content {
        Value::String(text) => js_string_len(text),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "text" {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map_or(0, js_string_len)
                } else if kind == "image" {
                    ESTIMATED_IMAGE_CHARS
                } else {
                    0
                }
            })
            .sum(),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Cut point detection
// ---------------------------------------------------------------------------

fn is_cut_point_message(message: &AgentMessage) -> bool {
    matches!(
        message.role(),
        "user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message.role(),
        "user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary"
    )
}

fn entry_context_messages(entry: &SessionEntry) -> Vec<AgentMessage> {
    session_entry_to_context_messages(entry).unwrap_or_default()
}

fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    if entry.discriminant() == "compaction" {
        return false;
    }
    entry_context_messages(entry)
        .iter()
        .any(is_turn_start_message)
}

fn find_valid_cut_points(
    entries: &[&SessionEntry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut cut_points = Vec::new();
    let end = end_index.min(entries.len());
    for (i, entry) in entries.iter().enumerate().take(end).skip(start_index) {
        if entry.discriminant() == "compaction" {
            continue;
        }
        if entry_context_messages(entry)
            .iter()
            .any(is_cut_point_message)
        {
            cut_points.push(i);
        }
    }
    cut_points
}

/// Find the context-visible turn-start entry that starts the turn containing `entry_index`.
///
/// Returns `usize::MAX` when no turn start is found (TS `-1`).
#[must_use]
pub fn find_turn_start_index(
    entries: &[&SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> usize {
    let mut i = entry_index;
    loop {
        if i < start_index || i >= entries.len() {
            return usize::MAX;
        }
        if is_turn_start_entry(entries[i]) {
            return i;
        }
        if i == 0 {
            return usize::MAX;
        }
        i -= 1;
    }
}

/// Find the cut point that keeps approximately `keep_recent_tokens`.
///
/// Walks newest→oldest accumulating `estimate_tokens`. Never cuts at
/// `toolResult`. Expands backward over metadata-only entries. Detects split
/// turns when the cut entry is not a turn start.
#[must_use]
pub fn find_cut_point(
    entries: &[&SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: usize::MAX,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0u64;
    let mut cut_index = cut_points[0];
    let end = end_index.min(entries.len());

    if end > start_index {
        let mut i = end - 1;
        loop {
            let entry = entries[i];
            let message_tokens: u64 = entry_context_messages(entry)
                .iter()
                .map(estimate_tokens)
                .sum();
            if message_tokens > 0 {
                accumulated_tokens = accumulated_tokens.saturating_add(message_tokens);
                if accumulated_tokens >= keep_recent_tokens {
                    for &cp in &cut_points {
                        if cp >= i {
                            cut_index = cp;
                            break;
                        }
                    }
                    break;
                }
            }
            if i == start_index {
                break;
            }
            i -= 1;
        }
    }

    // Expand cut index backward over adjacent metadata-only entries.
    while cut_index > start_index {
        let prev = entries[cut_index - 1];
        if prev.discriminant() == "compaction" || !entry_context_messages(prev).is_empty() {
            break;
        }
        cut_index -= 1;
    }

    let cut_entry = entries[cut_index];
    let starts_turn = is_turn_start_entry(cut_entry);
    let turn_start_index = if starts_turn {
        usize::MAX
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index != usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// Message extraction / file ops
// ---------------------------------------------------------------------------

fn get_message_from_entry_for_compaction(entry: &SessionEntry) -> Option<AgentMessage> {
    if entry.discriminant() == "compaction" {
        return None;
    }
    entry_context_messages(entry).into_iter().next()
}

fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[&SessionEntry],
    prev_compaction_index: Option<usize>,
) -> FileOperations {
    let mut file_ops = create_file_ops();

    if let Some(idx) = prev_compaction_index
        && let Some(SessionEntry::Compaction(prev)) = entries.get(idx).copied()
    {
        let from_hook = prev.from_hook.unwrap_or(false);
        if !from_hook && let Some(details) = prev.details.as_ref() {
            merge_details_into_file_ops(details, &mut file_ops);
        }
    }

    for msg in messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    file_ops
}

fn merge_details_into_file_ops(details: &Value, file_ops: &mut FileOperations) {
    if let Some(arr) = details.get("readFiles").and_then(Value::as_array) {
        for f in arr {
            if let Some(path) = f.as_str() {
                file_ops.read.insert(path.to_owned());
            }
        }
    }
    if let Some(arr) = details.get("modifiedFiles").and_then(Value::as_array) {
        for f in arr {
            if let Some(path) = f.as_str() {
                // Modified files go into edited for proper deduplication.
                file_ops.edited.insert(path.to_owned());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Preparation
// ---------------------------------------------------------------------------

/// Prepare compaction inputs without calling the LLM.
///
/// Returns `None` when the last entry is already a compaction, the session is
/// too small, or the first kept entry lacks an id.
///
/// # Errors
///
/// Returns [`CompactionError::MessageConversion`] when session context
/// projection fails while estimating `tokens_before`.
pub fn prepare_compaction(
    path_entries: &[&SessionEntry],
    settings: CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries
        .last()
        .is_some_and(|e| e.discriminant() == "compaction")
    {
        return Ok(None);
    }

    let mut prev_compaction_index = None;
    for (i, entry) in path_entries.iter().enumerate().rev() {
        if entry.discriminant() == "compaction" {
            prev_compaction_index = Some(i);
            break;
        }
    }

    let mut previous_summary = None;
    let mut boundary_start = 0usize;
    if let Some(idx) = prev_compaction_index
        && let SessionEntry::Compaction(prev) = path_entries[idx]
    {
        previous_summary = Some(prev.summary.clone());
        let first_kept_idx = path_entries
            .iter()
            .position(|e| e.id() == Some(prev.first_kept_entry_id.as_str()));
        boundary_start = first_kept_idx.unwrap_or(idx.saturating_add(1));
    }
    let boundary_end = path_entries.len();

    let session_ctx = build_session_context(path_entries, LeafRef::Last)?;
    let tokens_before = estimate_context_tokens(&session_ctx.messages).tokens;

    let cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );

    let first_kept_entry = path_entries.get(cut_point.first_kept_entry_index).copied();
    let Some(first_kept_entry) = first_kept_entry else {
        return Ok(None);
    };
    let Some(first_kept_entry_id) = first_kept_entry.id().map(str::to_owned) else {
        return Ok(None);
    };

    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in path_entries.iter().take(history_end).skip(boundary_start) {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(msg);
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if cut_point.is_split_turn {
        for entry in path_entries
            .iter()
            .take(cut_point.first_kept_entry_index)
            .skip(cut_point.turn_start_index)
        {
            if let Some(msg) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }

    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return Ok(None);
    }

    let mut file_ops =
        extract_file_operations(&messages_to_summarize, path_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    }))
}

/// Classify a `None` preparation into the exact manual-compact error strings.
#[must_use]
pub fn preparation_none_error(path_entries: &[&SessionEntry]) -> CompactionError {
    if path_entries
        .last()
        .is_some_and(|e| e.discriminant() == "compaction")
    {
        CompactionError::AlreadyCompacted
    } else {
        CompactionError::NothingToCompact
    }
}

// ---------------------------------------------------------------------------
// Summarization helpers
// ---------------------------------------------------------------------------

fn thinking_level_from_str(level: &str) -> Option<ModelThinkingLevel> {
    match level {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

fn create_summarization_options(
    model: &Model,
    max_tokens: u64,
    api_key: Option<String>,
    headers: Option<BTreeMap<String, Option<String>>>,
    env: Option<BTreeMap<String, String>>,
    signal: Option<CancellationToken>,
    thinking_level: Option<&str>,
) -> StreamOptions {
    let mut options = StreamOptions {
        max_tokens: Some(max_tokens),
        signal,
        api_key,
        headers,
        env,
        ..StreamOptions::default()
    };
    if model.reasoning
        && let Some(level) = thinking_level
        && thinking_level_from_str(level).is_some()
        && (level != "off"
            || matches!(model.api.as_str(), "google-generative-ai" | "google-vertex"))
    {
        options.insert_extra(StreamOptionKey::REASONING, Value::String(level.to_owned()));
    }
    options
}

async fn complete_summarization_once(
    model: &Model,
    context: Context,
    options: StreamOptions,
    stream_fn: &SummarizeStreamFn,
) -> Result<AssistantMessage, CompactionError> {
    ensure_not_cancelled(options.signal.as_ref())?;

    let signal = options.signal.clone();
    let mut stream_future = stream_fn(model.clone(), context, options);
    let mut stream = if let Some(signal) = signal.as_ref() {
        tokio::select! {
            biased;
            () = signal.cancelled() => return Err(CompactionError::Cancelled),
            stream = stream_future.as_mut() => stream,
        }
    } else {
        stream_future.await
    };
    let mut last: Option<AssistantMessage> = None;

    loop {
        let item = if let Some(signal) = signal.as_ref() {
            tokio::select! {
                biased;
                () = signal.cancelled() => return Err(CompactionError::Cancelled),
                item = stream.next() => item,
            }
        } else {
            stream.next().await
        };
        let Some(item) = item else {
            break;
        };
        match item {
            Ok(AssistantMessageEvent::Done { message, .. }) => return Ok(message),
            Ok(AssistantMessageEvent::Error { error, .. }) => return Ok(error),
            Ok(other) => {
                // Keep last partial as a fallback if the stream ends without Done.
                if let Some(partial) = event_partial_message(&other) {
                    last = Some(partial.clone());
                }
            }
            Err(err) => return Err(CompactionError::Provider(err)),
        }
    }

    last.ok_or_else(|| CompactionError::SummarizationFailed("Unknown error".to_owned()))
}

/// Retry one assistant-producing summarization call on transient assistant errors.
///
/// This ports the behavioural contract of TypeScript `retryAssistantCall`: an
/// aborted response is terminal, only retryable error responses consume the
/// budget, and `on_retry_finished` fires once only if a retry was scheduled.
async fn retry_summarization_call<F>(
    mut produce: F,
    policy: Option<&SummarizationRetryPolicy>,
    signal: Option<&CancellationToken>,
    callbacks: Option<&SummarizationRetryCallbacks>,
) -> Result<AssistantMessage, CompactionError>
where
    F: FnMut() -> Pin<Box<dyn Future<Output = Result<AssistantMessage, CompactionError>> + Send>>,
{
    let max_retries = policy
        .filter(|policy| policy.enabled)
        .map_or(0, |policy| policy.max_retries);
    let mut attempt = 0_u32;
    let mut retried = false;

    let finish_callback = callbacks.and_then(|callbacks| callbacks.on_retry_finished.as_ref());

    loop {
        let response = match produce().await {
            Ok(response) => response,
            Err(error) => {
                if retried && let Some(callback) = finish_callback {
                    callback();
                }
                return Err(error);
            }
        };

        if response.stop_reason == StopReason::Aborted {
            if retried && let Some(callback) = finish_callback {
                callback();
            }
            return Ok(response);
        }

        if response.stop_reason != StopReason::Error
            || attempt >= max_retries
            || !crate::core::agent_session::retry::is_retryable_assistant_error(&response)
        {
            if retried && let Some(callback) = finish_callback {
                callback();
            }
            return Ok(response);
        }

        attempt = attempt.saturating_add(1);
        retried = true;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_owned());
        let delay_ms = policy.map_or(0, |policy| {
            let exponent = attempt.saturating_sub(1);
            let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
            policy
                .base_delay_ms
                .saturating_mul(multiplier)
                .min(MAX_RETRY_BACKOFF_MS)
        });

        if let Some(callback) =
            callbacks.and_then(|callbacks| callbacks.on_retry_scheduled.as_ref())
        {
            callback(attempt, max_retries, delay_ms, error_message);
        }

        let cancelled = if let Some(signal) = signal {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => false,
                () = signal.cancelled() => true,
            }
        } else {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            false
        };
        if cancelled {
            if let Some(callback) = finish_callback {
                callback();
            }
            let mut aborted = response;
            aborted.stop_reason = StopReason::Aborted;
            aborted.error_message = None;
            return Ok(aborted);
        }

        if let Some(callback) =
            callbacks.and_then(|callbacks| callbacks.on_retry_attempt_start.as_ref())
        {
            callback();
        }
    }
}

async fn complete_summarization(
    model: &Model,
    context: Context,
    options: StreamOptions,
    stream_fn: &SummarizeStreamFn,
    policy: Option<&SummarizationRetryPolicy>,
    callbacks: Option<&SummarizationRetryCallbacks>,
) -> Result<AssistantMessage, CompactionError> {
    let signal = options.signal.clone();
    let model = model.clone();
    let stream_fn = Arc::clone(stream_fn);
    retry_summarization_call(
        move || {
            let model = model.clone();
            let context = context.clone();
            let options = options.clone();
            let stream_fn = Arc::clone(&stream_fn);
            Box::pin(async move {
                complete_summarization_once(&model, context, options, &stream_fn).await
            })
        },
        policy,
        signal.as_ref(),
        callbacks,
    )
    .await
}

fn event_partial_message(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn history_max_tokens(reserve_tokens: u64, model: &Model) -> u64 {
    let budget = (reserve_tokens / 5) * 4 + (reserve_tokens % 5) * 4 / 5;
    if model.max_tokens > 0 {
        budget.min(model.max_tokens)
    } else {
        budget
    }
}

fn turn_prefix_max_tokens(reserve_tokens: u64, model: &Model) -> u64 {
    let budget = reserve_tokens / 2;
    if model.max_tokens > 0 {
        budget.min(model.max_tokens)
    } else {
        budget
    }
}

fn ensure_not_cancelled(signal: Option<&CancellationToken>) -> Result<(), CompactionError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(CompactionError::Cancelled)
    } else {
        Ok(())
    }
}

/// Combine two `Usage` records by summing all fields (mirrors TS `combineUsage`).
#[must_use]
pub fn combine_usage(first: &pi_ai::Usage, second: &pi_ai::Usage) -> pi_ai::Usage {
    pi_ai::Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write1h: match (first.cache_write1h, second.cache_write1h) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        total_tokens: first.total_tokens + second.total_tokens,
        cost: pi_ai::UsageCost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

/// Generate a summary of the conversation using the injected stream function.
///
/// If `previous_summary` is provided, uses the update prompt. Custom
/// instructions are always **appended** (`\n\nAdditional focus: …`).
///
/// # Errors
///
/// Returns [`CompactionError::SummarizationFailed`], [`CompactionError::Cancelled`],
/// or conversion/provider errors.
pub async fn generate_summary(
    preparation: &CompactionPreparation,
    options: &CompactOptions<'_>,
) -> Result<(String, Option<pi_ai::Usage>), CompactionError> {
    ensure_not_cancelled(options.signal.as_ref())?;

    let max_tokens = history_max_tokens(preparation.settings.reserve_tokens, options.model);
    let nonempty_previous_summary = preparation
        .previous_summary
        .as_deref()
        .filter(|summary| !summary.is_empty());
    let mut base_prompt = if nonempty_previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if let Some(custom) = options.custom_instructions.filter(|s| !s.is_empty()) {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {custom}");
    }

    let llm_messages = convert_to_llm(&preparation.messages_to_summarize)?;
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(prev) = nonempty_previous_summary {
        let _ = write!(
            prompt_text,
            "<previous-summary>\n{prev}\n</previous-summary>\n\n"
        );
    }
    prompt_text.push_str(&base_prompt);

    let summarization_messages = vec![Message::User(UserMessage::new(
        UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(prompt_text))]),
        now_millis(),
    ))];

    let completion_options = create_summarization_options(
        options.model,
        max_tokens,
        options.api_key.clone(),
        options.headers.clone(),
        options.env.clone(),
        options.signal.clone(),
        options.thinking_level,
    );

    ensure_not_cancelled(options.signal.as_ref())?;

    let response = complete_summarization(
        options.model,
        Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: summarization_messages,
            tools: None,
        },
        completion_options,
        &options.stream_fn,
        options.retry.as_ref(),
        options.retry_callbacks.as_ref(),
    )
    .await?;

    ensure_not_cancelled(options.signal.as_ref())?;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::Cancelled);
    }
    if response.stop_reason == StopReason::Error {
        let msg = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_owned());
        return Err(CompactionError::SummarizationFailed(msg));
    }

    Ok((assistant_text(&response), Some(response.usage)))
}

async fn generate_turn_prefix_summary(
    preparation: &CompactionPreparation,
    options: &CompactOptions<'_>,
) -> Result<(String, Option<pi_ai::Usage>), CompactionError> {
    ensure_not_cancelled(options.signal.as_ref())?;

    let max_tokens = turn_prefix_max_tokens(preparation.settings.reserve_tokens, options.model);
    let llm_messages = convert_to_llm(&preparation.turn_prefix_messages)?;
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    let summarization_messages = vec![Message::User(UserMessage::new(
        UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(prompt_text))]),
        now_millis(),
    ))];

    ensure_not_cancelled(options.signal.as_ref())?;

    let response = complete_summarization(
        options.model,
        Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: summarization_messages,
            tools: None,
        },
        create_summarization_options(
            options.model,
            max_tokens,
            options.api_key.clone(),
            options.headers.clone(),
            options.env.clone(),
            options.signal.clone(),
            options.thinking_level,
        ),
        &options.stream_fn,
        options.retry.as_ref(),
        options.retry_callbacks.as_ref(),
    )
    .await?;

    ensure_not_cancelled(options.signal.as_ref())?;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::Cancelled);
    }
    if response.stop_reason == StopReason::Error {
        let msg = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_owned());
        return Err(CompactionError::TurnPrefixSummarizationFailed(msg));
    }

    Ok((assistant_text(&response), Some(response.usage)))
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

async fn summarize_preparation(
    preparation: &CompactionPreparation,
    options: &CompactOptions<'_>,
) -> Result<(String, Option<pi_ai::Usage>), CompactionError> {
    if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let (history_text, history_usage) = if preparation.messages_to_summarize.is_empty() {
            ("No prior history.".to_owned(), None)
        } else {
            generate_summary(preparation, options).await?
        };

        ensure_not_cancelled(options.signal.as_ref())?;
        let (turn_prefix_text, turn_prefix_usage) =
            generate_turn_prefix_summary(preparation, options).await?;
        let combined = match (history_usage, turn_prefix_usage) {
            (Some(h), Some(t)) => Some(combine_usage(&h, &t)),
            (Some(h), None) => Some(h),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        };
        Ok((
            format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}"
            ),
            combined,
        ))
    } else {
        generate_summary(preparation, options).await
    }
}

/// Options for [`compact`].
pub struct CompactOptions<'a> {
    /// Active model.
    pub model: &'a Model,
    /// Explicit API key.
    pub api_key: Option<String>,
    /// Optional request headers (`None` value suppresses a default).
    pub headers: Option<BTreeMap<String, Option<String>>>,
    /// Optional custom focus (always appended for compaction).
    pub custom_instructions: Option<&'a str>,
    /// Cancellation token.
    pub signal: Option<CancellationToken>,
    /// Thinking level string (`"off"`, `"high"`, …).
    pub thinking_level: Option<&'a str>,
    /// Injected summarizer stream.
    pub stream_fn: SummarizeStreamFn,
    /// Provider-scoped environment overrides.
    pub env: Option<BTreeMap<String, String>>,
    /// Retry policy for each standalone summarization request.
    pub retry: Option<SummarizationRetryPolicy>,
    /// Lifecycle callbacks for transient summarization retries.
    pub retry_callbacks: Option<SummarizationRetryCallbacks>,
}

/// Generate summaries for compaction using prepared data.
///
/// # Errors
///
/// Returns exact contract error strings for cancellation, summarization
/// failure, missing first-kept id, and message conversion failures.
pub async fn compact(
    preparation: &CompactionPreparation,
    options: CompactOptions<'_>,
) -> Result<CompactionResult, CompactionError> {
    ensure_not_cancelled(options.signal.as_ref())?;

    ensure_not_cancelled(options.signal.as_ref())?;

    if preparation.first_kept_entry_id.is_empty() {
        return Err(CompactionError::MissingFirstKeptId);
    }

    let (mut summary, usage) = summarize_preparation(preparation, &options).await?;

    ensure_not_cancelled(options.signal.as_ref())?;

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    let details = CompactionDetails {
        read_files,
        modified_files,
    };
    let details_value = serde_json::to_value(&details).unwrap_or(Value::Null);

    let result = CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id.clone(),
        tokens_before: preparation.tokens_before,
        estimated_tokens_after: None,
        details: Some(details_value),
        from_hook: None,
        usage,
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantMessage, TextContent, ToolCall, ToolResultContent, ToolResultMessage};
    use serde_json::Map;
    use serde_json::json;

    fn result_ok<T, E>(result: Result<T, E>) -> T {
        assert!(result.is_ok());
        match result {
            Ok(value) => value,
            Err(_) => unreachable!(),
        }
    }

    fn result_err<T, E>(result: Result<T, E>) -> E {
        assert!(result.is_err());
        match result {
            Ok(_) => unreachable!(),
            Err(error) => error,
        }
    }

    fn option_some<T>(option: Option<T>) -> T {
        assert!(option.is_some());
        match option {
            Some(value) => value,
            None => unreachable!(),
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write1h: None,
            reasoning: None,
            total_tokens: input + output + cache_read + cache_write,
            cost: pi_ai::UsageCost::default(),
        }
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
            UserMessageContent::Text(text.to_owned()),
            1,
        ))))
    }

    fn assistant_msg(text: &str, u: Usage) -> AgentMessage {
        let mut msg =
            AssistantMessage::new("anthropic-messages", "anthropic", "claude-sonnet-4-5", 1);
        msg.content = vec![AssistantContent::Text(TextContent::new(text))];
        msg.usage = u;
        msg.stop_reason = StopReason::Stop;
        AgentMessage::Llm(Box::new(Message::Assistant(msg)))
    }

    fn tool_result_msg(text: &str) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::ToolResult(ToolResultMessage::new(
            "tc1",
            "read",
            vec![ToolResultContent::Text(TextContent::new(text))],
            false,
            1,
        ))))
    }

    fn custom_msg(content: &str) -> AgentMessage {
        let mut payload = Map::new();
        payload.insert("customType".into(), Value::String("test".into()));
        payload.insert("content".into(), Value::String(content.into()));
        payload.insert("display".into(), Value::Bool(true));
        payload.insert("timestamp".into(), Value::from(1));
        AgentMessage::Custom(pi_agent::CustomAgentMessage::new("custom", payload))
    }

    fn bash_msg(command: &str, output: &str) -> AgentMessage {
        let mut payload = Map::new();
        payload.insert("command".into(), Value::String(command.into()));
        payload.insert("output".into(), Value::String(output.into()));
        payload.insert("cancelled".into(), Value::Bool(false));
        payload.insert("truncated".into(), Value::Bool(false));
        payload.insert("timestamp".into(), Value::from(1));
        AgentMessage::Custom(pi_agent::CustomAgentMessage::new("bashExecution", payload))
    }

    fn branch_summary_msg(summary: &str) -> AgentMessage {
        let mut payload = Map::new();
        payload.insert("summary".into(), Value::String(summary.into()));
        payload.insert("fromId".into(), Value::String("root".into()));
        payload.insert("timestamp".into(), Value::from(1));
        AgentMessage::Custom(pi_agent::CustomAgentMessage::new("branchSummary", payload))
    }

    fn compaction_summary_msg(summary: &str) -> AgentMessage {
        let mut payload = Map::new();
        payload.insert("summary".into(), Value::String(summary.into()));
        payload.insert("tokensBefore".into(), Value::from(100));
        payload.insert("timestamp".into(), Value::from(1));
        AgentMessage::Custom(pi_agent::CustomAgentMessage::new(
            "compactionSummary",
            payload,
        ))
    }

    fn message_entry(id: &str, parent: Option<&str>, message: AgentMessage) -> SessionEntry {
        let message = result_ok(serde_json::to_value(message));
        result_ok(serde_json::from_value(json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "message": message,
        })))
    }

    fn compaction_entry(
        id: &str,
        parent: Option<&str>,
        summary: &str,
        first_kept: &str,
    ) -> SessionEntry {
        result_ok(serde_json::from_value(json!({
            "type": "compaction",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "summary": summary,
            "firstKeptEntryId": first_kept,
            "tokensBefore": 10000,
        })))
    }

    fn model_change_entry(id: &str, parent: Option<&str>) -> SessionEntry {
        result_ok(serde_json::from_value(json!({
            "type": "model_change",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "provider": "openai",
            "modelId": "gpt-4",
        })))
    }

    fn custom_message_entry(id: &str, parent: Option<&str>, content: &str) -> SessionEntry {
        result_ok(serde_json::from_value(json!({
            "type": "custom_message",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "customType": "test",
            "content": content,
            "display": true,
        })))
    }

    fn test_model(max_tokens: u64, context_window: u64) -> Model {
        Model {
            id: "test".into(),
            name: "Test".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://example.test".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![pi_ai::ModelInput::Text],
            cost: pi_ai::ModelCost::default(),
            context_window,
            max_tokens,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn mock_stream_fn(text: &str, stop: StopReason) -> SummarizeStreamFn {
        let text = text.to_owned();
        Arc::new(move |_model, _ctx, _opts| {
            let text = text.clone();
            Box::pin(async move {
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new(text))];
                msg.stop_reason = stop;
                if stop == StopReason::Error {
                    msg.error_message = Some("boom".into());
                }
                let stream = futures::stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::Stop,
                    message: msg,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        })
    }

    #[test]
    fn summarization_preserves_off_for_google_models() {
        let mut model = test_model(4_096, 32_000);
        model.api = "google-vertex".to_owned();
        model.reasoning = true;
        let options =
            create_summarization_options(&model, 1_024, None, None, None, None, Some("off"));
        assert_eq!(
            options.extra_value(StreamOptionKey::REASONING),
            Some(&Value::String("off".to_owned()))
        );
    }

    #[test]
    fn calculate_context_tokens_prefers_total() {
        let mut u = usage(1000, 500, 200, 100);
        assert_eq!(calculate_context_tokens(&u), 1800);
        u.total_tokens = 0;
        assert_eq!(calculate_context_tokens(&u), 1800);
        let zero = usage(0, 0, 0, 0);
        assert_eq!(calculate_context_tokens(&zero), 0);
    }

    #[test]
    fn get_last_assistant_usage_skips_aborted_error_zero() {
        let a1 = message_entry("a1", None, assistant_msg("Hi", usage(100, 50, 0, 0)));
        let a2 = message_entry("a2", Some("a1"), {
            let mut m = AssistantMessage::new("anthropic-messages", "anthropic", "m", 1);
            m.usage = usage(300, 150, 0, 0);
            m.stop_reason = StopReason::Aborted;
            AgentMessage::Llm(Box::new(Message::Assistant(m)))
        });
        let entries = [&a1, &a2];
        let found = option_some(get_last_assistant_usage(&entries));
        assert_eq!(found.input, 100);

        let zero = message_entry("z", Some("a1"), assistant_msg("Partial", usage(0, 0, 0, 0)));
        let entries = [&a1, &zero];
        let found = option_some(get_last_assistant_usage(&entries));
        assert_eq!(found.input, 100);

        let only_user = message_entry("u", None, user_msg("Hello"));
        assert!(get_last_assistant_usage(&[&only_user]).is_none());
    }

    #[test]
    fn estimate_context_tokens_anchors_usage() {
        let messages = vec![
            user_msg("Hello"),
            assistant_msg("Hi", usage(100, 50, 0, 0)),
            user_msg("continue"),
            assistant_msg("Partial thinking", usage(0, 0, 0, 0)),
        ];
        let estimate = estimate_context_tokens(&messages);
        assert_eq!(estimate.usage_tokens, 150);
        assert_eq!(estimate.last_usage_index, Some(1));
        assert!(estimate.trailing_tokens > 0);
        assert_eq!(estimate.tokens, 150 + estimate.trailing_tokens);
    }

    #[test]
    fn estimate_tokens_every_role() {
        assert!(estimate_tokens(&user_msg("abcd")) >= 1);
        let with_image = UserMessage::new(
            UserMessageContent::Blocks(vec![
                UserContent::Text(TextContent::new("hi")),
                UserContent::Image(pi_ai::ImageContent::new("abc", "image/png")),
            ]),
            1,
        );
        let img_tokens = estimate_tokens(&AgentMessage::Llm(Box::new(Message::User(with_image))));
        assert!(img_tokens >= 4800 / 4);

        let mut asst = AssistantMessage::new("a", "p", "m", 1);
        asst.content = vec![
            AssistantContent::Text(TextContent::new("hello")),
            AssistantContent::Thinking(pi_ai::ThinkingContent::new("think")),
            AssistantContent::ToolCall(ToolCall::new(
                "1",
                "read",
                Map::from_iter([("path".into(), Value::String("x".into()))]),
            )),
        ];
        assert!(estimate_tokens(&AgentMessage::Llm(Box::new(Message::Assistant(asst)))) > 0);

        assert!(estimate_tokens(&tool_result_msg("result text")) > 0);
        assert!(estimate_tokens(&custom_msg("custom body")) > 0);
        assert!(estimate_tokens(&bash_msg("ls", "out")) > 0);
        assert!(estimate_tokens(&branch_summary_msg("branch")) > 0);
        assert!(estimate_tokens(&compaction_summary_msg("compact")) > 0);
        // Non-BMP emoji: one scalar, two UTF-16 code units → ceil(2/4)=1.
        assert_eq!(estimate_tokens(&user_msg("😀")), 1);
        // Two emoji = 4 UTF-16 units → 1 token.
        assert_eq!(estimate_tokens(&user_msg("😀😀")), 1);
        // Five emoji = 10 UTF-16 units → 3 tokens.
        assert_eq!(estimate_tokens(&user_msg("😀😀😀😀😀")), 3);
    }

    #[test]
    fn should_compact_strict_gt_and_disabled() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 20_000,
        };
        assert!(should_compact(95_000, 100_000, &settings));
        assert!(!should_compact(90_000, 100_000, &settings)); // equal threshold
        assert!(!should_compact(89_000, 100_000, &settings));
        // JS signed subtraction: reserve > window yields a negative threshold.
        let oversized_reserve = CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 20_000,
        };
        assert!(should_compact(0, 5, &oversized_reserve));
        let disabled = CompactionSettings {
            enabled: false,
            ..settings
        };
        assert!(!should_compact(95_000, 100_000, &disabled));
    }

    #[test]
    fn find_cut_point_never_cuts_tool_result() {
        let u = message_entry("u", None, user_msg("hi"));
        let a = message_entry("a", Some("u"), {
            let mut m = AssistantMessage::new("a", "p", "m", 1);
            m.content = vec![AssistantContent::ToolCall(ToolCall::new(
                "1",
                "read",
                Map::from_iter([("path".into(), Value::String("f".into()))]),
            ))];
            m.usage = usage(0, 0, 0, 0);
            AgentMessage::Llm(Box::new(Message::Assistant(m)))
        });
        let tr = message_entry("tr", Some("a"), tool_result_msg(&"x".repeat(8000)));
        let a2 = message_entry("a2", Some("tr"), assistant_msg("done", usage(0, 50, 0, 0)));
        let entries = [&u, &a, &tr, &a2];
        let result = find_cut_point(&entries, 0, entries.len(), 1);
        // Cut should never land on toolResult.
        assert_ne!(entries[result.first_kept_entry_index].id(), Some("tr"));
    }

    #[test]
    fn find_cut_point_metadata_expansion_and_split_turn() {
        let u1 = message_entry("u1", None, user_msg("Turn 1"));
        let a1 = message_entry(
            "a1",
            Some("u1"),
            assistant_msg("A1", usage(0, 100, 1000, 0)),
        );
        let u2 = message_entry("u2", Some("a1"), user_msg("Turn 2"));
        let mc = model_change_entry("mc", Some("u2")); // metadata before cut
        let a2 = message_entry(
            "a2",
            Some("mc"),
            assistant_msg("A2", usage(0, 100, 8000, 0)),
        );
        let a3 = message_entry(
            "a3",
            Some("a2"),
            assistant_msg("A3", usage(0, 100, 10000, 0)),
        );
        let entries = [&u1, &a1, &u2, &mc, &a2, &a3];
        let result = find_cut_point(&entries, 0, entries.len(), 3000);
        // If cut lands on assistant, split turn should point at turn start u2.
        if entries[result.first_kept_entry_index]
            .id()
            .is_some_and(|id| id == "a2" || id == "a3")
        {
            assert!(result.is_split_turn);
            assert_eq!(entries[result.turn_start_index].id(), Some("u2"));
        }
    }

    #[test]
    fn find_cut_point_custom_message_budget() {
        let u = message_entry("u", None, user_msg("hi"));
        let a = message_entry("a", Some("u"), assistant_msg("hello", usage(100, 50, 0, 0)));
        let c = custom_message_entry("c", Some("a"), &"x".repeat(4000));
        let a2 = message_entry("a2", Some("c"), assistant_msg("ok", usage(100, 50, 0, 0)));
        let entries = [&u, &a, &c, &a2];
        let tiny = find_cut_point(&entries, 0, entries.len(), 1);
        assert_eq!(tiny.first_kept_entry_index, 3);
        assert!(tiny.is_split_turn);
        assert_eq!(tiny.turn_start_index, 2);
        let fits = find_cut_point(&entries, 0, entries.len(), 2);
        assert_eq!(fits.first_kept_entry_index, 2);
        assert!(!fits.is_split_turn);
    }

    #[test]
    fn prepare_compaction_previous_boundary_and_nothing() {
        let u1 = message_entry("u1", None, user_msg("user msg 1"));
        let a1 = message_entry(
            "a1",
            Some("u1"),
            assistant_msg("assistant msg 1", usage(100, 50, 0, 0)),
        );
        let u2 = message_entry("u2", Some("a1"), user_msg("user msg 2 - kept"));
        let a2 = message_entry(
            "a2",
            Some("u2"),
            assistant_msg("assistant msg 2", usage(100, 50, 0, 0)),
        );
        let compaction = compaction_entry("c1", Some("a2"), "First summary", "u2");
        let u3 = message_entry("u3", Some("c1"), user_msg("new"));
        let a3 = message_entry(
            "a3",
            Some("u3"),
            assistant_msg("new a", usage(100, 50, 0, 0)),
        );
        let path = [&u1, &a1, &u2, &a2, &compaction, &u3, &a3];
        let _default = result_ok(prepare_compaction(&path, DEFAULT_COMPACTION_SETTINGS));
        // With default keepRecent 20000, this tiny session may be None — force tiny keep.
        let settings = CompactionSettings {
            keep_recent_tokens: 1,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        let prep = option_some(result_ok(prepare_compaction(&path, settings)));
        assert_eq!(prep.previous_summary.as_deref(), Some("First summary"));
        assert!(!prep.first_kept_entry_id.is_empty());

        // Already compacted
        let path2 = [&u1, &a1, &compaction];
        assert!(result_ok(prepare_compaction(&path2, settings)).is_none());
        assert!(matches!(
            preparation_none_error(&path2),
            CompactionError::AlreadyCompacted
        ));
    }

    #[test]
    fn prepare_compaction_nothing_to_compact_error() {
        let u = message_entry("u", None, user_msg("hi"));
        let a = message_entry("a", Some("u"), assistant_msg("yo", usage(10, 5, 0, 0)));
        let path = [&u, &a];
        let settings = CompactionSettings {
            keep_recent_tokens: 50_000,
            ..DEFAULT_COMPACTION_SETTINGS
        };
        let prep = result_ok(prepare_compaction(&path, settings));
        assert!(prep.is_none());
        assert!(matches!(
            preparation_none_error(&path),
            CompactionError::NothingToCompact
        ));
    }

    #[tokio::test]
    async fn compact_prompts_caps_custom_and_split_merge() {
        let captured_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured_max = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let prompts = Arc::clone(&captured_prompts);
        let maxes = Arc::clone(&captured_max);

        let stream_fn: SummarizeStreamFn = Arc::new(move |model, ctx, opts| {
            let prompts = Arc::clone(&prompts);
            let maxes = Arc::clone(&maxes);
            Box::pin(async move {
                if let Some(max) = opts.max_tokens {
                    result_ok(maxes.lock()).push(max);
                }
                if let Some(Message::User(user)) = ctx.messages.first() {
                    let text = match &user.content {
                        UserMessageContent::Text(t) => t.clone(),
                        UserMessageContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|b| match b {
                                UserContent::Text(t) => Some(t.text.as_str()),
                                UserContent::Image(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    };
                    result_ok(prompts.lock()).push(text);
                }
                let _ = model;
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new("SUMMARY"))];
                msg.stop_reason = StopReason::Stop;
                let stream = futures::stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::Stop,
                    message: msg,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        });

        let model = test_model(4096, 128_000);
        let prep = CompactionPreparation {
            first_kept_entry_id: "keep".into(),
            messages_to_summarize: vec![
                user_msg("history user"),
                assistant_msg("hist a", usage(10, 5, 0, 0)),
            ],
            turn_prefix_messages: vec![
                user_msg("turn prefix"),
                assistant_msg("prefix a", usage(10, 5, 0, 0)),
            ],
            is_split_turn: true,
            tokens_before: 999,
            previous_summary: Some("Prev".into()),
            file_ops: FileOperations::default(),
            settings: CompactionSettings {
                enabled: true,
                reserve_tokens: 1000,
                keep_recent_tokens: 100,
            },
        };

        let result = result_ok(
            compact(
                &prep,
                CompactOptions {
                    model: &model,
                    api_key: None,
                    headers: None,
                    custom_instructions: Some("focus on tests"),
                    signal: None,
                    thinking_level: None,
                    stream_fn,
                    env: None,
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );

        assert!(result.summary.contains("SUMMARY"));
        assert!(result.summary.contains("**Turn Context (split turn):**"));
        assert_eq!(result.first_kept_entry_id, "keep");
        assert_eq!(result.tokens_before, 999);

        let prompts = result_ok(captured_prompts.lock());
        assert_eq!(prompts.len(), 2);
        // History uses UPDATE prompt + previous-summary + custom append.
        assert!(prompts[0].contains("<previous-summary>"));
        assert!(prompts[0].contains(UPDATE_SUMMARIZATION_PROMPT));
        assert!(prompts[0].contains("Additional focus: focus on tests"));
        // Turn prefix uses TURN_PREFIX prompt, no custom append.
        assert!(prompts[1].contains(TURN_PREFIX_SUMMARIZATION_PROMPT));
        assert!(!prompts[1].contains("Additional focus"));

        let maxes = result_ok(captured_max.lock());
        // history: floor(0.8 * 1000)=800, turn: floor(0.5*1000)=500, both capped by model 4096
        assert_eq!(maxes[0], 800);
        assert_eq!(maxes[1], 500);
    }

    #[tokio::test]
    async fn empty_previous_summary_uses_initial_prompt() {
        let captured_prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let prompts = Arc::clone(&captured_prompts);
        let stream_fn: SummarizeStreamFn = Arc::new(move |_model, ctx, _opts| {
            let prompts = Arc::clone(&prompts);
            Box::pin(async move {
                if let Some(Message::User(user)) = ctx.messages.first() {
                    let text = match &user.content {
                        UserMessageContent::Text(t) => t.clone(),
                        UserMessageContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|b| match b {
                                UserContent::Text(t) => Some(t.text.as_str()),
                                UserContent::Image(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    };
                    result_ok(prompts.lock()).push(text);
                }
                let mut msg = AssistantMessage::new("a", "p", "m", 1);
                msg.content = vec![AssistantContent::Text(TextContent::new("SUMMARY"))];
                msg.stop_reason = StopReason::Stop;
                let stream = futures::stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::Stop,
                    message: msg,
                })]);
                Box::pin(stream)
                    as Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        });

        let model = test_model(2048, 128_000);
        let prep = CompactionPreparation {
            first_kept_entry_id: "k".into(),
            messages_to_summarize: vec![user_msg("x")],
            turn_prefix_messages: vec![],
            is_split_turn: false,
            tokens_before: 1,
            previous_summary: Some(String::new()),
            file_ops: FileOperations::default(),
            settings: DEFAULT_COMPACTION_SETTINGS,
        };
        result_ok(
            compact(
                &prep,
                CompactOptions {
                    model: &model,
                    api_key: None,
                    headers: None,
                    custom_instructions: None,
                    signal: None,
                    thinking_level: None,
                    stream_fn,
                    env: None,
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );

        let prompts = result_ok(captured_prompts.lock());
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains(SUMMARIZATION_PROMPT));
        assert!(!prompts[0].contains(UPDATE_SUMMARIZATION_PROMPT));
        assert!(!prompts[0].contains("<previous-summary>"));
    }

    #[test]
    fn emoji_utf16_cut_point_budget() {
        // Each 😀 is 2 UTF-16 units → 1 token. Budget 2 keeps last two user msgs.
        let u1 = message_entry("u1", None, user_msg("😀"));
        let u2 = message_entry("u2", Some("u1"), user_msg("😀"));
        let u3 = message_entry("u3", Some("u2"), user_msg("😀"));
        let entries = [&u1, &u2, &u3];
        let cut = find_cut_point(&entries, 0, entries.len(), 2);
        assert_eq!(cut.first_kept_entry_index, 1);
    }

    #[tokio::test]
    async fn compact_summarizer_failure_and_cancel() {
        let model = test_model(2048, 128_000);
        let prep = CompactionPreparation {
            first_kept_entry_id: "k".into(),
            messages_to_summarize: vec![user_msg("x")],
            turn_prefix_messages: vec![],
            is_split_turn: false,
            tokens_before: 1,
            previous_summary: None,
            file_ops: FileOperations::default(),
            settings: DEFAULT_COMPACTION_SETTINGS,
        };

        let err = result_err(
            compact(
                &prep,
                CompactOptions {
                    model: &model,
                    api_key: None,
                    headers: None,
                    custom_instructions: None,
                    signal: None,
                    thinking_level: None,
                    stream_fn: mock_stream_fn("x", StopReason::Error),
                    env: None,
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );
        assert_eq!(err.to_string(), "Summarization failed: boom");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = result_err(
            compact(
                &prep,
                CompactOptions {
                    model: &model,
                    api_key: None,
                    headers: None,
                    custom_instructions: None,
                    signal: Some(cancel),
                    thinking_level: None,
                    stream_fn: mock_stream_fn("x", StopReason::Stop),
                    env: None,
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );
        assert!(matches!(err, CompactionError::Cancelled));
    }

    #[tokio::test]
    async fn compact_cancels_stalled_stream_item() -> Result<(), Box<dyn std::error::Error>> {
        let model = test_model(2_048, 128_000);
        let prep = CompactionPreparation {
            first_kept_entry_id: "k".into(),
            messages_to_summarize: vec![user_msg("x")],
            turn_prefix_messages: Vec::new(),
            is_split_turn: false,
            tokens_before: 1,
            previous_summary: None,
            file_ops: FileOperations::default(),
            settings: DEFAULT_COMPACTION_SETTINGS,
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let stream_fn: SummarizeStreamFn = Arc::new({
            let entered = Arc::clone(&entered);
            move |_model, _context, _options| {
                let entered = Arc::clone(&entered);
                Box::pin(async move {
                    entered.notify_one();
                    let stream: Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    > = Box::pin(futures::stream::pending());
                    stream
                })
            }
        });
        let cancel = CancellationToken::new();
        let mut run = Box::pin(compact(
            &prep,
            CompactOptions {
                model: &model,
                api_key: None,
                headers: None,
                custom_instructions: None,
                signal: Some(cancel.clone()),
                thinking_level: None,
                stream_fn,
                env: None,
                retry: None,
                retry_callbacks: None,
            },
        ));

        tokio::select! {
            () = entered.notified() => {}
            result = run.as_mut() => {
                return Err(format!("stalled stream returned early: {result:?}").into());
            }
        }
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), run.as_mut()).await?;
        assert!(matches!(result, Err(CompactionError::Cancelled)));
        Ok(())
    }

    #[tokio::test]
    async fn retry_summarization_call_returns_terminal_attempt_usage() {
        let policy = SummarizationRetryPolicy {
            enabled: true,
            max_retries: 1,
            base_delay_ms: 0,
        };

        let mut call_count = 0_u32;
        let result = retry_summarization_call(
            move || {
                let count = call_count;
                call_count = call_count.saturating_add(1);
                Box::pin(async move {
                    if count == 0 {
                        // Pinned TypeScript parity discards usage from retryable responses.
                        let mut msg = AssistantMessage::new(
                            "anthropic-messages",
                            "anthropic",
                            "claude-sonnet-4-5",
                            1,
                        );
                        msg.stop_reason = StopReason::Error;
                        msg.error_message = Some("overloaded_error".to_owned());
                        msg.usage = Usage {
                            input: 100,
                            output: 50,
                            cache_read: 10,
                            cache_write: 5,
                            cache_write1h: Some(3),
                            reasoning: Some(20),
                            total_tokens: 165,
                            cost: pi_ai::UsageCost {
                                input: 0.01,
                                output: 0.02,
                                cache_read: 0.001,
                                cache_write: 0.005,
                                total: 0.036,
                            },
                        };
                        Ok(msg)
                    } else {
                        let mut msg = AssistantMessage::new(
                            "anthropic-messages",
                            "anthropic",
                            "claude-sonnet-4-5",
                            1,
                        );
                        msg.content = vec![AssistantContent::Text(TextContent::new("summary"))];
                        msg.stop_reason = StopReason::Stop;
                        msg.usage = Usage {
                            input: 200,
                            output: 100,
                            cache_read: 20,
                            cache_write: 10,
                            cache_write1h: Some(7),
                            reasoning: Some(40),
                            total_tokens: 330,
                            cost: pi_ai::UsageCost {
                                input: 0.02,
                                output: 0.04,
                                cache_read: 0.002,
                                cache_write: 0.01,
                                total: 0.072,
                            },
                        };
                        Ok(msg)
                    }
                })
            },
            Some(&policy),
            None,
            None,
        )
        .await;

        let response = result_ok(result);

        assert_eq!(response.stop_reason, StopReason::Stop);
        assert_eq!(assistant_text(&response), "summary");
        assert_eq!(response.usage.input, 200);
        assert_eq!(response.usage.output, 100);
        assert_eq!(response.usage.cache_read, 20);
        assert_eq!(response.usage.cache_write, 10);
        assert_eq!(response.usage.cache_write1h, Some(7));
        assert_eq!(response.usage.reasoning, Some(40));
        assert_eq!(response.usage.total_tokens, 330);
        assert!((response.usage.cost.input - 0.02).abs() < 1e-9);
        assert!((response.usage.cost.output - 0.04).abs() < 1e-9);
        assert!((response.usage.cost.cache_read - 0.002).abs() < 1e-9);
        assert!((response.usage.cost.cache_write - 0.01).abs() < 1e-9);
        assert!((response.usage.cost.total - 0.072).abs() < 1e-9);
    }
}
