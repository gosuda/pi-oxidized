//! Provider contracts, transports, models, and credentials.

pub mod assistant_message_frame;
pub mod auth;
pub mod catalog;
pub mod constrained_sampling;
pub mod estimate;
pub mod lockfile;
pub mod models_store;
pub mod provider;
pub mod providers;
pub mod radius_config;
pub mod simple_options;
pub mod types;

pub use assistant_message_frame::{
    AssistantMessageFrame, AssistantMessageFrameEncoder, AssistantMessageFrameError,
    reduce_assistant_message_frames,
};
pub use constrained_sampling::{
    ConstrainedSamplingError, GrammarConstrainedSampling, GrammarSyntax, GrammarToolInputBuffer,
    UnsupportedStrictJsonSchema, grammar_tool_input, grammar_tool_input_properties,
    make_strict_json_schema, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
pub use estimate::{
    ContextUsageEstimate, calculate_context_tokens, estimate_context_tokens,
    estimate_message_tokens, estimate_messages_tokens, estimate_text_and_image_content_tokens,
    estimate_text_tokens,
};
pub use provider::{
    CancelDeferredFn, DeferredCallbacks, FetchDeferredFn, Provider, ProviderError,
    ProviderResponse, StreamOptionKey, StreamOptions,
};
pub use simple_options::{
    AdjustedMaxTokens, CONTEXT_SAFETY_TOKENS, DEFAULT_CACHE_RETENTION, DEFAULT_MAX_RETRY_DELAY_MS,
    DEFAULT_THINKING_BUDGET_HIGH, DEFAULT_THINKING_BUDGET_LOW, DEFAULT_THINKING_BUDGET_MEDIUM,
    DEFAULT_THINKING_BUDGET_MINIMAL, SimpleStreamOptions, ThinkingBudgets, ThinkingBudgetsResolved,
    adjust_max_tokens_for_thinking, apply_simple_max_tokens_clamp,
    apply_thinking_and_context_clamp, build_base_options, clamp_max_tokens_to_context,
    clamp_reasoning, default_thinking_budgets,
};
pub use types::*;
