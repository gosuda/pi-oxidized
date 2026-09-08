//! Immutable persisted configuration and structural-preparation records.
//!
//! These are the canonical durable declarations consumed by session
//! operations, storage backends, and (later) runtime/stream code. They are
//! pure serde data: no live callbacks, tools, hooks, providers, session
//! handles, or `Context` instances are stored here. Field names and serde
//! shapes mirror the candidate harness wire format exactly.

use std::collections::BTreeMap;

use pi_ai::{CacheRetention, Transport};
use serde::{Deserialize, Serialize};

use crate::message::AgentMessage;

/// Whole-request retry policy captured by a durable generation or summary step.
///
/// The numeric fields mirror JavaScript safe-integer semantics at the Rust
/// boundary. `max_retries` is the retry count; normalized operation state stores
/// the corresponding total attempt count.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRetryPolicy {
    /// Whether whole-request retries are enabled.
    pub enabled: bool,
    /// Number of retries after the initial request.
    pub max_retries: u64,
    /// Base delay between whole-request retries, in milliseconds.
    pub base_delay_ms: u64,
}

/// Default whole-request retry policy: three retries after the initial attempt.
pub const DEFAULT_HARNESS_RETRY_POLICY: HarnessRetryPolicy = HarnessRetryPolicy {
    enabled: true,
    max_retries: 3,
    base_delay_ms: 1_000,
};

impl Default for HarnessRetryPolicy {
    fn default() -> Self {
        DEFAULT_HARNESS_RETRY_POLICY
    }
}

/// Retry policy values that cannot be represented safely at the JS wire edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("retry policy values must be non-negative JavaScript-safe integers and maxRetries must leave room for one attempt")]
pub struct InvalidRetryPolicy;

impl HarnessRetryPolicy {
    /// Validates the safe-integer bounds required by the harness wire format.
    ///
    /// `max_retries` must leave room for the initial attempt when normalized,
    /// so the maximum safe integer itself is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRetryPolicy`] when either numeric field exceeds the
    /// JavaScript safe-integer range or `max_retries` leaves no room for the
    /// initial attempt.
    pub fn validate(&self) -> Result<(), InvalidRetryPolicy> {
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

        if self.max_retries >= MAX_SAFE_INTEGER || self.base_delay_ms > MAX_SAFE_INTEGER {
            return Err(InvalidRetryPolicy);
        }
        Ok(())
    }
}

/// Compaction thresholds and retention settings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    /// Enable automatic compaction decisions.
    pub enabled: bool,
    /// Tokens reserved for summary prompt and output.
    pub reserve_tokens: u64,
    /// Approximate recent-context tokens to keep after compaction.
    pub keep_recent_tokens: u64,
}

/// Default compaction settings used by the harness.
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

/// Compaction trigger recorded on durable summary tasks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    /// User request.
    Manual,
    /// Token threshold.
    Threshold,
    /// Context overflow.
    Overflow,
}

/// Curated provider request options snapshotted per turn.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessStreamOptions {
    /// Preferred transport forwarded to the stream function.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// Provider request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Maximum provider retry attempts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// Optional cap for provider-requested retry delays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    /// Additional request headers merged with auth and lifecycle headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Provider metadata forwarded with requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    /// Provider cache retention hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    /// Ask a capable provider to continue generation asynchronously.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredRequest>,
}

/// Deferred-generation request: a bare flag or an options object.
///
/// `Flag(false)` is a real supplied `false`, not absence; `Options` with
/// `window: None` is a supplied `{}`, not an invented default window.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum DeferredRequest {
    /// `deferred: true | false`.
    Flag(bool),
    /// `deferred: { window?: "15m" | "1h" | "24h" }`.
    Options {
        /// Optional deferred completion window.
        #[serde(skip_serializing_if = "Option::is_none")]
        window: Option<DeferredWindow>,
    },
}

/// Deferred completion window accepted by capable providers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeferredWindow {
    /// Fifteen minutes.
    #[serde(rename = "15m")]
    FifteenMinutes,
    /// One hour.
    #[serde(rename = "1h")]
    OneHour,
    /// Twenty-four hours.
    #[serde(rename = "24h")]
    TwentyFourHours,
}

/// Durable form of file operations observed during a summarized span.
///
/// Live `FileOperations` are sets; the durable record stores arrays.
/// Algorithms convert explicitly at the boundary.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableFileOperations {
    /// Paths read.
    pub read: Vec<String>,
    /// Paths written.
    pub written: Vec<String>,
    /// Paths edited.
    pub edited: Vec<String>,
}

/// Persisted structural preparation for a summary task.
///
/// Both variants are concrete typed records, not opaque JSON, so recovery can
/// consume them without a serializer conversion or live streaming code.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum DurableStructuralPreparation {
    /// Compaction preparation.
    #[serde(rename = "compaction")]
    Compaction {
        /// Messages folded into the summary.
        messages_to_summarize: Vec<AgentMessage>,
        /// Turn-prefix messages retained ahead of the summary.
        turn_prefix_messages: Vec<AgentMessage>,
        /// Tail messages retained after the summary.
        retained_tail: Vec<AgentMessage>,
        /// Whether the summarized span splits a turn.
        is_split_turn: bool,
        /// Context token count before compaction.
        tokens_before: u64,
        /// Prior summary text this compaction extends.
        #[serde(skip_serializing_if = "Option::is_none")]
        previous_summary: Option<String>,
        /// File operations observed during the summarized span.
        file_ops: DurableFileOperations,
        /// Compaction settings in force for the task.
        settings: CompactionSettings,
    },
    /// Branch-summary preparation.
    #[serde(rename = "branch_summary")]
    BranchSummary {
        /// Messages folded into the branch summary.
        messages: Vec<AgentMessage>,
        /// File operations observed during the summarized span.
        file_ops: DurableFileOperations,
        /// Total token count of the summarized messages.
        total_tokens: u64,
    },
}
