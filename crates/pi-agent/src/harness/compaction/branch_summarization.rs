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
    Branch, BranchScan, DurableStructuralPreparation, Entry, EntryId, HarnessRetryPolicy,
    ScanOrder, Session, SessionError,
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
    let old_path = branch
        .find_entries(
            Some(&BranchScan {
                start: Some(old_tip_id.clone()),
                order: Some(ScanOrder::Desc),
                ..BranchScan::default()
            }),
            cx,
        )
        .await?
        .into_iter()
        .map(|entry| entry.id().clone())
        .collect::<HashSet<_>>();
    let target_path = branch
        .find_entries(
            Some(&BranchScan {
                start: Some(target_id.clone()),
                order: Some(ScanOrder::Desc),
                ..BranchScan::default()
            }),
            cx,
        )
        .await?;
    let common_ancestor_id = target_path
        .iter()
        .find(|entry| old_path.contains(entry.id()))
        .map(|entry| entry.id().clone());

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

/// Selects branch entries in chronological order within `token_budget`.
#[must_use]
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    let mut file_ops = FileOperations::default();
    for entry in entries {
        let Entry::BranchSummary {
            details: Some(details),
            ..
        } = entry
        else {
            continue;
        };
        let Some(object) = details.as_object() else {
            continue;
        };
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

    let mut selected_reverse = Vec::new();
    let mut total_tokens = 0_u64;
    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens.saturating_add(tokens) > token_budget {
            if matches!(
                entry,
                Entry::Compaction { .. } | Entry::BranchSummary { .. }
            ) && total_tokens.saturating_mul(10) < token_budget.saturating_mul(9)
            {
                selected_reverse.push(message);
                total_tokens = total_tokens.saturating_add(tokens);
            }
            break;
        }
        selected_reverse.push(message);
        total_tokens = total_tokens.saturating_add(tokens);
    }
    selected_reverse.reverse();
    BranchPreparation {
        messages: selected_reverse,
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
    use pi_ai::{Message, TextContent};

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
}
