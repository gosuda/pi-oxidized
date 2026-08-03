//! Provider contracts, transports, models, and credentials.

pub mod auth;
pub mod catalog;
pub mod estimate;
pub mod lockfile;
pub mod models_store;
pub mod provider;
pub mod providers;
pub mod simple_options;
pub mod types;

pub use estimate::{
    ContextUsageEstimate, calculate_context_tokens, estimate_context_tokens,
    estimate_message_tokens, estimate_messages_tokens, estimate_text_and_image_content_tokens,
    estimate_text_tokens,
};
pub use provider::{Provider, ProviderError, ProviderResponse, StreamOptions};
pub use simple_options::{
    AdjustedMaxTokens, CONTEXT_SAFETY_TOKENS, DEFAULT_CACHE_RETENTION, DEFAULT_MAX_RETRY_DELAY_MS,
    DEFAULT_THINKING_BUDGET_HIGH, DEFAULT_THINKING_BUDGET_LOW, DEFAULT_THINKING_BUDGET_MEDIUM,
    DEFAULT_THINKING_BUDGET_MINIMAL, SimpleStreamOptions, ThinkingBudgets, ThinkingBudgetsResolved,
    adjust_max_tokens_for_thinking, apply_simple_max_tokens_clamp,
    apply_thinking_and_context_clamp, build_base_options, clamp_max_tokens_to_context,
    clamp_reasoning, default_thinking_budgets,
};
pub use types::*;
