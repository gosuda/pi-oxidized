//! Branch navigation summaries.
//!
//! Ported from `.references/pi/packages/agent/src/harness/compaction/branch-summarization.ts`.

use std::collections::HashSet;
use std::sync::Arc;

use pi_ai::{Context as AiContext, Model, StreamOptions};
use serde_json::Value;

use crate::context::Context;
use crate::message::AgentMessage;
use crate::session::{
    Branch, BranchScan, DurableStructuralPreparation, Entry, EntryCursor, EntryId,
    HarnessRetryPolicy, LIST_READ_MAX_LIMIT, ScanOrder, Session, SessionError,
};

use super::messages::{
    create_branch_summary_message, create_compaction_summary_message, summary_user_message,
};
use super::utils::{
    FileLists, FileOperations, compute_file_lists, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use super::{
    SUMMARIZATION_SYSTEM_PROMPT, SummaryRequest, SummaryRetryCallbacks,
    complete_simple_with_retries, create_summary_request_options, estimate_tokens,
};
use crate::harness::api::HarnessModels;
use crate::harness::hooks::BranchSummaryResult;

/// Stable branch-summary error categories.
#[derive(Debug, thiserror::Error)]
pub enum BranchSummaryError {
    /// The summary request was cancelled.
    #[error("branch summary aborted: {0}")]
    Aborted(String),
    /// The provider returned a terminal semantic error.
    #[error("branch summarization failed: {0}")]
    SummarizationFailed(String),
    /// The request boundary failed before a terminal assistant response.
    #[error("branch summary transport failed: {0}")]
    Transport(#[from] pi_ai::ProviderError),
    /// Session ancestry lookup failed.
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl BranchSummaryError {
    /// Stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Aborted(_) => "aborted",
            Self::SummarizationFailed(_) => "summarization_failed",
            Self::Transport(_) => "transport",
            Self::Session(_) => "session",
        }
    }
}

/// Entries collected from the abandoned path before navigation.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectEntriesResult {
    /// Chronological entries to summarize.
    pub entries: Vec<Entry>,
    /// Deepest common ancestor of old and target paths.
    pub common_ancestor_id: Option<EntryId>,
}

/// Prepared branch content within an optional token budget.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchPreparation {
    /// Messages selected for the branch summary.
    pub messages: Vec<AgentMessage>,
    /// File operations observed in the selected span.
    pub file_ops: FileOperations,
    /// Estimated token count of selected messages.
    pub total_tokens: u64,
}

impl BranchPreparation {
    /// Converts live sets into the canonical durable branch-summary variant.
    #[must_use]
    pub fn to_durable(&self) -> DurableStructuralPreparation {
        DurableStructuralPreparation::BranchSummary {
            messages: self.messages.clone(),
            file_ops: self.file_ops.to_durable(),
            total_tokens: self.total_tokens,
        }
    }

    /// Rehydrates a branch preparation; returns `None` for compaction values.
    #[must_use]
    pub fn from_durable(value: DurableStructuralPreparation) -> Option<Self> {
        let DurableStructuralPreparation::BranchSummary {
            messages,
            file_ops,
            total_tokens,
        } = value
        else {
            return None;
        };
        Some(Self {
            messages,
            file_ops: FileOperations::from_durable(file_ops),
            total_tokens,
        })
    }
}

/// Provider/model options for a branch summary.
#[derive(Clone)]
pub struct GenerateBranchSummaryOptions {
    /// Provider-backed model collection.
    pub models: Arc<dyn HarnessModels>,
    /// Model used for summarization.
    pub model: Model,
    /// Optional additional prompt instructions.
    pub custom_instructions: Option<String>,
    /// Replace rather than append to the default branch prompt.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt and output.
    pub reserve_tokens: Option<u64>,
    /// Optional transient retry policy.
    pub retry: Option<HarnessRetryPolicy>,
    /// Optional retry callbacks.
    pub callbacks: Option<SummaryRetryCallbacks>,
}

/// Options for an already-prepared branch summary request.
#[derive(Clone, Debug, Default)]
pub struct PreparedBranchSummaryOptions {
    /// Optional additional prompt instructions.
    pub custom_instructions: Option<String>,
    /// Replace rather than append to the default branch prompt.
    pub replace_instructions: bool,
}

const DEFAULT_BRANCH_RESERVE_TOKENS: u64 = 16_384;
const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";
const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Collects entries on the old path up to its common ancestor with `target_id`.
///
/// # Errors
///
/// Returns `Err` when the branch ancestry lookup or session reads fail, or
/// when an expected entry is missing from the session.
pub async fn collect_entries_for_branch_summary(
    branch: &dyn Branch,
    session: &dyn Session,
    old_tip_id: Option<&EntryId>,
    target_id: &EntryId,
    cx: &Context,
) -> Result<CollectEntriesResult, SessionError> {
    let Some(old_tip_id) = old_tip_id else {
        return Ok(CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        });
    };
    // Both ancestry scans page until the common ancestor is found or the root
    // is reached; relying on one backend read window would silently miss an
    // ancestor deeper than the window and walk the abandoned path to root.
    let mut old_path = HashSet::new();
    scan_branch_ancestry(branch, old_tip_id, cx, |page| {
        old_path.extend(page.iter().map(|entry| entry.id().clone()));
        true
    })
    .await?;
    let mut common_ancestor_id = None;
    scan_branch_ancestry(branch, target_id, cx, |page| {
        if let Some(entry) = page.iter().find(|entry| old_path.contains(entry.id())) {
            common_ancestor_id = Some(entry.id().clone());
            return false;
        }
        true
    })
    .await?;

    let mut entries = Vec::new();
    let mut current = Some(old_tip_id.clone());
    while let Some(id) = current {
        if common_ancestor_id.as_ref() == Some(&id) {
            break;
        }
        let entry = session.get_entry(&id, cx).await?.ok_or_else(|| {
            SessionError::Invariant(format!("Corrupt session: entry {id} not found"))
        })?;
        current = entry.parent_id().cloned();
        entries.push(entry);
    }
    entries.reverse();
    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

/// Pages through the ancestry from `start` (newest first) so the walk is not
/// bounded by the backend's read window.
///
/// `visit` receives each non-empty page and returns whether to keep scanning;
/// the walk also stops once the root entry is reached or the backend stops
/// advancing the cursor.
async fn scan_branch_ancestry(
    branch: &dyn Branch,
    start: &EntryId,
    cx: &Context,
    mut visit: impl FnMut(&[Entry]) -> bool,
) -> Result<(), SessionError> {
    let mut cursor = None;
    loop {
        let page = branch
            .find_entries(
                Some(&BranchScan {
                    start: Some(start.clone()),
                    order: Some(ScanOrder::Desc),
                    limit: Some(LIST_READ_MAX_LIMIT),
                    cursor,
                    ..BranchScan::default()
                }),
                cx,
            )
            .await?;
        let Some(last) = page.last() else {
            break;
        };
        let next = EntryCursor { seq: last.seq() };
        let keep_going = visit(&page)
            && last.parent_id().is_some()
            && cursor.is_none_or(|previous: EntryCursor| next.seq < previous.seq);
        if !keep_going {
            break;
        }
        cursor = Some(next);
    }
    Ok(())
}

/// Selects branch entries in chronological order within `token_budget`.
#[must_use]
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    // Selection runs newest-first; file operations are collected afterwards
    // from the selected entries only, so the metadata never covers messages or
    // inherited branch-summary details outside the prepared range.
    let mut selected_reverse = Vec::new();
    let mut total_tokens = 0_u64;
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens.saturating_add(tokens) > token_budget {
            if matches!(
                entry,
                Entry::Compaction { .. } | Entry::BranchSummary { .. }
            ) && total_tokens.saturating_mul(10) < token_budget.saturating_mul(9)
            {
                total_tokens = total_tokens.saturating_add(tokens);
                selected_reverse.push((entry, message));
            }
            break;
        }
        total_tokens = total_tokens.saturating_add(tokens);
        selected_reverse.push((entry, message));
    }

    let mut file_ops = FileOperations::default();
    let mut messages = Vec::with_capacity(selected_reverse.len());
    for (entry, message) in selected_reverse.into_iter().rev() {
        if let Entry::BranchSummary {
            details: Some(details),
            ..
        } = entry
            && let Some(object) = details.as_object()
        {
            if let Some(paths) = object.get("readFiles").and_then(Value::as_array) {
                file_ops
                    .read
                    .extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            if let Some(paths) = object.get("modifiedFiles").and_then(Value::as_array) {
                file_ops
                    .edited
                    .extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
        extract_file_ops_from_message(&message, &mut file_ops);
        messages.push(message);
    }
    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message { message, .. } if message.role() != "toolResult" => Some(message.clone()),
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
        Entry::Message { .. } | Entry::Custom { .. } => None,
    }
}

/// Generates a branch summary through the configured provider stream.
///
/// # Errors
///
/// Returns [`BranchSummaryError::Aborted`] if the provider cancels the request,
/// [`BranchSummaryError::Transport`] if the request boundary fails, or
/// [`BranchSummaryError::SummarizationFailed`] if the provider returns a
/// terminal error response.
pub async fn generate_branch_summary(
    entries: &[Entry],
    options: &GenerateBranchSummaryOptions,
    cx: &Context,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let context_window = if options.model.context_window == 0 {
        128_000
    } else {
        options.model.context_window
    };
    let reserve_tokens = options
        .reserve_tokens
        .unwrap_or(DEFAULT_BRANCH_RESERVE_TOKENS);
    let preparation =
        prepare_branch_entries(entries, context_window.saturating_sub(reserve_tokens));
    let models = Arc::clone(&options.models);
    let model = options.model.clone();
    let retry = options.retry;
    let callbacks = options.callbacks.clone();
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
    generate_branch_summary_with_request(
        &preparation,
        &PreparedBranchSummaryOptions {
            custom_instructions: options.custom_instructions.clone(),
            replace_instructions: options.replace_instructions,
        },
        &request,
        cx,
    )
    .await
}

/// Generates a prepared branch summary through one caller-owned request.
///
/// # Errors
///
/// Returns [`BranchSummaryError::Aborted`] if the request reports a
/// cancellation, [`BranchSummaryError::Transport`] if the request boundary
/// fails, or [`BranchSummaryError::SummarizationFailed`] if the provider
/// returns a terminal error response.
pub async fn generate_branch_summary_with_request(
    preparation: &BranchPreparation,
    options: &PreparedBranchSummaryOptions,
    request: &SummaryRequest,
    cx: &Context,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_owned(),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }
    let conversation_text =
        serialize_conversation(&super::messages::convert_to_llm(&preparation.messages));
    let instructions = match (
        options.replace_instructions,
        options
            .custom_instructions
            .as_deref()
            .filter(|custom| !custom.is_empty()),
    ) {
        (true, Some(custom)) => custom.to_owned(),
        (_, Some(custom)) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        _ => BRANCH_SUMMARY_PROMPT.to_owned(),
    };
    let prompt = format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");
    let response = request(
        AiContext {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_owned()),
            messages: vec![summary_user_message(&prompt)],
            tools: None,
        },
        create_summary_request_options(
            &StreamOptions {
                max_tokens: Some(2_048),
                ..StreamOptions::default()
            },
            cx,
        ),
        cx.clone(),
    )
    .await
    .map_err(|error| {
        let text = error.to_string();
        if text.to_ascii_lowercase().contains("cancel")
            || text.to_ascii_lowercase().contains("abort")
        {
            BranchSummaryError::Aborted(text)
        } else {
            BranchSummaryError::Transport(error)
        }
    })?;
    match response.stop_reason {
        pi_ai::StopReason::Aborted => {
            return Err(BranchSummaryError::Aborted(
                response
                    .error_message
                    .unwrap_or_else(|| "Branch summary aborted".to_owned()),
            ));
        }
        pi_ai::StopReason::Error => {
            return Err(BranchSummaryError::SummarizationFailed(format!(
                "Branch summary failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_owned())
            )));
        }
        _ => {}
    }
    let text = super::utils::assistant_content_text(&response.content, "\n");
    let FileLists {
        read_files,
        modified_files,
    } = compute_file_lists(&preparation.file_ops);
    let summary = format!(
        "{BRANCH_SUMMARY_PREAMBLE}{text}{}",
        format_file_operations(&read_files, &modified_files)
    );
    Ok(BranchSummaryResult {
        summary: if summary.is_empty() {
            "No summary generated".to_owned()
        } else {
            summary
        },
        usage: Some(response.usage),
        read_files,
        modified_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use pi_ai::{Message, TextContent, UserMessage, UserMessageContent};

    use crate::session::{
        LIST_READ_DEFAULT_LIMIT, LaneName, MemoryStorage, SessionMetadata, StorageBackedSession,
        UuidV7Generator,
    };

    fn base(id: &str) -> crate::session::EntryBase {
        crate::session::EntryBase {
            id: EntryId::new(id),
            parent_id: None,
            seq: 0,
            timestamp: 1,
            custom_type: None,
        }
    }

    #[test]
    fn empty_preparation_is_typed_absence_at_generation_boundary() {
        let prep = BranchPreparation {
            messages: Vec::new(),
            file_ops: FileOperations::default(),
            total_tokens: 0,
        };
        assert!(prep.messages.is_empty());
    }

    #[test]
    fn durable_branch_round_trip() {
        let prep = BranchPreparation {
            messages: Vec::new(),
            file_ops: FileOperations::default(),
            total_tokens: 5,
        };
        let durable = prep.to_durable();
        assert_eq!(BranchPreparation::from_durable(durable), Some(prep));
    }

    #[test]
    fn preparation_skips_tool_results() {
        let tool = AgentMessage::Llm(Box::new(Message::ToolResult(
            pi_ai::ToolResultMessage::new(
                "call",
                "tool",
                vec![pi_ai::ToolResultContent::Text(TextContent::new("r"))],
                false,
                1,
            ),
        )));
        let prep = prepare_branch_entries(
            &[Entry::Message {
                base: base("tool"),
                message: tool,
                terminate: false,
            }],
            0,
        );
        assert!(prep.messages.is_empty());
    }

    /// A branch handle that clamps every scan to `page` entries, simulating a
    /// backend whose read window is smaller than the ancestry depth.
    struct SmallPageBranch {
        inner: Arc<dyn Branch>,
        page: u32,
    }

    impl Branch for SmallPageBranch {
        fn name(&self) -> &LaneName {
            self.inner.name()
        }

        fn get_tip_id<'a>(
            &'a self,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Option<EntryId>, SessionError>> {
            self.inner.get_tip_id(cx)
        }

        fn find_entries<'a>(
            &'a self,
            q: Option<&'a BranchScan>,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
            Box::pin(async move {
                let mut q = q.cloned().unwrap_or_default();
                q.limit = Some(q.limit.unwrap_or(LIST_READ_DEFAULT_LIMIT).min(self.page));
                self.inner.find_entries(Some(&q), cx).await
            })
        }

        fn find_entry<'a>(
            &'a self,
            q: Option<&'a BranchScan>,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<Option<Entry>, SessionError>> {
            self.inner.find_entry(q, cx)
        }

        fn append_message<'a>(
            &'a self,
            message: AgentMessage,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<EntryId, SessionError>> {
            self.inner.append_message(message, cx)
        }

        fn append_custom_entry<'a>(
            &'a self,
            custom_type: &'a str,
            data: Option<Value>,
            cx: &'a Context,
        ) -> BoxFuture<'a, Result<EntryId, SessionError>> {
            self.inner.append_custom_entry(custom_type, data, cx)
        }
    }

    #[test]
    fn excluded_entries_do_not_contribute_file_operations() {
        // A branch summary priced out of the budget must not leak its recorded
        // file lists into the preparation metadata.
        let excluded = Entry::BranchSummary {
            base: base("excluded-summary"),
            from_id: None,
            summary: "s".repeat(400),
            details: Some(serde_json::json!({
                "readFiles": ["excluded-read.txt"],
                "modifiedFiles": ["excluded-mod.txt"],
            })),
            usage: None,
            from_hook: false,
        };
        let kept_summary = Entry::BranchSummary {
            base: base("kept-summary"),
            from_id: None,
            summary: "s".repeat(40),
            details: Some(serde_json::json!({
                "readFiles": ["kept-read.txt"],
                "modifiedFiles": [],
            })),
            usage: None,
            from_hook: false,
        };
        let kept_message = Entry::Message {
            base: base("kept-message"),
            message: AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
                UserMessageContent::Text("u".repeat(400)),
                1,
            )))),
            terminate: false,
        };
        // Newest-first selection keeps the 100-token message and the 10-token
        // summary inside a 110-token budget; the oldest 100-token summary
        // overflows it past the 90% rescue threshold and is dropped.
        let prep = prepare_branch_entries(&[excluded, kept_summary, kept_message], 110);
        assert_eq!(prep.messages.len(), 2);
        assert!(prep.file_ops.read.contains("kept-read.txt"));
        assert!(!prep.file_ops.read.contains("excluded-read.txt"));
        assert!(!prep.file_ops.edited.contains("excluded-mod.txt"));
    }

    #[tokio::test]
    async fn common_ancestor_beyond_one_scan_window_is_found() -> Result<(), SessionError> {
        // With a backend window smaller than the distance to the common
        // ancestor, collection must keep paging instead of walking the
        // abandoned path to the root.
        let cx = Context::background();
        let session = StorageBackedSession::new(
            SessionMetadata {
                id: "branch-ancestry-pagination".to_owned(),
                created_at: 1,
                storage_version: MemoryStorage::STORAGE_VERSION,
                cwd: None,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            Arc::new(MemoryStorage::new()),
            Arc::new(UuidV7Generator::new()),
            None,
        );
        let main = session
            .create_branch(&LaneName::from("main"), None, &cx)
            .await?;
        let message = || {
            AgentMessage::Llm(Box::new(Message::User(UserMessage::new(
                UserMessageContent::Text("hi".to_owned()),
                1,
            ))))
        };
        let mut shared = Vec::new();
        for _ in 0..4 {
            shared.push(main.append_message(message(), &cx).await?);
        }
        let old_tip = main.append_message(message(), &cx).await?;
        let ancestor = shared[2].clone();
        let other = session
            .create_branch(&LaneName::from("other"), Some(&ancestor), &cx)
            .await?;
        let mut target = ancestor.clone();
        for _ in 0..2 {
            target = other.append_message(message(), &cx).await?;
        }

        let paged = SmallPageBranch {
            inner: main,
            page: 2,
        };
        let collected = collect_entries_for_branch_summary(
            &paged,
            session.as_ref(),
            Some(&old_tip),
            &target,
            &cx,
        )
        .await?;
        assert_eq!(collected.common_ancestor_id.as_ref(), Some(&ancestor));
        let ids: Vec<EntryId> = collected
            .entries
            .iter()
            .map(|entry| entry.id().clone())
            .collect();
        assert_eq!(ids, [shared[3].clone(), old_tip]);
        Ok(())
    }
}
