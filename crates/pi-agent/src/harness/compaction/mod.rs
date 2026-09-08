//! Harness compaction and branch-summary algorithms.
//!
//! This module owns structural preparation, prompt construction, and the
//! provider-request boundary. Runtime code owns admission/persistence; storage
//! owns durable values and entries.

pub mod branch_summarization;
mod core;
pub mod context;
pub mod messages;
pub mod utils;

pub use branch_summarization::{
    BranchPreparation, BranchSummaryError, CollectEntriesResult,
    GenerateBranchSummaryOptions, PreparedBranchSummaryOptions,
    collect_entries_for_branch_summary, generate_branch_summary,
    generate_branch_summary_with_request, prepare_branch_entries,
};
pub use core::{
    CompactionDetails, CompactionError, CompactionPreparation, CompactGenerationOptions,
    CutPointResult, GeneratedSummary, RetryAttemptCallback, RetryFinishedCallback,
    RetryScheduledCallback, SummaryGenerationOptions, SummaryRequest, SummaryRetryCallbacks,
    add_usage, calculate_context_tokens, compact, compact_with_request, complete_simple_with_retries,
    create_summary_request_options, estimate_context_tokens, estimate_tokens, find_cut_point,
    find_turn_start_index, find_valid_cut_points, generate_summary, generate_summary_with_request,
    generate_summary_with_usage, get_last_assistant_usage, is_retryable_assistant_error,
    prepare_compaction, should_compact,
};
pub use context::{build_context_entries, is_context_message, session_entry_to_context_messages};
pub use messages::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX, bash_execution_to_text, convert_to_llm,
    create_branch_summary_message, create_compaction_summary_message, message_timestamp,
};
pub use core::{
    SUMMARIZATION_PROMPT, SUMMARIZATION_SYSTEM_PROMPT, TURN_PREFIX_SUMMARIZATION_PROMPT,
    UPDATE_SUMMARIZATION_PROMPT,
};
pub use utils::{
    FileLists, FileOperations, assistant_content_text, compute_file_lists, create_file_ops,
    estimate_custom_content_chars, estimate_text_and_image_content_chars,
    estimate_tool_result_content_chars, extract_file_ops_from_message, format_file_operations,
    js_string_len, serialize_conversation, truncate_for_summary,
};

/// Hook-owned result records are the canonical algorithm outputs too.
pub use crate::harness::hooks::{BranchSummaryResult, CompactResult};
