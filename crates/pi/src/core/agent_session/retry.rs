//! Auto-retry with exponential backoff for transient provider errors.
//!
//! Owns the retry classification, backoff schedule, abortable sleep, and the
//! `auto_retry_start` / `auto_retry_end` lifecycle events. `agent_end.willRetry`
//! is computed here so the event pump can annotate the public event.
//!
//! # Event ordering
//!
//! Single transient error then success:
//! `auto_retry_start{1}` → (backoff) → successful assistant `message_end` →
//! `auto_retry_end{success:true, attempt:1}` (emitted from persistence).
//!
//! Exhausted retries (maxRetries = 2, three consecutive errors):
//! `auto_retry_start{1}` → `auto_retry_start{2}` →
//! `auto_retry_end{success:false, attempt:2, final_error}` (emitted from
//! [`AgentSession::handle_post_agent_run`]).
//!
//! Aborted during sleep:
//! `auto_retry_start{1}` → abort →
//! `auto_retry_end{success:false, attempt:1, final_error:"Retry cancelled"}`.
//!
//! # Lock discipline
//!
//! `retry_attempt` lives on `AgentSessionInner` under the std `Mutex`. The
//! persistence half (pump task) resets it on success; the prompt half (caller
//! task) increments and reads it. Never hold the inner mutex across `.await`.

use std::sync::LazyLock;
use std::time::Duration;

use pi_ai::{AssistantMessage, StopReason};
use regex::Regex;

use super::AgentSession;
use super::compaction::is_context_overflow;
use super::events::AgentSessionEvent;

impl AgentSession {
    // -----------------------------------------------------------------
    // Public accessors
    // -----------------------------------------------------------------

    /// Current retry attempt (0 when not retrying).
    #[must_use]
    pub fn retry_attempt(&self) -> u32 {
        self.lock_inner().retry_attempt
    }

    // -----------------------------------------------------------------
    // Classification
    // -----------------------------------------------------------------

    /// Whether `message` is a transient error that auto-retry should handle.
    ///
    /// Context overflow and auth errors are NOT retryable: overflow is owned by
    /// compaction, and auth errors require user action.
    pub(super) fn is_retryable_error(&self, message: &AssistantMessage) -> bool {
        let context_window = self.model().context_window;
        if is_context_overflow(message, context_window) {
            return false;
        }
        is_retryable_assistant_error(message)
    }

    // -----------------------------------------------------------------
    // Backoff + sleep
    // -----------------------------------------------------------------

    /// Prepare a retry: increment attempt, emit `auto_retry_start`, drop the
    /// trailing error assistant from agent state, then sleep with backoff.
    ///
    /// Returns `Ok(true)` when the caller should `continue_run`.
    /// Returns `Ok(false)` when retries are disabled, exhausted, or aborted.
    pub(super) async fn prepare_retry(
        self: &std::sync::Arc<Self>,
        message: &AssistantMessage,
    ) -> bool {
        // Runtime flags (`auto_retry_enabled` / `max_retries`) are the source of
        // truth for the live session — same as will_retry / set_auto_retry_enabled.
        // Settings only supply base_delay_ms (not mutated by the toggle).
        let (enabled, max_retries) = {
            let inner = self.lock_inner();
            (inner.auto_retry_enabled, inner.max_retries)
        };
        if !enabled {
            return false;
        }

        let base_delay_ms = self.lock_settings().get_retry_settings().base_delay_ms;

        // Increment attempt; bail (preserving count) when over the limit.
        let attempt = {
            let mut inner = self.lock_inner();
            inner.retry_attempt = inner.retry_attempt.saturating_add(1);
            if inner.retry_attempt > max_retries {
                inner.retry_attempt = inner.retry_attempt.saturating_sub(1);
                return false;
            }
            inner.retry_attempt
        };

        let delay_ms = backoff_delay_ms(base_delay_ms, attempt);

        self.emit_public(AgentSessionEvent::AutoRetryStart {
            attempt,
            max_attempts: max_retries,
            delay_ms,
            error_message: message
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_owned()),
        });

        // Remove the trailing error assistant from agent state so the retry
        // re-generates it. Session history keeps the original entry.
        let _ = self.agent.pop_last_if_assistant();

        // Abortable exponential backoff sleep.
        let token = self.begin_retry_abort();
        tokio::select! {
            () = token.cancelled() => {
                let attempt = {
                    let mut inner = self.lock_inner();
                    let prev = inner.retry_attempt;
                    inner.retry_attempt = 0;
                    prev
                };
                self.clear_retry_abort();
                self.emit_public(AgentSessionEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: Some("Retry cancelled".to_owned()),
                });
                false
            }
            () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {
                self.clear_retry_abort();
                true
            }
        }
    }

    // -----------------------------------------------------------------
    // Terminal failure emit
    // -----------------------------------------------------------------

    /// Emit `auto_retry_end{success:false}` when an error follows retries and
    /// retry did not fire again. Resets the counter. No-op when `retry_attempt`
    /// is already 0.
    pub(super) fn emit_retry_exhausted(&self, message: &AssistantMessage) {
        let (attempt, final_error) = {
            let mut inner = self.lock_inner();
            if message.stop_reason == StopReason::Error && inner.retry_attempt > 0 {
                let attempt = inner.retry_attempt;
                inner.retry_attempt = 0;
                (attempt, message.error_message.clone())
            } else {
                return;
            }
        };
        self.emit_public(AgentSessionEvent::AutoRetryEnd {
            success: false,
            attempt,
            final_error,
        });
    }
}

// -----------------------------------------------------------------------
// Free functions
// -----------------------------------------------------------------------

/// Compute exponential backoff: `base * 2^(attempt-1)` (saturating).
fn backoff_delay_ms(base_delay_ms: u64, attempt: u32) -> u64 {
    if attempt == 0 {
        return 0;
    }
    let exp = attempt.saturating_sub(1);
    base_delay_ms.saturating_mul(2u64.saturating_pow(exp))
}

/// Whether `text` contains `status` as a standalone decimal status token.
fn contains_status_code(text: &str, status: &str) -> bool {
    text.match_indices(status).any(|(index, _)| {
        let before_is_digit = text[..index]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_ascii_digit());
        let after = index.saturating_add(status.len());
        let after_is_digit = text[after..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit());
        !before_is_digit && !after_is_digit
    })
}

/// Non-retryable quota/billing/subscription provider error patterns.
///
/// Port of `pi-ai/utils/retry.ts::NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`.
/// These defeat retry even when the message also contains a transient-looking
/// status code such as 429.
static NON_RETRYABLE_PROVIDER_LIMIT_PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let patterns = [
        "GoUsageLimitError",
        "FreeUsageLimitError",
        "Monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ];
    Regex::new(&format!("(?i)(?:{})", patterns.join("|"))).ok()
});

/// Retryable provider/transient error patterns.
///
/// Port of `pi-ai/utils/retry.ts::RETRYABLE_PROVIDER_ERROR_PATTERN`. Numeric
/// HTTP status codes are handled separately by [`contains_status_code`] so they
/// are recognised only as standalone tokens (e.g. "429" but not "14290").
static RETRYABLE_PROVIDER_ERROR_PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let patterns = [
        // Generic provider load, HTTP status, and server-side transient failures.
        "overloaded",
        "rate.?limit",
        "too many requests",
        "service.?unavailable",
        "server.?error",
        "internal.?error",
        // Wrapper/provider text for transient upstream failures.
        "provider.?returned.?error",
        // Network, proxy, and fetch transport failures.
        "network.?error",
        "connection.?error",
        "connection.?refused",
        "connection.?lost",
        "other side closed",
        "fetch failed",
        "upstream.?connect",
        "reset before headers",
        "socket hang up",
        "socket connection was closed",
        "timed? out",
        "timeout",
        "terminated",
        // WebSocket transports.
        "websocket.?closed",
        "websocket.?error",
        // Premature stream endings.
        "ended without",
        "stream ended before message_stop",
        "stream ended before a terminal response event",
        "http2 request did not get a response",
        // Provider-requested retry delay / guidance.
        "retry delay",
        "you can retry your request",
        "try your request again",
        "please retry your request",
        // gRPC based providers (e.g. NVIDIA NIM).
        "ResourceExhausted",
    ];
    Regex::new(&format!("(?i)(?:{})", patterns.join("|"))).ok()
});

const RETRYABLE_STATUS_CODES: &[&str] = &["429", "500", "502", "503", "504", "524"];

/// Whether the assistant message is a retryable transient error.
///
/// Non-retryable: auth failures, context overflow, and provider limit/billing
/// errors (overflow is caught by [`is_context_overflow`] first, but this
/// function is also safe to call directly).
pub(crate) fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    let lower = err.to_ascii_lowercase();

    // Provider quota/billing/subscription limits defeat retry first.
    if NON_RETRYABLE_PROVIDER_LIMIT_PATTERN
        .as_ref()
        .is_some_and(|re| re.is_match(&lower))
    {
        return false;
    }

    // Auth failures and context-overflow wording are not transient.
    if lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("context overflow")
        || lower.contains("context length")
        || lower.contains("maximum context")
    {
        return false;
    }

    // Transient / retryable patterns.
    if RETRYABLE_PROVIDER_ERROR_PATTERN
        .as_ref()
        .is_some_and(|re| re.is_match(&lower))
    {
        return true;
    }

    for status in RETRYABLE_STATUS_CODES {
        if contains_status_code(&lower, status) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_progression() {
        assert_eq!(backoff_delay_ms(1000, 1), 1000);
        assert_eq!(backoff_delay_ms(1000, 2), 2000);
        assert_eq!(backoff_delay_ms(1000, 3), 4000);
        assert_eq!(backoff_delay_ms(1000, 0), 0);
        // Saturates rather than overflowing.
        assert_eq!(backoff_delay_ms(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn overloaded_is_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("overloaded_error".to_owned());
        assert!(is_retryable_assistant_error(&msg));
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn invalid_key_not_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("invalid_api_key".to_owned());
        assert!(!is_retryable_assistant_error(&msg));
    }

    #[test]
    fn context_overflow_not_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("This model's maximum context length is 8192 tokens.".to_owned());
        assert!(!is_retryable_assistant_error(&msg));
        assert!(is_context_overflow(&msg, 0));
    }

    #[test]
    fn rate_limit_is_retryable_but_not_overflow() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("rate_limit exceeded".to_owned());
        assert!(is_retryable_assistant_error(&msg));
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn status_first_429_is_retryable() {
        for error in [
            "OpenAI API error: 429: {}",
            "HTTP 429: ",
            "provider failed with status 429",
        ] {
            let mut msg = AssistantMessage::new("api", "provider", "m", 0);
            msg.stop_reason = StopReason::Error;
            msg.error_message = Some(error.to_owned());
            assert!(is_retryable_assistant_error(&msg), "{error}");
        }

        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("internal code 14290".to_owned());
        assert!(!is_retryable_assistant_error(&msg));
    }

    #[test]
    fn network_connection_lost_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("Network connection lost.".to_owned());
        assert!(is_retryable_assistant_error(&msg));
    }

    #[test]
    fn bedrock_retry_text() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("Please try your request again later.".to_owned());
        assert!(is_retryable_assistant_error(&msg));
    }

    #[test]
    fn openai_retry_text() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("The server had an error while processing your request. Sorry about that! Please retry your request.".to_owned());
        assert!(is_retryable_assistant_error(&msg));
    }

    #[test]
    fn quota_429_not_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("429 insufficient_quota billing error".to_owned());
        assert!(!is_retryable_assistant_error(&msg));
    }

    #[test]
    fn socket_hang_up_retryable() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("provider fetch failed: socket hang up".to_owned());
        assert!(is_retryable_assistant_error(&msg));
    }

    #[test]
    fn existing_classifications_unchanged() {
        let mut overloaded = AssistantMessage::new("api", "provider", "m", 0);
        overloaded.stop_reason = StopReason::Error;
        overloaded.error_message = Some("overloaded_error".to_owned());
        assert!(is_retryable_assistant_error(&overloaded));

        let mut invalid = AssistantMessage::new("api", "provider", "m", 0);
        invalid.stop_reason = StopReason::Error;
        invalid.error_message = Some("invalid_api_key".to_owned());
        assert!(!is_retryable_assistant_error(&invalid));

        let mut embedded = AssistantMessage::new("api", "provider", "m", 0);
        embedded.stop_reason = StopReason::Error;
        embedded.error_message = Some("internal code 14290".to_owned());
        assert!(!is_retryable_assistant_error(&embedded));
    }

    #[test]
    fn throttling_not_overflow() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("Throttling error: Too many tokens, please wait.".to_owned());
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn stop_reason_non_error_not_retryable() {
        let msg = AssistantMessage::new("api", "provider", "m", 0);
        assert!(!is_retryable_assistant_error(&msg));
    }

    #[tokio::test]
    async fn usage_exceeds_window_is_overflow_not_retryable()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::super::AgentSessionConfig;
        use futures::stream::{self, BoxStream};
        use pi_ai::{
            AssistantMessageEvent, Context, Model, Provider, ProviderError, StreamOptions,
        };
        use std::collections::BTreeMap;
        use std::sync::Arc;

        struct NoopProvider;
        impl Provider for NoopProvider {
            fn stream(
                &self,
                _model: &Model,
                _ctx: Context,
                _opts: StreamOptions,
            ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                Box::pin(stream::iter(Vec::new()))
            }
        }

        let model = Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: "url".to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![pi_ai::ModelInput::Text],
            cost: pi_ai::ModelCost::default(),
            context_window: 8192,
            max_tokens: 1024,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        };

        let config = AgentSessionConfig::test_config(Arc::new(NoopProvider), model)?;
        let session = AgentSession::new(config)?;

        // 1. Silent overflow (stop reason: Stop, input usage > window)
        let mut msg_silent = AssistantMessage::new("test-api", "test-provider", "m", 0);
        msg_silent.stop_reason = StopReason::Stop;
        msg_silent.usage.input = 10000;
        assert!(!session.is_retryable_error(&msg_silent));

        // 2. Length overflow (stop reason: Length, output: 0, input exceeds window)
        let mut msg_length = AssistantMessage::new("test-api", "test-provider", "m", 0);
        msg_length.stop_reason = StopReason::Length;
        msg_length.usage.input = 8192;
        msg_length.usage.output = 0;
        assert!(!session.is_retryable_error(&msg_length));

        // 3. Normal transient error (stop reason: Error, under window) -> should be retryable
        let mut msg_normal = AssistantMessage::new("test-api", "test-provider", "m", 0);
        msg_normal.stop_reason = StopReason::Error;
        msg_normal.error_message = Some("overloaded".to_owned());
        msg_normal.usage.input = 1000;
        assert!(session.is_retryable_error(&msg_normal));

        Ok(())
    }
}
