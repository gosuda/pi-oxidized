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

use std::time::Duration;

use pi_ai::{AssistantMessage, StopReason};

use super::AgentSession;
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
    pub(super) fn is_retryable_error(message: &AssistantMessage) -> bool {
        if is_context_overflow(message) {
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

/// Whether the assistant message is a context-overflow error.
///
/// Context overflow is handled by compaction, not retry. The detection is
/// conservative: it only fires for `stop_reason == Error` messages whose error
/// text matches overflow patterns, while excluding rate-limit and throttling
/// messages that mention tokens but are transient.
fn is_context_overflow(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    let lower = err.to_ascii_lowercase();

    // Exclude transient errors that may mention "tokens" or "limit".
    let is_non_overflow = lower.contains("rate_limit")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("throttling")
        || lower.contains("service unavailable")
        || lower.contains("overloaded");

    if is_non_overflow {
        return false;
    }

    lower.contains("context overflow")
        || lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("too long")
        || lower.contains("exceeds the limit")
        || lower.contains("exceeds the context")
        || lower.contains("exceeds model")
        || lower.contains("prompt has")
        || lower.contains("token count")
        || lower.contains("input is too long")
        || lower.contains("input length")
}

/// Whether the assistant message is a retryable transient error.
///
/// Non-retryable: auth failures and context overflow (overflow is caught by
/// [`is_context_overflow`] first, but this function is also safe to call
/// directly).
fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    let lower = err.to_ascii_lowercase();

    // Explicitly non-retryable.
    if lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("context overflow")
        || lower.contains("context length")
        || lower.contains("maximum context")
    {
        return false;
    }

    // Transient / retryable patterns.
    lower.contains("overloaded")
        || lower.contains("rate_limit")
        || lower.contains("rate limit")
        || lower.contains("network")
        || lower.contains("timeout")
        || lower.contains("server error")
        || lower.contains("internal error")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("500")
        || lower.contains("retry your request")
        || lower.contains("try your request again")
        || lower.contains("network connection lost")
        || lower.contains("temporarily unavailable")
        || lower.contains("service_unavailable")
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
        assert!(!is_context_overflow(&msg));
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
        assert!(is_context_overflow(&msg));
    }

    #[test]
    fn rate_limit_is_retryable_but_not_overflow() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("rate_limit exceeded".to_owned());
        assert!(is_retryable_assistant_error(&msg));
        assert!(!is_context_overflow(&msg));
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
    fn throttling_not_overflow() {
        let mut msg = AssistantMessage::new("api", "provider", "m", 0);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some("Throttling error: Too many tokens, please wait.".to_owned());
        assert!(!is_context_overflow(&msg));
    }

    #[test]
    fn stop_reason_non_error_not_retryable() {
        let msg = AssistantMessage::new("api", "provider", "m", 0);
        assert!(!is_retryable_assistant_error(&msg));
    }
}
