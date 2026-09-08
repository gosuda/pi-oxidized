//! Context compaction algorithms and provider-backed summary generation.
//!
//! This is the Rust port of
//! `.references/pi/packages/agent/src/harness/compaction/compaction.ts`.
//! Structural preparation is pure and durable; model calls cross the explicit
//! [`SummaryRequest`] boundary so runtime/storage code owns persistence and
//! provider configuration.

use std::future::Future;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::StreamExt;
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention,
    Context as AiContext, Message, Model, ModelThinkingLevel, ProviderError, StopReason,
    StreamOptionKey, StreamOptions, Usage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::context::Context;
use crate::message::AgentMessage;
use crate::session::{
    CompactionSettings, DurableStructuralPreparation, Entry, EntryBase, EntryId, HarnessRetryPolicy,
};

use super::utils::{
    FileLists, FileOperations, assistant_content_text, compute_file_lists,
    estimate_custom_content_chars, estimate_text_and_image_content_chars,
    estimate_tool_result_content_chars, extract_file_ops_from_message, format_file_operations,
    js_string_len, safe_json_stringify, serialize_conversation,
};
use super::{
    context::{build_context_entries, session_entry_to_context_messages},
    messages::{
        create_branch_summary_message, create_compaction_summary_message, message_timestamp,
        summary_user_message,
    },
};
use crate::harness::api::HarnessModels;
use crate::harness::hooks::CompactResult;

/// Request boundary used by compaction and branch-summary algorithms.
///
/// The runtime normally supplies a closure backed by [`HarnessModels::stream`].
/// Tests and alternate owners can supply another request implementation without
/// changing the pure preparation algorithms.
pub type SummaryRequest = Arc<
    dyn Fn(
            AiContext,
            StreamOptions,
            Context,
        ) -> BoxFuture<'static, Result<AssistantMessage, ProviderError>>
        + Send
        + Sync,
>;

/// Retry callback invoked when a retry is scheduled.
pub type RetryScheduledCallback =
    Arc<dyn Fn(u64, u64, u64, String) -> BoxFuture<'static, ()> + Send + Sync>;
/// Retry callback invoked immediately before a retry attempt.
pub type RetryAttemptCallback = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
/// Retry callback invoked when the retry loop finishes.
pub type RetryFinishedCallback =
    Arc<dyn Fn(bool, u64, Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Async retry lifecycle callbacks.
#[derive(Clone, Default)]
pub struct SummaryRetryCallbacks {
    /// Called before a retry sleeps: `(attempt, max_retries, delay_ms, error)`.
    pub on_retry_scheduled: Option<RetryScheduledCallback>,
    /// Called immediately before a retry attempt.
    pub on_retry_attempt_start: Option<RetryAttemptCallback>,
    /// Called once when the retry loop finishes: `(success, attempts, error)`.
    pub on_retry_finished: Option<RetryFinishedCallback>,
}

/// Stable compaction error categories.
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    /// The summary request was cancelled.
    #[error("compaction summary aborted: {0}")]
    Aborted(String),
    /// The provider returned a terminal semantic error.
    #[error("compaction summarization failed: {0}")]
    SummarizationFailed(String),
    /// The request boundary failed before a terminal assistant response.
    #[error("compaction transport failed: {0}")]
    Transport(#[from] ProviderError),
}

impl CompactionError {
    /// Stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Aborted(_) => "aborted",
            Self::SummarizationFailed(_) => "summarization_failed",
            Self::Transport(_) => "transport",
        }
    }
}

/// Summary text and provider usage returned by one summary request.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedSummary {
    /// Structured summary text.
    pub text: String,
    /// Provider usage for the request.
    pub usage: Usage,
}

/// Options used to generate one history summary.
#[derive(Clone, Debug)]
pub struct SummaryGenerationOptions {
    /// Model used for the request.
    pub model: Model,
    /// Token budget reserved for the summary prompt and output.
    pub reserve_tokens: u64,
    /// Optional additional instructions.
    pub custom_instructions: Option<String>,
    /// Existing summary being updated, if this is an iterative compaction.
    pub previous_summary: Option<String>,
    /// Requested reasoning level.
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Options used to generate a compaction result.
#[derive(Clone, Debug)]
pub struct CompactGenerationOptions {
    /// Model used for summary requests.
    pub model: Model,
    /// Optional additional instructions.
    pub custom_instructions: Option<String>,
    /// Requested reasoning level.
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Summary request system instruction.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Default structured summary instruction.
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Iterative-summary instruction.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n### In Progress\n- [ ] [Current work - update based on progress]\n### Blocked\n- [Current blockers - remove if resolved]\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n## Next Steps\n1. [Update based on current state]\n## Critical Context\n- [Preserve important context, add new if needed]\n- [Or \"(none)\" if not applicable]\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Prompt instruction for a split-turn prefix summary.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

/// Calculate total context tokens from provider usage.
#[must_use]
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    pi_ai::calculate_context_tokens(usage)
}

/// Returns usage from the last valid assistant entry.
#[must_use]
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<Usage> {
    entries.iter().rev().find_map(|entry| {
        let Entry::Message { message, .. } = entry else {
            return None;
        };
        get_assistant_usage(message)
    })
}

fn get_assistant_usage(message: &AgentMessage) -> Option<Usage> {
    let AgentMessage::Llm(message) = message else {
        return None;
    };
    let Message::Assistant(assistant) = message.as_ref() else {
        return None;
    };
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    (calculate_context_tokens(&assistant.usage) > 0).then(|| assistant.usage.clone())
}
/// Estimates one agent message using the provider's 4-character heuristic for
/// LLM messages and the harness custom-role payloads for custom messages.
#[must_use]
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Llm(message) => estimate_llm_message_tokens(message.as_ref()),
        AgentMessage::Custom(custom) => match custom.role.as_str() {
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
                js_string_len(command)
                    .saturating_add(js_string_len(output))
                    .div_ceil(4)
            }
            "branchSummary" | "compactionSummary" => custom
                .payload
                .get("summary")
                .and_then(Value::as_str)
                .map_or(0, js_string_len)
                .div_ceil(4),
            "custom" => custom
                .payload
                .get("content")
                .map_or(0, estimate_custom_content_chars)
                .div_ceil(4),
            _ => 0,
        },
    }
}

fn estimate_llm_message_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::User(message) => estimate_text_and_image_content_chars(&message.content),
        Message::ToolResult(message) => estimate_tool_result_content_chars(&message.content),
        Message::Assistant(message) => message
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => js_string_len(&text.text),
                AssistantContent::Thinking(thinking) => js_string_len(&thinking.thinking),
                AssistantContent::ToolCall(call) => js_string_len(&call.name).saturating_add(
                    js_string_len(&safe_json_stringify(&Value::Object(call.arguments.clone()))),
                ),
            })
            .sum(),
    };
    chars.div_ceil(4)
}

/// Estimates context usage, anchored on the newest applicable assistant usage.
#[must_use]
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> pi_ai::ContextUsageEstimate {
    if let Some((index, usage)) = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| get_assistant_usage(message).map(|usage| (index, usage)))
    {
        let usage_tokens = calculate_context_tokens(&usage);
        let trailing_tokens = messages[index.saturating_add(1)..]
            .iter()
            .map(estimate_tokens)
            .sum();
        pi_ai::ContextUsageEstimate {
            tokens: usage_tokens.saturating_add(trailing_tokens),
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        }
    } else {
        let tokens = messages.iter().map(estimate_tokens).sum();
        pi_ai::ContextUsageEstimate {
            tokens,
            usage_tokens: 0,
            trailing_tokens: tokens,
            last_usage_index: None,
        }
    }
}

/// Returns whether context usage exceeds the configured compaction threshold.
#[must_use]
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// Finds entries that can start a retained context boundary.
#[must_use]
pub fn find_valid_cut_points(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let end_index = end_index.min(entries.len());
    let start_index = start_index.min(end_index);
    let mut cut_points = Vec::new();
    for (offset, entry) in entries[start_index..end_index].iter().enumerate() {
        let index = start_index + offset;
        match entry {
            Entry::Message { message, .. } => match message.role() {
                "bashExecution" | "custom" | "branchSummary" | "compactionSummary" | "user"
                | "assistant" => cut_points.push(index),
                _ => {}
            },
            Entry::BranchSummary { .. } => cut_points.push(index),
            _ => {}
        }
    }
    cut_points
}

/// Finds the user-visible turn start containing an entry.
#[must_use]
pub fn find_turn_start_index(
    entries: &[Entry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    if entries.is_empty() || entry_index >= entries.len() {
        return None;
    }
    let start_index = start_index.min(entry_index);
    for (offset, entry) in entries[start_index..=entry_index].iter().enumerate().rev() {
        let index = start_index + offset;
        match entry {
            Entry::BranchSummary { .. } => return Some(index),
            Entry::Message { message, .. }
                if matches!(message.role(), "user" | "bashExecution") =>
            {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

/// Selected compaction boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutPointResult {
    /// First entry retained after compaction.
    pub first_kept_entry_index: usize,
    /// Turn start when the boundary splits a turn.
    pub turn_start_index: Option<usize>,
    /// Whether the boundary splits an in-progress turn.
    pub is_split_turn: bool,
}

/// Finds a boundary that retains approximately `keep_recent_tokens`.
#[must_use]
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let end_index = end_index.min(entries.len());
    let start_index = start_index.min(end_index);
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0_u64;
    let mut cut_index = cut_points[0];
    for index in (start_index..end_index).rev() {
        let Entry::Message { message, .. } = &entries[index] else {
            continue;
        };
        accumulated_tokens = accumulated_tokens.saturating_add(estimate_tokens(message));
        if accumulated_tokens >= keep_recent_tokens {
            if let Some(candidate) = cut_points
                .iter()
                .copied()
                .find(|candidate| *candidate >= index)
            {
                cut_index = candidate;
            }
            break;
        }
    }

    while cut_index > start_index {
        match &entries[cut_index - 1] {
            Entry::Compaction { .. } | Entry::Message { .. } => break,
            _ => cut_index -= 1,
        }
    }
    let is_user_message = matches!(
        entries.get(cut_index),
        Some(Entry::Message { message, .. }) if message.role() == "user"
    );
    let turn_start_index = if is_user_message {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };
    CutPointResult {
        first_kept_entry_index: cut_index,
        is_split_turn: !is_user_message && turn_start_index.is_some(),
        turn_start_index,
    }
}

/// Build a compactable preparation from a path.
///
/// # Errors
///
/// This preparation stage currently has no error cases and always returns
/// `Ok`.
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: &CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty() || matches!(path_entries.last(), Some(Entry::Compaction { .. })) {
        return Ok(None);
    }

    let previous_compaction = path_entries
        .iter()
        .enumerate()
        .rfind(|(_, entry)| matches!(entry, Entry::Compaction { .. }));
    let previous_compaction_index = previous_compaction.as_ref().map(|pair| pair.0);
    let (previous_summary, compactable_entries) = if let Some((
        index,
        Entry::Compaction {
            base,
            retained_tail,
            summary,
            ..
        },
    )) = previous_compaction
    {
        let mut virtual_entries =
            Vec::with_capacity(retained_tail.len() + path_entries.len() - index - 1);
        for (retained_index, message) in retained_tail.iter().enumerate() {
            let id = EntryId::new(format!("{}:retained:{retained_index}", base.id.as_str()));
            let parent_id = if retained_index == 0 {
                Some(base.id.clone())
            } else {
                Some(EntryId::new(format!(
                    "{}:retained:{}",
                    base.id.as_str(),
                    retained_index - 1
                )))
            };
            virtual_entries.push(Entry::Message {
                base: EntryBase {
                    id,
                    parent_id,
                    seq: base.seq,
                    timestamp: message_timestamp(message),
                    custom_type: None,
                },
                message: message.clone(),
                terminate: false,
            });
        }
        virtual_entries.extend(path_entries[index.saturating_add(1)..].iter().cloned());
        (Some(summary.clone()), virtual_entries)
    } else {
        (None, path_entries.to_vec())
    };

    let context_messages = build_context_entries(path_entries)
        .into_iter()
        .flat_map(session_entry_to_context_messages)
        .collect::<Vec<_>>();
    let tokens_before = estimate_context_tokens(&context_messages).tokens;
    let cut = find_cut_point(
        &compactable_entries,
        0,
        compactable_entries.len(),
        settings.keep_recent_tokens,
    );
    let history_end = if cut.is_split_turn {
        cut.turn_start_index.unwrap_or(cut.first_kept_entry_index)
    } else {
        cut.first_kept_entry_index
    };

    let messages_to_summarize = compactable_entries[..history_end]
        .iter()
        .filter_map(get_message_from_entry_for_compaction)
        .collect::<Vec<_>>();
    let turn_prefix_messages = if cut.is_split_turn {
        let turn_start = cut.turn_start_index.unwrap_or(cut.first_kept_entry_index);
        compactable_entries[turn_start..cut.first_kept_entry_index]
            .iter()
            .filter_map(get_message_from_entry_for_compaction)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let retained_tail = compactable_entries[cut.first_kept_entry_index..]
        .iter()
        .filter_map(get_message_from_entry_for_compaction)
        .collect::<Vec<_>>();

    let mut file_ops = extract_file_operations(
        path_entries,
        previous_compaction_index,
        &messages_to_summarize,
    );
    for message in &turn_prefix_messages {
        extract_file_ops_from_message(message, &mut file_ops);
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: *settings,
    }))
}

fn extract_file_operations(
    entries: &[Entry],
    previous_compaction_index: Option<usize>,
    messages: &[AgentMessage],
) -> FileOperations {
    let mut file_ops = FileOperations::default();
    if let Some(index) = previous_compaction_index
        && let Entry::Compaction {
            details: Some(details),
            ..
        } = &entries[index]
        && let Some(object) = details.as_object()
    {
        if let Some(paths) = object.get("readFiles").and_then(Value::as_array) {
            for path in paths.iter().filter_map(Value::as_str) {
                file_ops.read.insert(path.to_owned());
            }
        }
        if let Some(paths) = object.get("modifiedFiles").and_then(Value::as_array) {
            for path in paths.iter().filter_map(Value::as_str) {
                file_ops.edited.insert(path.to_owned());
            }
        }
    }
    for message in messages {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    file_ops
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } => Some(message.clone()),
        Entry::BranchSummary {
            base,
            from_id,
            summary,
            ..
        } => Some(create_branch_summary_message(
            summary,
            from_id.as_ref(),
            base.timestamp,
        )),
        Entry::Compaction {
            base,
            summary,
            tokens_before,
            ..
        } => Some(create_compaction_summary_message(
            summary,
            *tokens_before,
            base.timestamp,
        )),
        Entry::Custom { .. } => None,
    }
}

fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    if matches!(entry, Entry::Compaction { .. }) {
        None
    } else {
        get_message_from_entry(entry)
    }
}

/// Durable-ready preparation for one compaction operation.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    /// Messages folded into the summary.
    pub messages_to_summarize: Vec<AgentMessage>,
    /// Prefix of a split turn summarized separately.
    pub turn_prefix_messages: Vec<AgentMessage>,
    /// Recent messages retained verbatim after the summary.
    pub retained_tail: Vec<AgentMessage>,
    /// Whether the cut split an in-progress turn.
    pub is_split_turn: bool,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Existing summary being updated, if any.
    pub previous_summary: Option<String>,
    /// File operations observed in the summarized span.
    pub file_ops: FileOperations,
    /// Settings used to build the preparation.
    pub settings: CompactionSettings,
}

impl CompactionPreparation {
    /// Converts live sets into the canonical durable preparation variant.
    #[must_use]
    pub fn to_durable(&self) -> DurableStructuralPreparation {
        DurableStructuralPreparation::Compaction {
            messages_to_summarize: self.messages_to_summarize.clone(),
            turn_prefix_messages: self.turn_prefix_messages.clone(),
            retained_tail: self.retained_tail.clone(),
            is_split_turn: self.is_split_turn,
            tokens_before: self.tokens_before,
            previous_summary: self.previous_summary.clone(),
            file_ops: self.file_ops.to_durable(),
            settings: self.settings,
        }
    }

    /// Rehydrates a live preparation; returns `None` for branch summaries.
    #[must_use]
    pub fn from_durable(value: DurableStructuralPreparation) -> Option<Self> {
        let DurableStructuralPreparation::Compaction {
            messages_to_summarize,
            turn_prefix_messages,
            retained_tail,
            is_split_turn,
            tokens_before,
            previous_summary,
            file_ops,
            settings,
        } = value
        else {
            return None;
        };
        Some(Self {
            messages_to_summarize,
            turn_prefix_messages,
            retained_tail,
            is_split_turn,
            tokens_before,
            previous_summary,
            file_ops: FileOperations::from_durable(file_ops),
            settings,
        })
    }
}

/// Generates a summary through the caller-owned request boundary.
///
/// # Errors
///
/// Returns [`CompactionError::Aborted`] when the request is cancelled,
/// [`CompactionError::Transport`] when the provider boundary fails, or
/// [`CompactionError::SummarizationFailed`] when the provider returns an
/// error response.
pub async fn generate_summary_with_request(
    current_messages: &[AgentMessage],
    options: &SummaryGenerationOptions,
    request: &SummaryRequest,
    cx: &Context,
) -> Result<GeneratedSummary, CompactionError> {
    let max_tokens = options
        .reserve_tokens
        .saturating_mul(4)
        .checked_div(5)
        .unwrap_or(0)
        .min(if options.model.max_tokens > 0 {
            options.model.max_tokens
        } else {
            u64::MAX
        });
    let mut base_prompt = if options.previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if let Some(custom) = options
        .custom_instructions
        .as_deref()
        .filter(|custom| !custom.is_empty())
    {
        base_prompt.push_str("\n\nAdditional focus: ");
        base_prompt.push_str(custom);
    }
    let mut prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n",
        serialize_conversation(&super::messages::convert_to_llm(current_messages))
    );
    if let Some(previous) = options.previous_summary.as_deref() {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</previous-summary>\n\n");
    }
    prompt.push_str(&base_prompt);

    let mut request_options = StreamOptions {
        max_tokens: Some(max_tokens),
        ..StreamOptions::default()
    };
    if options.model.reasoning
        && let Some(level) = options.thinking_level
        && level != ModelThinkingLevel::Off
    {
        request_options.insert_extra(
            StreamOptionKey::REASONING,
            Value::String(thinking_level_string(level).to_owned()),
        );
    }
    let response = request(
        AiContext {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: vec![summary_user_message(&prompt)],
            tools: None,
        },
        create_summary_request_options(&request_options, cx),
        cx.clone(),
    )
    .await
    .map_err(|error| {
        if provider_error_is_cancellation(&error) {
            CompactionError::Aborted(error.to_string())
        } else {
            CompactionError::Transport(error)
        }
    })?;
    classify_summary_response(response, "Summarization", "compaction")
}

/// Generates plain summary text through [`generate_summary_with_request`].
///
/// # Errors
///
/// Propagates errors from summary generation.
pub async fn generate_summary(
    current_messages: &[AgentMessage],
    models: &Arc<dyn HarnessModels>,
    options: &SummaryGenerationOptions,
    retry: Option<&HarnessRetryPolicy>,
    callbacks: Option<&SummaryRetryCallbacks>,
    cx: &Context,
) -> Result<String, CompactionError> {
    Ok(
        generate_summary_with_usage(current_messages, models, options, retry, callbacks, cx)
            .await?
            .text,
    )
}

/// Generates summary text and usage using the configured provider stream.
///
/// # Errors
///
/// Propagates request, cancellation, and summarization failures from the
/// provider.
pub async fn generate_summary_with_usage(
    current_messages: &[AgentMessage],
    models: &Arc<dyn HarnessModels>,
    options: &SummaryGenerationOptions,
    retry: Option<&HarnessRetryPolicy>,
    callbacks: Option<&SummaryRetryCallbacks>,
    cx: &Context,
) -> Result<GeneratedSummary, CompactionError> {
    let models = Arc::clone(models);
    let model = options.model.clone();
    let retry = retry.copied();
    let callbacks = callbacks.cloned();
    let request: SummaryRequest = Arc::new(move |ai_context, request_options, request_cx| {
        let models = Arc::clone(&models);
        let model = model.clone();
        let retry = retry;
        let callbacks = callbacks.clone();
        Box::pin(async move {
            complete_simple_with_retries(
                &models,
                &model,
                ai_context,
                request_options,
                retry.as_ref(),
                callbacks.as_ref(),
                &request_cx,
            )
            .await
        })
    });
    generate_summary_with_request(current_messages, options, &request, cx).await
}

/// Generates a compaction result through the configured provider stream.
///
/// # Errors
///
/// Propagates provider, cancellation, and summarization failures from the
/// summary request.
pub async fn compact(
    preparation: &CompactionPreparation,
    models: &Arc<dyn HarnessModels>,
    options: &CompactGenerationOptions,
    retry: Option<&HarnessRetryPolicy>,
    callbacks: Option<&SummaryRetryCallbacks>,
    cx: &Context,
) -> Result<CompactResult, CompactionError> {
    let models = Arc::clone(models);
    let model = options.model.clone();
    let retry = retry.copied();
    let callbacks = callbacks.cloned();
    let request: SummaryRequest = Arc::new(move |ai_context, request_options, request_cx| {
        let models = Arc::clone(&models);
        let model = model.clone();
        let retry = retry;
        let callbacks = callbacks.clone();
        Box::pin(async move {
            complete_simple_with_retries(
                &models,
                &model,
                ai_context,
                request_options,
                retry.as_ref(),
                callbacks.as_ref(),
                &request_cx,
            )
            .await
        })
    });
    compact_with_request(preparation, options, &request, cx).await
}

/// Generates compaction data through a caller-owned request boundary.
///
/// # Errors
///
/// Propagates provider, cancellation, and summarization failures from the
/// summary requests.
pub async fn compact_with_request(
    preparation: &CompactionPreparation,
    options: &CompactGenerationOptions,
    request: &SummaryRequest,
    cx: &Context,
) -> Result<CompactResult, CompactionError> {
    let (summary, usage) =
        if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
            let mut history_text = "No prior history.".to_owned();
            let mut history_usage = None;
            if !preparation.messages_to_summarize.is_empty() {
                let result = generate_summary_with_request(
                    &preparation.messages_to_summarize,
                    &SummaryGenerationOptions {
                        model: options.model.clone(),
                        reserve_tokens: preparation.settings.reserve_tokens,
                        custom_instructions: options.custom_instructions.clone(),
                        previous_summary: preparation.previous_summary.clone(),
                        thinking_level: options.thinking_level,
                    },
                    request,
                    cx,
                )
                .await?;
                history_text = result.text;
                history_usage = Some(result.usage);
            }
            let prefix = generate_turn_prefix_summary(
                &preparation.turn_prefix_messages,
                &options.model,
                preparation.settings.reserve_tokens,
                options.thinking_level,
                request,
                cx,
            )
            .await?;
            (
                format!(
                    "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                    prefix.text
                ),
                history_usage.map_or(prefix.usage.clone(), |history| {
                    add_usage(&history, &prefix.usage)
                }),
            )
        } else {
            let result = generate_summary_with_request(
                &preparation.messages_to_summarize,
                &SummaryGenerationOptions {
                    model: options.model.clone(),
                    reserve_tokens: preparation.settings.reserve_tokens,
                    custom_instructions: options.custom_instructions.clone(),
                    previous_summary: preparation.previous_summary.clone(),
                    thinking_level: options.thinking_level,
                },
                request,
                cx,
            )
            .await?;
            (result.text, result.usage)
        };
    let FileLists {
        read_files,
        modified_files,
    } = compute_file_lists(&preparation.file_ops);
    let mut summary = summary;
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    let details = serde_json::to_value(CompactionDetails {
        read_files,
        modified_files,
    })
    .ok();
    Ok(CompactResult {
        summary,
        tokens_before: preparation.tokens_before,
        usage: Some(usage),
        retained_tail: preparation.retained_tail.clone(),
        details,
    })
}

async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    model: &Model,
    reserve_tokens: u64,
    thinking_level: Option<ModelThinkingLevel>,
    request: &SummaryRequest,
    cx: &Context,
) -> Result<GeneratedSummary, CompactionError> {
    let max_tokens = reserve_tokens
        .checked_div(2)
        .unwrap_or(0)
        .min(if model.max_tokens > 0 {
            model.max_tokens
        } else {
            u64::MAX
        });
    let conversation = serialize_conversation(&super::messages::convert_to_llm(messages));
    let prompt = format!(
        "<conversation>\n{conversation}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    let mut request_options = StreamOptions {
        max_tokens: Some(max_tokens),
        ..StreamOptions::default()
    };
    if model.reasoning
        && let Some(level) = thinking_level
        && level != ModelThinkingLevel::Off
    {
        request_options.insert_extra(
            StreamOptionKey::REASONING,
            Value::String(thinking_level_string(level).to_owned()),
        );
    }
    let response = request(
        AiContext {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: vec![summary_user_message(&prompt)],
            tools: None,
        },
        create_summary_request_options(&request_options, cx),
        cx.clone(),
    )
    .await
    .map_err(|error| {
        if provider_error_is_cancellation(&error) {
            CompactionError::Aborted(error.to_string())
        } else {
            CompactionError::Transport(error)
        }
    })?;
    classify_summary_response(response, "Turn prefix summarization", "turn prefix")
}

fn classify_summary_response(
    response: AssistantMessage,
    label: &str,
    _kind: &str,
) -> Result<GeneratedSummary, CompactionError> {
    match response.stop_reason {
        StopReason::Aborted => Err(CompactionError::Aborted(
            response
                .error_message
                .unwrap_or_else(|| format!("{label} aborted")),
        )),
        StopReason::Error => Err(CompactionError::SummarizationFailed(format!(
            "{label} failed: {}",
            response
                .error_message
                .unwrap_or_else(|| "Unknown error".to_owned())
        ))),
        _ => Ok(GeneratedSummary {
            text: assistant_content_text(&response.content, "\n"),
            usage: response.usage,
        }),
    }
}

/// Applies summary-owned request options: cancellation, cache isolation, and
/// a fresh request/session id when the caller has not supplied one.
#[must_use]
pub fn create_summary_request_options(options: &StreamOptions, cx: &Context) -> StreamOptions {
    let mut options = options.clone();
    options.signal = cx.token().cloned();
    options.cache_retention = Some(CacheRetention::None);
    if options.session_id.is_none() {
        options.session_id = Some(uuid::Uuid::now_v7().to_string());
    }
    options
}
fn provider_error_is_cancellation(error: &ProviderError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("cancel") || text.contains("abort")
}

/// Completes one provider stream, retrying retryable semantic failures.
///
/// # Errors
///
/// Returns provider errors produced while opening or consuming the stream.
pub async fn complete_simple_with_retries(
    models: &Arc<dyn HarnessModels>,
    model: &Model,
    ai_context: AiContext,
    options: StreamOptions,
    retry: Option<&HarnessRetryPolicy>,
    callbacks: Option<&SummaryRetryCallbacks>,
    cx: &Context,
) -> Result<AssistantMessage, ProviderError> {
    let request_options = create_summary_request_options(&options, cx);
    let signal = request_options.signal.clone();
    retry_assistant_call(
        || {
            let models = Arc::clone(models);
            let model = model.clone();
            let ai_context = ai_context.clone();
            let options = request_options.clone();
            async move { complete_simple(&models, &model, ai_context, options).await }
        },
        retry,
        signal,
        callbacks,
    )
    .await
}

async fn complete_simple(
    models: &Arc<dyn HarnessModels>,
    model: &Model,
    ai_context: AiContext,
    options: StreamOptions,
) -> Result<AssistantMessage, ProviderError> {
    let mut stream = models.stream(model, ai_context, options);
    let mut partial = None;
    while let Some(event) = stream.next().await {
        let event = event?;
        match event {
            AssistantMessageEvent::Start { partial: value }
            | AssistantMessageEvent::TextStart { partial: value, .. }
            | AssistantMessageEvent::ThinkingStart { partial: value, .. }
            | AssistantMessageEvent::ToolCallStart { partial: value, .. }
            | AssistantMessageEvent::TextEnd { partial: value, .. }
            | AssistantMessageEvent::ThinkingEnd { partial: value, .. }
            | AssistantMessageEvent::ToolCallEnd { partial: value, .. }
            | AssistantMessageEvent::TextDelta { partial: value, .. }
            | AssistantMessageEvent::ThinkingDelta { partial: value, .. }
            | AssistantMessageEvent::ToolCallDelta { partial: value, .. } => {
                partial = Some(value.as_ref().clone());
            }
            AssistantMessageEvent::Done { message, .. } => return Ok(message),
            AssistantMessageEvent::Error { error, .. } => return Ok(error),
        }
    }
    partial.ok_or_else(|| ProviderError::new("stream ended before a terminal response event"))
}

const NON_RETRYABLE: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

const RETRYABLE: &[&str] = &[
    "overloaded",
    "rate.?limit",
    "too many requests",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "enotfound",
    "eai_again",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "resourceexhausted",
];

/// Returns whether a terminal assistant failure is retryable.
#[must_use]
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    let lower = error.to_ascii_lowercase();
    if NON_RETRYABLE.iter().any(|pattern| lower.contains(pattern)) {
        return false;
    }
    if RETRYABLE
        .iter()
        .any(|pattern| wildcard_pattern_matches(&lower, pattern))
    {
        return true;
    }
    [429_u16, 500, 502, 503, 504, 524]
        .iter()
        .any(|code| contains_status_code(&lower, *code))
}

fn contains_status_code(text: &str, code: u16) -> bool {
    let code = code.to_string();
    let mut offset = 0;
    while let Some(index) = text[offset..].find(&code) {
        let start = offset + index;
        let end = start + code.len();
        let before_digit = start > 0 && text.as_bytes()[start - 1].is_ascii_digit();
        let after_digit = end < text.len() && text.as_bytes()[end].is_ascii_digit();
        if !before_digit && !after_digit {
            return true;
        }
        offset = end;
    }
    false
}

fn wildcard_pattern_matches(text: &str, pattern: &str) -> bool {
    let text = text.as_bytes();
    let pattern = pattern.as_bytes();
    (0..=text.len()).any(|start| wildcard_match_at(text, pattern, start, 0))
}

fn wildcard_match_at(text: &[u8], pattern: &[u8], text_index: usize, pattern_index: usize) -> bool {
    if pattern_index == pattern.len() {
        return true;
    }
    let pattern_byte = pattern[pattern_index];
    let optional = pattern.get(pattern_index + 1) == Some(&b'?');
    let (element, next_pattern) = if optional {
        (pattern_byte, pattern_index + 2)
    } else {
        (pattern_byte, pattern_index + 1)
    };
    let matches_one = if element == b'.' {
        text_index < text.len()
    } else {
        text.get(text_index).copied() == Some(element)
    };
    (matches_one && wildcard_match_at(text, pattern, text_index + 1, next_pattern))
        || (optional && wildcard_match_at(text, pattern, text_index, next_pattern))
}

/// Invokes the completion callback once retrying has begun.
async fn notify_retry_finished(
    callbacks: Option<&SummaryRetryCallbacks>,
    retried: bool,
    success: bool,
    attempts: u64,
    error: Option<String>,
) {
    if let (Some(callback), true) = (
        callbacks.and_then(|callbacks| callbacks.on_retry_finished.as_ref()),
        retried,
    ) {
        callback(success, attempts, error).await;
    }
}

/// Retries a producer after retryable semantic failures.
///
/// # Errors
///
/// Returns the provider error produced by `produce`.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    retry: Option<&HarnessRetryPolicy>,
    signal: Option<CancellationToken>,
    callbacks: Option<&SummaryRetryCallbacks>,
) -> Result<AssistantMessage, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<AssistantMessage, ProviderError>>,
{
    let policy = retry.copied();
    let max_retries = policy
        .filter(|policy| policy.enabled)
        .map_or(0, |policy| policy.max_retries);
    let base_delay_ms = policy.map_or(0, |policy| policy.base_delay_ms);
    let mut retry_attempt = 0_u64;
    let mut retried = false;
    loop {
        let response = produce().await?;

        if response.stop_reason == StopReason::Aborted {
            notify_retry_finished(callbacks, retried, false, retry_attempt, None).await;
            return Ok(response);
        }

        if response.stop_reason != StopReason::Error {
            notify_retry_finished(callbacks, retried, true, retry_attempt, None).await;
            return Ok(response);
        }

        let exhausted = retry_attempt >= max_retries;
        if exhausted || !is_retryable_assistant_error(&response) {
            notify_retry_finished(
                callbacks,
                retried,
                false,
                retry_attempt,
                response.error_message.clone(),
            )
            .await;
            return Ok(response);
        }

        retry_attempt = retry_attempt.saturating_add(1);
        retried = true;
        let shift = retry_attempt.saturating_sub(1).min(63) as u32;
        let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay_ms = base_delay_ms.saturating_mul(multiplier);
        let error_message = response.error_message.clone().unwrap_or_default();
        if let Some(callback) =
            callbacks.and_then(|callbacks| callbacks.on_retry_scheduled.as_ref())
        {
            callback(retry_attempt, max_retries, delay_ms, error_message.clone()).await;
        }

        let sleep = tokio::time::sleep(std::time::Duration::from_millis(delay_ms));
        if let Some(token) = signal.as_ref() {
            tokio::select! {
                () = token.cancelled() => {
                    notify_retry_finished(
                        callbacks,
                        retried,
                        false,
                        retry_attempt,
                        Some(error_message),
                    )
                    .await;
                    let mut aborted = response;
                    aborted.stop_reason = StopReason::Aborted;
                    aborted.error_message = None;
                    return Ok(aborted);
                }
                () = sleep => {}
            }
        } else {
            sleep.await;
        }

        if let Some(callback) =
            callbacks.and_then(|callbacks| callbacks.on_retry_attempt_start.as_ref())
        {
            callback().await;
        }
    }
}

fn thinking_level_string(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

/// File-operation details stored on generated compaction entries.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    /// Files read in the summarized span.
    pub read_files: Vec<String>,
    /// Files modified in the summarized span.
    pub modified_files: Vec<String>,
}

/// Adds provider usage fields and costs without dropping optional fields.
#[must_use]
pub fn add_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input: left.input.saturating_add(right.input),
        output: left.output.saturating_add(right.output),
        cache_read: left.cache_read.saturating_add(right.cache_read),
        cache_write: left.cache_write.saturating_add(right.cache_write),
        cache_write1h: add_optional_u64(left.cache_write1h, right.cache_write1h),
        reasoning: add_optional_u64(left.reasoning, right.reasoning),
        total_tokens: left.total_tokens.saturating_add(right.total_tokens),
        cost: pi_ai::UsageCost {
            input: left.cost.input + right.cost.input,
            output: left.cost.output + right.cost.output,
            cache_read: left.cost.cache_read + right.cost.cache_read,
            cache_write: left.cost.cache_write + right.cost.cache_write,
            total: left.cost.total + right.cost.total,
        },
    }
}

fn add_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantContent, AssistantMessage, TextContent};
    use serde_json::{Map, Value};

    fn assistant(text: &str, stop_reason: StopReason, usage: Usage) -> AgentMessage {
        let mut message = AssistantMessage::new("api", "provider", "model", 1);
        message.content = vec![AssistantContent::Text(TextContent::new(text))];
        message.stop_reason = stop_reason;
        message.usage = usage;
        AgentMessage::Llm(Box::new(Message::Assistant(Box::new(message))))
    }

    fn base(id: &str) -> EntryBase {
        EntryBase {
            id: EntryId::new(id),
            parent_id: None,
            seq: 0,
            timestamp: 1,
            custom_type: None,
        }
    }

    #[test]
    fn thresholds_and_estimates_are_deterministic() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 2,
        };
        assert!(!should_compact(90, 100, &settings));
        assert!(should_compact(91, 100, &settings));
        assert!(!should_compact(
            100,
            100,
            &CompactionSettings {
                enabled: false,
                ..settings
            }
        ));
        assert_eq!(
            estimate_tokens(&AgentMessage::Custom(
                crate::message::CustomAgentMessage::new(
                    "branchSummary",
                    Map::from_iter([(String::from("summary"), Value::from("1234"))])
                )
            )),
            1
        );
    }

    #[test]
    fn usage_anchor_estimates_trailing_tokens() {
        let usage = Usage {
            total_tokens: 20,
            ..Usage::default()
        };
        let messages = vec![
            assistant("old", StopReason::Stop, Usage::default()),
            assistant("anchor", StopReason::Stop, usage),
            assistant("tail", StopReason::Stop, Usage::default()),
        ];
        let result = estimate_context_tokens(&messages);
        assert_eq!(result.usage_tokens, 20);
        assert_eq!(result.last_usage_index, Some(1));
        assert!(result.trailing_tokens > 0);
    }

    #[test]
    fn cut_point_skips_tool_results_and_keeps_turn_integrity() {
        let user = AgentMessage::Llm(Box::new(Message::User(pi_ai::UserMessage::new(
            pi_ai::UserMessageContent::Text("u".to_owned()),
            1,
        ))));
        let tool = AgentMessage::Llm(Box::new(Message::ToolResult(
            pi_ai::ToolResultMessage::new(
                "c",
                "x",
                vec![pi_ai::ToolResultContent::Text(TextContent::new("r"))],
                false,
                1,
            ),
        )));
        let entries = vec![
            Entry::Message {
                base: base("u"),
                message: user,
                terminate: false,
            },
            Entry::Message {
                base: base("t"),
                message: tool,
                terminate: false,
            },
            Entry::Message {
                base: base("a"),
                message: assistant("a", StopReason::Stop, Usage::default()),
                terminate: false,
            },
        ];
        let points = find_valid_cut_points(&entries, 0, entries.len());
        assert_eq!(points, vec![0, 2]);
        let cut = find_cut_point(&entries, 0, entries.len(), 1);
        assert_eq!(cut.first_kept_entry_index, 2);
    }

    #[expect(
        clippy::expect_used,
        reason = "test fixture asserts round-trip variant via expect"
    )]
    #[test]
    fn durable_compaction_round_trip() {
        let preparation = CompactionPreparation {
            messages_to_summarize: Vec::new(),
            turn_prefix_messages: Vec::new(),
            retained_tail: Vec::new(),
            is_split_turn: false,
            tokens_before: 12,
            previous_summary: Some("old".to_owned()),
            file_ops: FileOperations::default(),
            settings: CompactionSettings::default(),
        };
        let durable = preparation.to_durable();
        let recovered = CompactionPreparation::from_durable(durable).expect("compaction variant");
        assert_eq!(recovered, preparation);
    }
}
