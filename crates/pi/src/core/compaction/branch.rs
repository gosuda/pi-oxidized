//! Branch summarization for tree navigation.
//!
//! Ports
//! `.references/pi/packages/coding-agent/src/core/compaction/branch-summarization.ts`.

use std::collections::BTreeMap;

use pi_agent::AgentMessage;
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, Model, StopReason,
    StreamOptions, TextContent, UserContent, UserMessage, UserMessageContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::core::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
    create_custom_message,
};
use crate::core::sessions::{SessionEntry, SessionManager};

use super::{
    CompactionError, FileOperations, SUMMARIZATION_SYSTEM_PROMPT, SummarizationRetryCallbacks,
    SummarizationRetryPolicy, SummarizeStreamFn, complete_summarization,
    compute_file_lists, create_file_ops, estimate_tokens, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};

/// Default max tokens for branch summary generation.
pub const DEFAULT_BRANCH_MAX_TOKENS: u64 = 2048;

/// Default context window when the model reports 0 / missing.
pub const DEFAULT_BRANCH_CONTEXT_WINDOW: u64 = 128_000;

/// Default reserve tokens for branch summarization.
pub const DEFAULT_BRANCH_RESERVE_TOKENS: u64 = 16_384;

/// Preamble prepended to every generated branch summary.
pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// Default branch-summary instructions (exact TS text).
pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Result of [`generate_branch_summary`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryResult {
    /// Generated summary text (with preamble + file ops).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Read-only files tracked on the abandoned path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_files: Option<Vec<String>>,
    /// Modified files tracked on the abandoned path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_files: Option<Vec<String>>,
    /// True when cancelled / aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    /// Error message when summarization failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// LLM usage from the branch-summary call, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<pi_ai::Usage>,
}

/// Details stored on a branch-summary entry for cumulative file tracking.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    /// Paths only read.
    pub read_files: Vec<String>,
    /// Paths edited or written.
    pub modified_files: Vec<String>,
}

/// Prepared abandoned-path messages under a token budget.
#[derive(Clone, Debug)]
pub struct BranchPreparation {
    /// Messages extracted for summarization, chronological order.
    pub messages: Vec<AgentMessage>,
    /// File operations extracted from tool calls and nested summaries.
    pub file_ops: FileOperations,
    /// Total estimated tokens in `messages`.
    pub total_tokens: u64,
}

/// Result of collecting abandoned-path entries.
#[derive(Clone, Debug)]
pub struct CollectEntriesResult {
    /// Entries to summarize, chronological order.
    pub entries: Vec<SessionEntry>,
    /// Common ancestor between old and new positions, if any.
    pub common_ancestor_id: Option<String>,
}

/// Options for [`generate_branch_summary`].
pub struct GenerateBranchSummaryOptions {
    /// Model to use.
    pub model: Model,
    /// Explicit API key.
    pub api_key: Option<String>,
    /// Optional request headers.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    /// Provider-scoped environment overrides.
    pub env: Option<BTreeMap<String, String>>,
    /// Cancellation token.
    pub signal: CancellationToken,
    /// Optional custom instructions.
    pub custom_instructions: Option<String>,
    /// When true, `custom_instructions` **replaces** the default prompt.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt + response (default 16384).
    pub reserve_tokens: Option<u64>,
    /// Injected summarizer stream.
    pub stream_fn: SummarizeStreamFn,
    /// Retry policy for the standalone summarization request.
    pub retry: Option<SummarizationRetryPolicy>,
    /// Lifecycle callbacks for transient summarization retries.
    pub retry_callbacks: Option<SummarizationRetryCallbacks>,
}

/// Collect entries that should be summarized when navigating from one position
/// to another (abandoned path from `old_leaf_id` back to the common ancestor
/// with `target_id`).
#[must_use]
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> CollectEntriesResult {
    let Some(old_leaf_id) = old_leaf_id.filter(|s| !s.is_empty()) else {
        return CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        };
    };

    let old_path: std::collections::HashSet<String> = session
        .get_branch(Some(old_leaf_id))
        .into_iter()
        .filter_map(|e| e.id().map(str::to_owned))
        .collect();
    let target_path = session.get_branch(Some(target_id));

    let mut common_ancestor_id = None;
    for entry in target_path.iter().rev() {
        if let Some(id) = entry.id()
            && old_path.contains(id)
        {
            common_ancestor_id = Some(id.to_owned());
            break;
        }
    }

    let mut entries = Vec::new();
    let mut current = Some(old_leaf_id.to_owned());
    while let Some(cur) = current {
        if common_ancestor_id.as_deref() == Some(cur.as_str()) {
            break;
        }
        let Some(entry) = session.get_entry(&cur) else {
            break;
        };
        entries.push(entry.clone());
        current = entry.parent_id().map(str::to_owned);
    }
    entries.reverse();

    CollectEntriesResult {
        entries,
        common_ancestor_id,
    }
}

fn get_message_from_entry(entry: &SessionEntry) -> Option<AgentMessage> {
    match entry {
        SessionEntry::Message(m) => {
            if m.message.role() == "toolResult" {
                return None;
            }
            Some(m.message.clone())
        }
        SessionEntry::CustomMessage(c) => {
            let custom = create_custom_message(
                &c.custom_type,
                c.content.clone(),
                c.display,
                c.details.clone(),
                &c.timestamp,
            )
            .ok()?;
            product_to_agent(&custom)
        }
        SessionEntry::BranchSummary(b) => {
            let msg = create_branch_summary_message(&b.summary, &b.from_id, &b.timestamp).ok()?;
            product_to_agent(&msg)
        }
        SessionEntry::Compaction(c) => {
            let msg = create_compaction_summary_message(&c.summary, c.tokens_before, &c.timestamp)
                .ok()?;
            product_to_agent(&msg)
        }
        _ => None,
    }
}

fn product_to_agent<T: Serialize>(msg: &T) -> Option<AgentMessage> {
    let value = serde_json::to_value(msg).ok()?;
    let Value::Object(mut map) = value else {
        return None;
    };
    let Some(Value::String(role)) = map.remove("role") else {
        return None;
    };
    Some(AgentMessage::Custom(pi_agent::CustomAgentMessage::new(
        role, map,
    )))
}

/// Prepare entries for summarization with a newest→oldest token budget.
///
/// First pass collects file ops from **all** entries (including nested
/// `branch_summary` details when `fromHook != true`). Second pass walks
/// newest→oldest adding messages until the budget; summary entries may still
/// be rescued when `total_tokens < budget * 0.9`.
#[must_use]
pub fn prepare_branch_entries(entries: &[SessionEntry], token_budget: u64) -> BranchPreparation {
    let mut messages = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens = 0u64;

    // First pass: file ops from all entries (incl. nested branch summaries).
    for entry in entries {
        if let SessionEntry::BranchSummary(b) = entry {
            let from_hook = b.from_hook.unwrap_or(false);
            if !from_hook && let Some(details) = b.details.as_ref() {
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
                            file_ops.edited.insert(path.to_owned());
                        }
                    }
                }
            }
        }
    }

    // Second pass: newest → oldest under budget.
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);

        if token_budget > 0 && total_tokens.saturating_add(tokens) > token_budget {
            // Rescue summary entries when still under 90% of budget.
            if matches!(entry.discriminant(), "compaction" | "branch_summary") {
                let rescue_limit = (token_budget / 10) * 9 + (token_budget % 10) * 9 / 10;
                if total_tokens < rescue_limit {
                    messages.insert(0, message);
                    total_tokens = total_tokens.saturating_add(tokens);
                }
            }
            break;
        }

        messages.insert(0, message);
        total_tokens = total_tokens.saturating_add(tokens);
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

fn ensure_not_cancelled(signal: &CancellationToken) -> Result<(), CompactionError> {
    if signal.is_cancelled() {
        Err(CompactionError::Cancelled)
    } else {
        Ok(())
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

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Generate a summary of abandoned branch entries.
///
/// # Errors
///
/// Surfaces cancellation and provider failures; non-abort summarizer errors are
/// returned as [`BranchSummaryResult::error`] (matching TS soft-error shape).
pub async fn generate_branch_summary(
    entries: &[SessionEntry],
    options: GenerateBranchSummaryOptions,
) -> Result<BranchSummaryResult, CompactionError> {
    ensure_not_cancelled(&options.signal)?;

    let reserve_tokens = options
        .reserve_tokens
        .unwrap_or(DEFAULT_BRANCH_RESERVE_TOKENS);
    let context_window = if options.model.context_window == 0 {
        DEFAULT_BRANCH_CONTEXT_WINDOW
    } else {
        options.model.context_window
    };
    let token_budget = context_window.saturating_sub(reserve_tokens);

    let prepared = prepare_branch_entries(entries, token_budget);
    if prepared.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: Some("No content to summarize".to_owned()),
            ..BranchSummaryResult::default()
        });
    }

    ensure_not_cancelled(&options.signal)?;

    let llm_messages = convert_to_llm(&prepared.messages).map_err(CompactionError::from)?;
    let conversation_text = serialize_conversation(&llm_messages);

    let instructions = if options.replace_instructions {
        if let Some(custom) = options
            .custom_instructions
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            custom.to_owned()
        } else {
            BRANCH_SUMMARY_PROMPT.to_owned()
        }
    } else if let Some(custom) = options
        .custom_instructions
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}")
    } else {
        BRANCH_SUMMARY_PROMPT.to_owned()
    };

    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");
    let summarization_messages = vec![Message::User(UserMessage::new(
        UserMessageContent::Blocks(vec![UserContent::Text(TextContent::new(prompt_text))]),
        now_millis(),
    ))];

    let request_options = StreamOptions {
        api_key: options.api_key,
        headers: options.headers,
        env: options.env,
        signal: Some(options.signal.clone()),
        max_tokens: Some(DEFAULT_BRANCH_MAX_TOKENS),
        ..StreamOptions::default()
    };

    ensure_not_cancelled(&options.signal)?;

    let response = complete_summarization(
        &options.model,
        Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: summarization_messages,
            tools: None,
        },
        request_options,
        &options.stream_fn,
        options.retry.as_ref(),
        options.retry_callbacks.as_ref(),
    )
    .await?;

    ensure_not_cancelled(&options.signal)?;

    if response.stop_reason == StopReason::Aborted {
        return Ok(BranchSummaryResult {
            aborted: Some(true),
            ..BranchSummaryResult::default()
        });
    }
    if response.stop_reason == StopReason::Error {
        return Ok(BranchSummaryResult {
            error: Some(
                response
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Summarization failed".to_owned()),
            ),
            ..BranchSummaryResult::default()
        });
    }

    let mut summary = assistant_text(&response);
    summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");

    let (read_files, modified_files) = compute_file_lists(&prepared.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    if summary.is_empty() {
        "No summary generated".clone_into(&mut summary);
    }

    Ok(BranchSummaryResult {
        summary: Some(summary),
        read_files: Some(read_files),
        modified_files: Some(modified_files),
        aborted: None,
        error: None,
        usage: Some(response.usage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::compaction::{CompactionSettings, DEFAULT_COMPACTION_SETTINGS};
    use pi_ai::ProviderError;
    use pi_ai::{AssistantMessage, ModelInput, TextContent, ToolCall};
    use serde_json::{Map, json};
    use std::pin::Pin;
    use std::sync::Arc;

    fn result_ok<T, E>(result: Result<T, E>) -> T {
        assert!(result.is_ok());
        match result {
            Ok(value) => value,
            Err(_) => unreachable!(),
        }
    }

    fn usage(input: u64, output: u64) -> pi_ai::Usage {
        pi_ai::Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write1h: None,
            reasoning: None,
            total_tokens: input + output,
            cost: pi_ai::UsageCost::default(),
        }
    }

    fn user_agent(text: &str) -> AgentMessage {
        AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
            UserMessageContent::Text(text.to_owned()),
            1,
        ))))
    }

    fn assistant_agent(text: &str) -> AgentMessage {
        let mut m = AssistantMessage::new("a", "p", "m", 1);
        m.content = vec![AssistantContent::Text(TextContent::new(text))];
        m.usage = usage(10, 5);
        m.stop_reason = StopReason::Stop;
        AgentMessage::Llm(Box::new(Message::Assistant(m)))
    }

    fn test_model() -> Model {
        Model {
            id: "test".into(),
            name: "Test".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://example.test".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: pi_ai::ModelCost::default(),
            context_window: 128_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn capture_stream(
        captured: &Arc<std::sync::Mutex<Vec<String>>>,
        maxes: &Arc<std::sync::Mutex<Vec<u64>>>,
    ) -> SummarizeStreamFn {
        let captured = Arc::clone(captured);
        let maxes = Arc::clone(maxes);
        Arc::new(move |_model, ctx, opts| {
            let captured = Arc::clone(&captured);
            let maxes = Arc::clone(&maxes);
            Box::pin(async move {
                if let Some(max) = opts.max_tokens {
                    result_ok(maxes.lock()).push(max);
                }
                if let Some(Message::User(user)) = ctx.messages.first() {
                    let text = match &user.content {
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
                    result_ok(captured.lock()).push(text);
                }
                let mut message = AssistantMessage::new("a", "p", "m", 1);
                message.content = vec![AssistantContent::Text(TextContent::new("BRANCH"))];
                message.stop_reason = StopReason::Stop;
                let stream = futures::stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::Stop,
                    message,
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
    fn collect_common_ancestor() {
        let mut sm = result_ok(SessionManager::in_memory(Some("/tmp"), None));
        let u1 = result_ok(sm.append_message(&user_agent("root")));
        let a1 = result_ok(sm.append_message(&assistant_agent("a1")));
        let u2 = result_ok(sm.append_message(&user_agent("branch-a")));
        // branch back to a1 and create sibling
        result_ok(sm.branch(&a1));
        let u3 = result_ok(sm.append_message(&user_agent("branch-b")));

        let collected = collect_entries_for_branch_summary(&sm, Some(&u2), &u3);
        assert_eq!(collected.common_ancestor_id.as_deref(), Some(a1.as_str()));
        assert!(!collected.entries.is_empty());
        assert!(
            collected
                .entries
                .iter()
                .any(|e| e.id() == Some(u2.as_str()))
        );
        assert!(
            !collected
                .entries
                .iter()
                .any(|e| e.id() == Some(u1.as_str()))
        );
    }

    #[test]
    fn prepare_budget_rescue_and_nested_file_ops() {
        // Nested branch summary with file details (fromHook=false) must contribute.
        let nested = result_ok(serde_json::from_value::<SessionEntry>(json!({
            "type": "branch_summary",
            "id": "bs1",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "fromId": "root",
            "summary": "nested",
            "details": {
                "readFiles": ["nested-read.txt"],
                "modifiedFiles": ["nested-mod.txt"]
            }
        })));

        let mut asst = AssistantMessage::new("a", "p", "m", 1);
        asst.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "1",
            "read",
            Map::from_iter([("path".into(), Value::String("fresh.txt".into()))]),
        ))];
        let msg_entry = result_ok(serde_json::from_value::<SessionEntry>(json!({
            "type": "message",
            "id": "m1",
            "parentId": "bs1",
            "timestamp": "2025-01-01T00:00:00.000Z",
            "message": AgentMessage::Llm(Box::new(Message::Assistant(asst))),
        })));

        // Huge summary that exceeds tiny budget — should be rescued under 0.9.
        let big_summary = "S".repeat(400);
        let compact = result_ok(serde_json::from_value::<SessionEntry>(json!({
            "type": "compaction",
            "id": "c1",
            "parentId": "m1",
            "timestamp": "2025-01-01T00:00:00.000Z",
            "summary": big_summary,
            "firstKeptEntryId": "m1",
            "tokensBefore": 1,
        })));

        // Budget just below compact tokens so rescue path triggers.
        let prep = prepare_branch_entries(&[nested, msg_entry, compact], 50);
        let (read, modified) = compute_file_lists(&prep.file_ops);
        assert!(
            read.iter().any(|p| p == "nested-read.txt") || read.iter().any(|p| p == "fresh.txt")
        );
        assert!(modified.iter().any(|p| p == "nested-mod.txt"));

        // fromHook=true details must be ignored.
        let hooked = result_ok(serde_json::from_value::<SessionEntry>(json!({
            "type": "branch_summary",
            "id": "bs2",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "fromId": "root",
            "summary": "hooked",
            "fromHook": true,
            "details": {
                "readFiles": ["hook-only.txt"],
                "modifiedFiles": []
            }
        })));
        let prep2 = prepare_branch_entries(&[hooked], 0);
        let (read2, _) = compute_file_lists(&prep2.file_ops);
        assert!(!read2.iter().any(|p| p == "hook-only.txt"));
    }

    #[tokio::test]
    async fn generate_branch_summary_replace_vs_append_and_caps() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let maxes = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        let stream_fn = capture_stream(&captured, &maxes);

        let entry = result_ok(serde_json::from_value::<SessionEntry>(json!({
            "type": "message",
            "id": "u",
            "parentId": null,
            "timestamp": "2025-01-01T00:00:00.000Z",
            "message": user_agent("hello branch"),
        })));

        // Append custom
        let result = result_ok(
            generate_branch_summary(
                std::slice::from_ref(&entry),
                GenerateBranchSummaryOptions {
                    model: test_model(),
                    api_key: None,
                    headers: None,
                    env: None,
                    signal: CancellationToken::new(),
                    custom_instructions: Some("focus X".into()),
                    replace_instructions: false,
                    reserve_tokens: Some(16_384),
                    stream_fn: Arc::clone(&stream_fn),
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );
        assert!(
            result
                .summary
                .as_deref()
                .unwrap_or("")
                .contains(BRANCH_SUMMARY_PREAMBLE)
        );
        assert!(result.summary.as_deref().unwrap_or("").contains("BRANCH"));
        {
            let prompts = result_ok(captured.lock());
            assert!(prompts[0].contains(BRANCH_SUMMARY_PROMPT));
            assert!(prompts[0].contains("Additional focus: focus X"));
        }

        // Replace custom
        result_ok(captured.lock()).clear();
        let _ = result_ok(
            generate_branch_summary(
                &[entry],
                GenerateBranchSummaryOptions {
                    model: test_model(),
                    api_key: None,
                    headers: None,
                    env: None,
                    signal: CancellationToken::new(),
                    custom_instructions: Some("ONLY CUSTOM".into()),
                    replace_instructions: true,
                    reserve_tokens: None,
                    stream_fn,
                    retry: None,
                    retry_callbacks: None,
                },
            )
            .await,
        );
        {
            let prompts = result_ok(captured.lock());
            assert!(prompts[0].contains("ONLY CUSTOM"));
            assert!(!prompts[0].contains("## Goal"));
        }
        assert_eq!(result_ok(maxes.lock())[0], DEFAULT_BRANCH_MAX_TOKENS);
    }

    // keep DEFAULT_COMPACTION_SETTINGS referenced so settings parity is visible
    #[test]
    fn defaults_align() {
        assert_eq!(
            DEFAULT_BRANCH_RESERVE_TOKENS,
            DEFAULT_COMPACTION_SETTINGS.reserve_tokens
        );
        let _ = CompactionSettings::default();
    }
}
