//! Lossless agent-event pump.
//!
//! Spawns exactly one task that drains `Agent::subscribe()` and for each event:
//! 1. runs pre-public side effects (`message_start` queue dequeue)
//! 2. awaits the extension handler (extension-before-public)
//! 3. applies `message_end` replacement to agent state when present
//! 4. emits the public `AgentSessionEvent` (listeners called without holding locks)
//! 5. persists `message_end` / emits auto-retry success
//!
//! Session-level `agent_settled` is emitted exactly once when the session run
//! flag flips false (after retries / follow-ups / auto-compaction). The pump
//! itself never emits settled; [`AgentSession::emit_agent_settled`] is the
//! single entry point used by the prompt lifecycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_agent::AgentEvent;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::AgentSession;
use super::events::AgentSessionEvent;

/// Handle for the background event pump.
pub(super) struct EventPump {
    /// Cancellation token cancelled on dispose / disconnect.
    pub cancel: CancellationToken,
    /// Join handle for the pump task.
    pub join: JoinHandle<()>,
    /// True while the pump is the active agent subscription.
    pub active: Arc<AtomicBool>,
}

impl AgentSession {
    /// Spawn the single lossless event pump for this session.
    pub(super) fn spawn_event_pump(self: &Arc<Self>) -> EventPump {
        let cancel = CancellationToken::new();
        let active = Arc::new(AtomicBool::new(true));
        let session = Arc::clone(self);
        let cancel_child = cancel.clone();
        let active_flag = Arc::clone(&active);
        let mut rx: UnboundedReceiver<AgentEvent> = self.agent.subscribe();
        let wait_cancel = self.lock_inner().agent_end_wait_cancel.clone();

        let join = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel_child.cancelled() => break,
                    event = rx.recv() => {
                        match event {
                            Some(event) => {
                                session.process_agent_event(event).await;
                            }
                            None => break,
                        }
                    }
                }
            }
            wait_cancel.cancel();
            active_flag.store(false, Ordering::SeqCst);
        });

        EventPump {
            cancel,
            join,
            active,
        }
    }

    /// Disconnect the pump without disposing the session (compaction pause).
    pub(super) fn disconnect_from_agent(&self) {
        self.lock_inner().agent_end_wait_cancel.cancel();
        if let Some(pump) = self.take_pump() {
            pump.cancel.cancel();
            // Detach; do not await join here (may be called from async context
            // that must not block). The task exits promptly on cancel.
            pump.join.abort();
        }
    }

    /// Reconnect a pump after disconnect.
    pub(super) fn reconnect_to_agent(self: &Arc<Self>) {
        if self.pump_is_active() {
            return;
        }
        self.lock_inner().agent_end_wait_cancel = CancellationToken::new();
        let pump = self.spawn_event_pump();
        self.store_pump(pump);
    }

    /// Process one agent event through extension → public → persistence.
    async fn process_agent_event(self: &Arc<Self>, event: AgentEvent) {
        let is_agent_end = matches!(&event, AgentEvent::AgentEnd { .. });
        // 1. Pre-public side effects for message_start:user (queue dequeue).
        if matches!(&event, AgentEvent::MessageStart { message } if message.role() == "user") {
            self.handle_agent_event_side_effects(&event, &AgentSessionEvent::AgentStart)
                .await;
        }

        // 2. Extension handler BEFORE public listeners.
        let mut public = self.map_agent_event_for_public(event.clone());
        if let AgentEvent::MessageEnd { message } = &event {
            let runner = self.hooks.runner();
            match runner.emit_message_end(message.clone()).await {
                Ok(replacement) => {
                    let replaced = self.apply_message_end_replacement(message.clone(), replacement);
                    public = AgentSessionEvent::MessageEnd { message: replaced };
                }
                Err(err) => {
                    runner.emit_error(err.to_string());
                }
            }
        } else {
            let runner = self.hooks.runner();
            let ext_event = public.clone();
            if let Err(err) = runner.emit(ext_event).await {
                runner.emit_error(err.to_string());
            }
        }

        // 3. Public listeners (no locks held).
        self.emit_public(public.clone());

        // 4. Persistence half for message_end (and other post-public effects).
        if matches!(&event, AgentEvent::MessageEnd { .. }) {
            self.handle_agent_event_side_effects(&event, &public).await;
        }

        if is_agent_end {
            let notify = {
                let mut inner = self.lock_inner();
                inner.processed_agent_ends = inner.processed_agent_ends.saturating_add(1);
                Arc::clone(&inner.agent_end_notify)
            };
            notify.notify_waiters();
        }
    }

    pub(super) fn processed_agent_end_count(&self) -> u64 {
        self.lock_inner().processed_agent_ends
    }

    pub(super) async fn wait_for_processed_agent_end(&self, before: u64) -> bool {
        loop {
            let (notified, cancelled) = {
                let inner = self.lock_inner();
                if inner.processed_agent_ends > before {
                    return true;
                }
                (
                    Arc::clone(&inner.agent_end_notify).notified_owned(),
                    inner.agent_end_wait_cancel.clone(),
                )
            };
            tokio::select! {
                () = notified => {}
                () = cancelled.cancelled() => return false,
            }
        }
    }

    /// Map a core agent event into a public session event.
    fn map_agent_event_for_public(&self, event: AgentEvent) -> AgentSessionEvent {
        let will_retry = match &event {
            AgentEvent::AgentEnd { messages } => self.will_retry_after_agent_end(messages),
            _ => false,
        };
        AgentSessionEvent::from_agent_event(event, will_retry)
    }

    /// Whether auto-retry will continue after this `agent_end`.
    ///
    /// Full retry policy lives in `retry.rs` (sibling). Foundation uses a
    /// conservative default: never claim `will_retry` until retry module wires
    /// settings. Sibling modules replace this via the shared inner state.
    pub(super) fn will_retry_after_agent_end(&self, messages: &[pi_agent::AgentMessage]) -> bool {
        let inner = self.lock_inner();
        if !inner.auto_retry_enabled || inner.retry_attempt >= inner.max_retries {
            return false;
        }
        for message in messages.iter().rev() {
            if message.role() == "assistant" {
                if let Some(pi_ai::Message::Assistant(assistant)) = message.as_llm() {
                    return is_retryable_assistant(assistant);
                }
                return false;
            }
        }
        false
    }

    /// Emit `agent_settled` exactly once for the current session-level run.
    ///
    /// Callers (prompt lifecycle) invoke this after retries/follow-ups complete.
    /// Extension handler is awaited before public listeners.
    pub async fn emit_agent_settled(self: &Arc<Self>) {
        {
            let mut inner = self.lock_inner();
            if !inner.is_agent_run_active {
                // Already settled; do not double-emit.
                return;
            }
            inner.is_agent_run_active = false;
        }

        let runner = self.hooks.runner();
        let _ = runner.emit(AgentSessionEvent::AgentSettled).await;
        self.emit_public(AgentSessionEvent::AgentSettled);
        self.resolve_idle_waiters();
    }

    /// Mark the session-level run active (prompt start).
    pub(super) fn mark_agent_run_active(&self) {
        let mut inner = self.lock_inner();
        inner.is_agent_run_active = true;
    }

    /// Resolve waiters blocked in `wait_for_idle` when session is idle.
    pub(super) fn resolve_idle_waiters(&self) {
        let inner = self.lock_inner();
        if inner.is_agent_run_active {
            return;
        }
        inner.idle_notify.notify_waiters();
    }
}

fn is_retryable_assistant(message: &pi_ai::AssistantMessage) -> bool {
    if message.stop_reason != pi_ai::StopReason::Error {
        return false;
    }
    let Some(err) = message.error_message.as_deref() else {
        return false;
    };
    let lower = err.to_ascii_lowercase();
    if lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("context overflow")
        || lower.contains("context length")
        || lower.contains("maximum context")
    {
        return false;
    }
    lower.contains("overloaded")
        || lower.contains("rate_limit")
        || lower.contains("rate limit")
        || lower.contains("network")
        || lower.contains("timeout")
        || lower.contains("server error")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("500")
        || lower.contains("retry your request")
        || lower.contains("try your request again")
        || lower.contains("network connection lost")
}
