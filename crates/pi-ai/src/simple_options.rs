//! Shared simple-stream option shaping (TS `api/simple-options.ts`).
//!
//! `streamSimple` / agent request paths use these helpers to clamp `maxTokens`
//! against the remaining context window and to size thinking budgets.

use crate::estimate::estimate_context_tokens;
use crate::provider::StreamOptions;
use crate::types::{Context, Model, ThinkingLevel};
use serde::Deserialize;

/// Safety margin reserved between context usage and the output budget.
pub const CONTEXT_SAFETY_TOKENS: u64 = 4_096;
const MIN_MAX_TOKENS: u64 = 1;
const MIN_OUTPUT_TOKENS: u64 = 1_024;

/// Default thinking budget for the minimal level.
pub const DEFAULT_THINKING_BUDGET_MINIMAL: u64 = 1_024;
/// Default thinking budget for the low level.
pub const DEFAULT_THINKING_BUDGET_LOW: u64 = 2_048;
/// Default thinking budget for the medium level.
pub const DEFAULT_THINKING_BUDGET_MEDIUM: u64 = 8_192;
/// Default thinking budget for the high level (also used for xhigh/max).
pub const DEFAULT_THINKING_BUDGET_HIGH: u64 = 16_384;

/// Default maximum delay (ms) honored for server-requested retry waits.
///
/// TypeScript `StreamOptions.maxRetryDelayMs` default. Adapters that implement
/// client-side retry (for example Codex) must use this when the option is
/// unset; set `0` to disable the cap.
pub const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Implicit default for [`StreamOptions::cache_retention`] when unset.
///
/// Provider adapters already apply `short` (with `PI_CACHE_RETENTION=long`
/// override). Documented here so product/request paths can centralize the
/// same default without inventing new knobs.
pub const DEFAULT_CACHE_RETENTION: crate::types::CacheRetention =
    crate::types::CacheRetention::Short;

/// Token budgets for each thinking level (token-based providers only).
///
/// Mirrors TypeScript `ThinkingBudgets`. Missing fields fall back to the
/// defaults in [`default_thinking_budgets`].
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct ThinkingBudgets {
    /// Budget for [`ThinkingLevel::Minimal`].
    pub minimal: Option<u64>,
    /// Budget for [`ThinkingLevel::Low`].
    pub low: Option<u64>,
    /// Budget for [`ThinkingLevel::Medium`].
    pub medium: Option<u64>,
    /// Budget for [`ThinkingLevel::High`] (and xhigh/max after clamp).
    pub high: Option<u64>,
}

impl ThinkingBudgets {
    /// Resolve the budget for a (already-clamped) level.
    #[must_use]
    pub fn budget_for(self, level: ThinkingLevel) -> u64 {
        let defaults = default_thinking_budgets();
        match level {
            ThinkingLevel::Minimal => self.minimal.unwrap_or(defaults.minimal),
            ThinkingLevel::Low => self.low.unwrap_or(defaults.low),
            ThinkingLevel::Medium => self.medium.unwrap_or(defaults.medium),
            ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => {
                self.high.unwrap_or(defaults.high)
            }
        }
    }
}

/// Default thinking budgets used when callers omit a custom map.
#[must_use]
pub fn default_thinking_budgets() -> ThinkingBudgetsResolved {
    ThinkingBudgetsResolved {
        minimal: DEFAULT_THINKING_BUDGET_MINIMAL,
        low: DEFAULT_THINKING_BUDGET_LOW,
        medium: DEFAULT_THINKING_BUDGET_MEDIUM,
        high: DEFAULT_THINKING_BUDGET_HIGH,
    }
}

/// Fully-resolved default budget table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinkingBudgetsResolved {
    /// Minimal level budget.
    pub minimal: u64,
    /// Low level budget.
    pub low: u64,
    /// Medium level budget.
    pub medium: u64,
    /// High level budget.
    pub high: u64,
}

/// Clamp a requested max-token budget to the remaining context window.
///
/// `available = contextWindow - estimateContextTokens(context) - 4096`, floored
/// at 1. When `contextWindow` is 0 the request is not clamped against context.
#[must_use]
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let estimated = estimate_context_tokens(context).tokens;
    let available = model
        .context_window
        .saturating_sub(estimated)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    let floor = available.max(MIN_MAX_TOKENS);
    max_tokens.min(floor)
}

/// Unified simple-stream options (TS `SimpleStreamOptions`).
///
/// Extends the transport [`StreamOptions`] surface with reasoning effort and
/// optional custom thinking budgets. Product code maps these through the
/// typed provider-option vocabulary for the adapters that consume them.
#[derive(Clone, Default)]
pub struct SimpleStreamOptions {
    /// Base stream options (temperature, headers, timeouts, …).
    pub base: StreamOptions,
    /// Requested reasoning effort.
    pub reasoning: Option<ThinkingLevel>,
    /// Custom token budgets for thinking levels.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Build transport stream options from simple options, clamping max tokens.
///
/// Mirrors `buildBaseOptions`: uses `options.max_tokens ?? model.max_tokens`,
/// then [`clamp_max_tokens_to_context`]. Does not map reasoning into `extra`;
/// callers that need adapter-specific reasoning keys do that separately (as
/// TypeScript `streamSimple` does per API).
#[must_use]
pub fn build_base_options(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
    api_key: Option<String>,
) -> StreamOptions {
    let empty = StreamOptions::default();
    let options = options.unwrap_or(&empty);
    let requested_max = options.max_tokens.unwrap_or(model.max_tokens);
    StreamOptions {
        temperature: options.temperature,
        max_tokens: Some(clamp_max_tokens_to_context(model, context, requested_max)),
        signal: options.signal.clone(),
        api_key: api_key.or_else(|| options.api_key.clone()),
        transport: options.transport,
        cache_retention: options.cache_retention,
        session_id: options.session_id.clone(),
        headers: options.headers.clone(),
        on_payload: options.on_payload.clone(),
        on_response: options.on_response.clone(),
        timeout_ms: options.timeout_ms,
        websocket_connect_timeout_ms: options.websocket_connect_timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        metadata: options.metadata.clone(),
        env: options.env.clone(),
        extra: options.extra.clone(),
    }
}

/// Collapse `xhigh` / `max` to `high` for token-budget tables.
#[must_use]
pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh | ThinkingLevel::Max) => Some(ThinkingLevel::High),
        other => other,
    }
}

/// Result of [`adjust_max_tokens_for_thinking`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjustedMaxTokens {
    /// Output budget including thinking room.
    pub max_tokens: u64,
    /// Thinking budget after fitting inside `max_tokens`.
    pub thinking_budget: u64,
}

/// Fit a thinking budget inside the model/output max-token cap.
///
/// `base_max_tokens == None` means no explicit caller cap: use `model_max_tokens`
/// and fit thinking inside it. When a caller cap is set, thinking is added on
/// top and then re-capped to the model max. If the resulting max is smaller
/// than the thinking budget, thinking shrinks to leave [`MIN_OUTPUT_TOKENS`].
#[must_use]
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<ThinkingBudgets>,
) -> AdjustedMaxTokens {
    let budgets = custom_budgets.unwrap_or_default();
    let level = clamp_reasoning(Some(reasoning_level)).unwrap_or(ThinkingLevel::High);
    let mut thinking_budget = budgets.budget_for(level);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => base.saturating_add(thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = max_tokens.saturating_sub(MIN_OUTPUT_TOKENS);
    }

    AdjustedMaxTokens {
        max_tokens,
        thinking_budget,
    }
}

/// Apply simple-stream max-token clamping for a prepared request.
///
/// Used by product `stream_simple` paths that already hold a
/// [`StreamOptions`]: clamps `max_tokens` the way `buildBaseOptions` does.
#[must_use]
pub fn apply_simple_max_tokens_clamp(
    model: &Model,
    context: &Context,
    mut options: StreamOptions,
) -> StreamOptions {
    let requested = options.max_tokens.unwrap_or(model.max_tokens);
    options.max_tokens = Some(clamp_max_tokens_to_context(model, context, requested));
    options
}

/// Apply thinking-budget adjustment then re-clamp to context.
///
/// Port of the Anthropic/Bedrock `streamSimple` sequence:
/// `adjustMaxTokensForThinking` then `clampMaxTokensToContext`.
#[must_use]
pub fn apply_thinking_and_context_clamp(
    model: &Model,
    context: &Context,
    mut options: StreamOptions,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<ThinkingBudgets>,
) -> (StreamOptions, u64) {
    let adjusted = adjust_max_tokens_for_thinking(
        options.max_tokens,
        model.max_tokens,
        reasoning_level,
        custom_budgets,
    );
    let max_tokens = clamp_max_tokens_to_context(model, context, adjusted.max_tokens);
    // Keep thinking budget inside the final max (Bedrock re-min against max-1024).
    let thinking_budget = adjusted
        .thinking_budget
        .min(max_tokens.saturating_sub(MIN_OUTPUT_TOKENS));
    options.max_tokens = Some(max_tokens);
    (options, thinking_budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput, UserMessage, UserMessageContent};
    use std::collections::BTreeMap;

    fn sample_model(context_window: u64, max_tokens: u64) -> Model {
        Model {
            id: "m".into(),
            name: "M".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://example.test".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window,
            max_tokens,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn empty_context() -> Context {
        Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        }
    }

    #[test]
    fn clamp_matrix_unset_and_overflow() {
        let model = sample_model(10_000, 4_096);
        let context = empty_context();
        // Unset → model max, then clamped (10000 - 0 - 4096 = 5904) so 4096 stays.
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context, model.max_tokens),
            4_096
        );
        // Overflowing request shrinks to available window.
        assert_eq!(
            clamp_max_tokens_to_context(&model, &context, 50_000),
            10_000 - CONTEXT_SAFETY_TOKENS
        );
        // Zero context window: no clamp beyond min floor.
        let open = sample_model(0, 4_096);
        assert_eq!(clamp_max_tokens_to_context(&open, &context, 7), 7);
    }

    #[test]
    fn clamp_accounts_for_estimated_messages() {
        let model = sample_model(8_000, 4_096);
        let context = Context {
            system_prompt: None,
            messages: vec![crate::types::Message::User(UserMessage::new(
                UserMessageContent::Text("x".repeat(4_000)),
                1,
            ))],
            tools: None,
        };
        // 4000 chars → 1000 tokens; available = 8000 - 1000 - 4096 = 2904.
        assert_eq!(clamp_max_tokens_to_context(&model, &context, 4_096), 2_904);
    }

    #[test]
    fn thinking_budgets_per_level() {
        let defaults = ThinkingBudgets::default();
        assert_eq!(
            defaults.budget_for(ThinkingLevel::Minimal),
            DEFAULT_THINKING_BUDGET_MINIMAL
        );
        assert_eq!(
            defaults.budget_for(ThinkingLevel::Low),
            DEFAULT_THINKING_BUDGET_LOW
        );
        assert_eq!(
            defaults.budget_for(ThinkingLevel::Medium),
            DEFAULT_THINKING_BUDGET_MEDIUM
        );
        assert_eq!(
            defaults.budget_for(ThinkingLevel::High),
            DEFAULT_THINKING_BUDGET_HIGH
        );
        assert_eq!(
            defaults.budget_for(ThinkingLevel::Xhigh),
            DEFAULT_THINKING_BUDGET_HIGH
        );
        assert_eq!(
            defaults.budget_for(ThinkingLevel::Max),
            DEFAULT_THINKING_BUDGET_HIGH
        );

        let custom = ThinkingBudgets {
            high: Some(2_000),
            ..ThinkingBudgets::default()
        };
        assert_eq!(custom.budget_for(ThinkingLevel::High), 2_000);
    }

    #[test]
    fn adjust_max_tokens_for_thinking_matrix() {
        // No caller cap: use model max, keep full thinking budget.
        let adjusted = adjust_max_tokens_for_thinking(None, 16_384, ThinkingLevel::Medium, None);
        assert_eq!(adjusted.max_tokens, 16_384);
        assert_eq!(adjusted.thinking_budget, DEFAULT_THINKING_BUDGET_MEDIUM);

        // Caller cap + thinking, re-capped to model max.
        let adjusted =
            adjust_max_tokens_for_thinking(Some(8_000), 10_000, ThinkingLevel::High, None);
        // 8000 + 16384 = 24384 → min 10000.
        assert_eq!(adjusted.max_tokens, 10_000);
        // max_tokens (10000) <= thinking (16384) → shrink to 10000 - 1024.
        assert_eq!(adjusted.thinking_budget, 10_000 - MIN_OUTPUT_TOKENS);

        // xhigh clamps to high budget table.
        let adjusted = adjust_max_tokens_for_thinking(None, 32_000, ThinkingLevel::Xhigh, None);
        assert_eq!(adjusted.thinking_budget, DEFAULT_THINKING_BUDGET_HIGH);
    }

    #[test]
    fn build_base_options_clamps_and_preserves_headers() {
        let model = sample_model(10_000, 4_096);
        let context = empty_context();
        let mut options = StreamOptions {
            max_tokens: Some(50_000),
            cache_retention: None,
            max_retry_delay_ms: None,
            ..StreamOptions::default()
        };
        options.headers = Some(BTreeMap::from([
            ("X-Keep".into(), Some("1".into())),
            ("X-Drop".into(), None),
        ]));
        let built = build_base_options(&model, &context, Some(&options), Some("key".into()));
        assert_eq!(built.max_tokens, Some(10_000 - CONTEXT_SAFETY_TOKENS));
        assert_eq!(built.api_key.as_deref(), Some("key"));
        assert_eq!(
            built
                .headers
                .as_ref()
                .and_then(|h| h.get("X-Drop"))
                .cloned(),
            Some(None)
        );
    }
}
