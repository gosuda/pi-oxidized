//! Session statistics + context usage impls.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/agent-session.ts`
//! `getSessionStats`, `getContextUsage`, and the
//! `ContextUsage` / `SessionStats` types.
//!
//! Behaviour preserved from the TypeScript contract:
//! - `get_session_stats` aggregates over **all** session entries (including
//!   history that was compacted away), so token / cost totals reflect what
//!   was actually billed across the session.
//! - `get_context_usage` reports `tokens: null` / `percent: null` when the
//!   latest compaction has no post-compaction assistant usage yet (the next
//!   LLM response must establish the new baseline).
//! - Cost / token sums use the canonical [`pi_ai::Usage`] fields, with the
//!   total falling back to `input + output + cache_read + cache_write` when
//!   `total_tokens` is zero (TypeScript `calculateContextTokens`).
//!
//! Lock order: only `AgentSessionInner` (briefly, to read mirrors). The
//! session manager async mutex is acquired for entry enumeration.

use pi_ai::{AssistantContent, Message, Model, StopReason};

use crate::core::compaction::{calculate_context_tokens, estimate_context_tokens};
use crate::core::sessions::{SessionEntry, get_latest_compaction_entry};

use super::AgentSession;

/// Aggregate session statistics (TypeScript `SessionStats`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStats {
    /// Session file path, if any.
    pub session_file: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Number of user messages.
    pub user_messages: u64,
    /// Number of assistant messages.
    pub assistant_messages: u64,
    /// Number of tool calls across all assistant messages.
    pub tool_calls: u64,
    /// Number of tool-result messages.
    pub tool_results: u64,
    /// Total messages (assistant + user + toolResult).
    pub total_messages: u64,
    /// Token totals.
    pub tokens: SessionTokenTotals,
    /// Total billed cost in US dollars.
    pub cost: f64,
    /// Context-usage snapshot, when computable.
    pub context_usage: Option<ContextUsage>,
}

/// Token totals (TypeScript `SessionStats['tokens']`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionTokenTotals {
    /// Sum of `usage.input` across assistant messages, compaction, and
    /// branch-summary entries.
    pub input: u64,
    /// Sum of `usage.output` across assistant messages, compaction, and
    /// branch-summary entries.
    pub output: u64,
    /// Sum of `usage.cacheRead` across assistant messages, compaction, and
    /// branch-summary entries.
    pub cache_read: u64,
    /// Sum of `usage.cacheWrite` across assistant messages, compaction, and
    /// branch-summary entries.
    pub cache_write: u64,
    /// `input + output + cache_read + cache_write`.
    pub total: u64,
}

/// Context-usage snapshot (TypeScript `ContextUsage`).
///
/// `tokens` / `percent` are `None` after a compaction until the next
/// assistant response establishes a fresh usage baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextUsage {
    /// Estimated context tokens (`None` when unknown after compaction).
    pub tokens: Option<u64>,
    /// Model context-window size.
    pub context_window: u64,
    /// `tokens / context_window * 100.0` (`None` when `tokens` is unknown).
    pub percent: Option<f64>,
}

impl AgentSession {
    /// Aggregate session statistics across all persisted entries.
    ///
    /// Counts / totals include compacted-away history so cost reflects what
    /// was actually billed. See [`Self::get_context_usage`] for the live
    /// context estimate used by the UI.
    pub async fn get_session_stats(&self) -> SessionStats {
        let (session_file, session_id, entries): (Option<String>, String, Vec<SessionEntry>) = {
            let manager = self.session_manager.lock().await;
            (
                manager.get_session_file().map(str::to_owned),
                manager.get_session_id().to_owned(),
                manager.get_entries().into_iter().cloned().collect(),
            )
        };

        let mut user_messages = 0u64;
        let mut assistant_messages = 0u64;
        let mut tool_results = 0u64;
        let mut total_messages = 0u64;
        let mut tool_calls = 0u64;
        let mut input = 0u64;
        let mut output = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;
        let mut cost = 0f64;

        for entry in &entries {
            // Persisted summary entries (compaction / branch_summary) carry
            // their own LLM usage from the summarization call. Include it in
            // token/cost totals so they reflect what was actually billed
            // (TypeScript `addUsageToTotals` on `entry.usage`).
            if let Some(usage) = summary_usage(entry) {
                input = input.saturating_add(usage.input);
                output = output.saturating_add(usage.output);
                cache_read = cache_read.saturating_add(usage.cache_read);
                cache_write = cache_write.saturating_add(usage.cache_write);
                cost += usage.cost.total;
            }

            let SessionEntry::Message(message_entry) = entry else {
                continue;
            };
            total_messages = total_messages.saturating_add(1);
            let message = &message_entry.message;
            match message.as_llm() {
                Some(Message::User(_)) => {
                    user_messages = user_messages.saturating_add(1);
                }
                Some(Message::ToolResult(_)) => {
                    tool_results = tool_results.saturating_add(1);
                }
                Some(Message::Assistant(assistant)) => {
                    assistant_messages = assistant_messages.saturating_add(1);
                    tool_calls = tool_calls.saturating_add(
                        assistant
                            .content
                            .iter()
                            .filter(|content| matches!(content, AssistantContent::ToolCall(_)))
                            .count() as u64,
                    );
                    input = input.saturating_add(assistant.usage.input);
                    output = output.saturating_add(assistant.usage.output);
                    cache_read = cache_read.saturating_add(assistant.usage.cache_read);
                    cache_write = cache_write.saturating_add(assistant.usage.cache_write);
                    cost += assistant.usage.cost.total;
                }
                None => {}
            }
        }

        let total = input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_write);

        SessionStats {
            session_file,
            session_id,
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages,
            tokens: SessionTokenTotals {
                input,
                output,
                cache_read,
                cache_write,
                total,
            },
            cost,
            context_usage: self.get_context_usage().await,
        }
    }

    /// Estimate current context usage against the active model's window.
    ///
    /// Returns `None` when the current model has no `contextWindow` (or no
    /// model is set). After the latest compaction on the active branch, the
    /// estimate is `None` until a post-compaction assistant response provides
    /// a fresh usage baseline (TypeScript `getContextUsage`).
    pub async fn get_context_usage(&self) -> Option<ContextUsage> {
        let model = self.model();
        let context_window = model.context_window;
        if context_window == 0 {
            return None;
        }

        // Branch + compaction boundary come from the persisted tree.
        let branch: Vec<SessionEntry> = {
            let manager = self.session_manager.lock().await;
            manager.get_branch(None).into_iter().cloned().collect()
        };

        let branch_refs: Vec<&SessionEntry> = branch.iter().collect();
        let latest_compaction = get_latest_compaction_entry(&branch_refs);

        if let Some(compaction) = latest_compaction {
            // `compaction` is `&CompactionEntry` borrowed from `branch_refs`.
            // Find the same entry by pointer identity in the owned `branch`
            // vector (the `&SessionEntry` pattern binds `c: &CompactionEntry`).
            let compaction_index = branch
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        SessionEntry::Compaction(c) if std::ptr::eq(c, compaction)
                    )
                })
                .unwrap_or(0);
            let has_post_compaction_usage = branch.iter().enumerate().any(|(idx, entry)| {
                if idx <= compaction_index {
                    return false;
                }
                post_compaction_usage_tokens(entry).is_some()
            });
            if !has_post_compaction_usage {
                return Some(ContextUsage {
                    tokens: None,
                    context_window,
                    percent: None,
                });
            }
        }

        let messages = self.messages();
        let estimate = estimate_context_tokens(&messages);
        let tokens = estimate.tokens;
        let percent = u64_as_f64(tokens) / u64_as_f64(context_window) * 100.0;
        Some(ContextUsage {
            tokens: Some(tokens),
            context_window,
            percent: Some(percent),
        })
    }
}

/// Convert a `u64` to `f64` without a precision-loss cast.
///
/// Splitting at the 32-bit boundary produces the same rounded binary value as
/// Rust's primitive conversion while keeping both integer-to-float conversions
/// lossless.
fn u64_as_f64(value: u64) -> f64 {
    let bytes = value.to_be_bytes();
    let high = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let low = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    f64::from(high).mul_add(4_294_967_296.0, f64::from(low))
}

/// LLM usage attached to a persisted summary entry (compaction or
/// branch-summary), when present.
///
/// Both entry types carry an optional `Usage` from the summarization call;
/// this extracts it through one path so the aggregation loop accumulates
/// once (special-case elimination).
fn summary_usage(entry: &SessionEntry) -> Option<&pi_ai::Usage> {
    match entry {
        SessionEntry::Compaction(c) => c.usage.as_ref(),
        SessionEntry::BranchSummary(b) => b.usage.as_ref(),
        _ => None,
    }
}

/// Tokens reported by an assistant entry usable as a post-compaction baseline.
///
/// Skips `aborted` / `error` stop reasons (TypeScript
/// `assistant.stopReason !== "aborted" && assistant.stopReason !== "error"`)
/// and zero-token usage records.
fn post_compaction_usage_tokens(entry: &SessionEntry) -> Option<u64> {
    let SessionEntry::Message(message_entry) = entry else {
        return None;
    };
    let message = &message_entry.message;
    let Message::Assistant(assistant) = message.as_llm()? else {
        return None;
    };
    if matches!(
        assistant.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        return None;
    }
    let tokens = calculate_context_tokens(&assistant.usage);
    if tokens == 0 {
        return None;
    }
    Some(tokens)
}

/// Helper retained for sibling modules / future slices that need to read the
/// active model's context window without downcasting the runtime.
#[allow(dead_code)]
fn model_context_window(model: &Model) -> u64 {
    model.context_window
}

#[cfg(test)]
mod tests {
    use crate::core::agent_session::{AgentSession, AgentSessionConfig};
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider, ProviderError,
        StreamOptions, Usage, UsageCost,
    };
    use std::sync::Arc;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn test_model() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[derive(Clone)]
    struct StubProvider;

    impl Provider for StubProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            stream::empty().boxed()
        }
    }

    fn make_session() -> TestResult<Arc<AgentSession>> {
        let config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        AgentSession::new(config).map_err(Into::into)
    }

    fn summary_usage(input: u64, output: u64, cost_total: f64) -> Usage {
        Usage {
            input,
            output,
            cache_read: input / 2,
            cache_write: output / 2,
            cost: UsageCost {
                total: cost_total,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A zero context-window model means `get_context_usage` cannot estimate
    /// and must return `None` (TypeScript `getContextUsage` early return).
    #[tokio::test]
    async fn context_usage_tokens_when_window_zero_is_none() -> TestResult {
        let mut model = test_model();
        model.context_window = 0;
        let session = {
            let config = AgentSessionConfig::test_config(Arc::new(StubProvider), model)?;
            AgentSession::new(config)?
        };
        // No compaction, no assistant entries — but the zero window alone
        // forces `None` before any branch inspection.
        assert!(session.get_context_usage().await.is_none());
        Ok(())
    }

    /// Two compaction usages whose input sums exceed `u64::MAX` must saturate
    /// at `u64::MAX` rather than overflowing (TypeScript saturating-add path).
    #[tokio::test]
    async fn token_totals_saturate_instead_of_overflow() -> TestResult {
        let session = make_session()?;
        // Each compaction carries input = u64::MAX; the aggregation path
        // uses `saturating_add`, so the total clamps at `u64::MAX`.
        let overflow_usage = summary_usage(u64::MAX, 0, 0.0);
        {
            let mut sm = session.session_manager.lock().await;
            sm.append_compaction(
                "first compaction",
                "kept1",
                1000,
                None,
                None,
                Some(overflow_usage.clone()),
            )?;
            sm.append_compaction(
                "second compaction",
                "kept2",
                2000,
                None,
                None,
                Some(overflow_usage),
            )?;
        }
        let stats = session.get_session_stats().await;
        assert_eq!(
            stats.tokens.input,
            u64::MAX,
            "input must saturate at u64::MAX, not overflow"
        );
        Ok(())
    }

    /// Totals must include persisted `usage` on `compaction` and
    /// `branch_summary` entries, not just assistant messages.
    #[tokio::test]
    async fn stats_include_compaction_and_branch_summary_usage() -> TestResult {
        let session = make_session()?;
        let compaction_usage = summary_usage(100, 200, 0.03);
        let branch_usage = summary_usage(10, 20, 0.01);

        {
            let mut sm = session.session_manager.lock().await;
            sm.append_compaction(
                "compaction summary",
                "kept1",
                1000,
                None,
                None,
                Some(compaction_usage),
            )?;
            sm.branch_with_summary(None, "branch summary", None, None, Some(branch_usage))?;
        }

        let stats = session.get_session_stats().await;

        // Compaction: input=100, output=200, cache_read=50, cache_write=100, cost=0.03
        // Branch:     input=10,  output=20,  cache_read=5,  cache_write=10,  cost=0.01
        // Totals:     input=110, output=220, cache_read=55, cache_write=110, cost=0.04
        assert_eq!(
            stats.tokens.input, 110,
            "input should include both summary usages"
        );
        assert_eq!(
            stats.tokens.output, 220,
            "output should include both summary usages"
        );
        assert_eq!(
            stats.tokens.cache_read, 55,
            "cache_read should include both summary usages"
        );
        assert_eq!(
            stats.tokens.cache_write, 110,
            "cache_write should include both summary usages"
        );
        assert!(
            (stats.cost - 0.04).abs() < f64::EPSILON,
            "cost should include both summary usages, got {}",
            stats.cost
        );
        // No message entries → message counts unchanged.
        assert_eq!(stats.total_messages, 0);
        assert_eq!(stats.assistant_messages, 0);
        Ok(())
    }
}
