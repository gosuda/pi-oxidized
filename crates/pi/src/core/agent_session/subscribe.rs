//! Bounded agent-event pump.
//!
//! Spawns exactly one task that drains `Agent::subscribe()`. Any observable
//! subscription lag becomes a typed session failure and aborts/settles the run
//! rather than persisting a partial lifecycle. For each retained event it:
//! 1. runs pre-public side effects (`message_start` queue dequeue)
//! 2. awaits the extension handler (compact deltas for streaming updates)
//! 3. applies `message_end` replacement to agent state when present
//! 4. emits the public `AgentSessionEvent` (listeners called without holding locks)
//! 5. persists `message_end` / emits auto-retry success
//!
//! Session-level `agent_settled` is emitted exactly once when the session run
//! flag flips false (after retries / follow-ups / auto-compaction). The same
//! guarded entry point also unblocks waiters after persistence or pump failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_agent::{AgentEvent, AgentEventSubscription};
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
    /// Spawn the single bounded event pump for this session.
    pub(super) fn spawn_event_pump(self: &Arc<Self>) -> EventPump {
        self.spawn_event_pump_with_subscription(self.agent.subscribe())
    }

    fn spawn_event_pump_with_subscription(
        self: &Arc<Self>,
        mut rx: AgentEventSubscription,
    ) -> EventPump {
        let cancel = CancellationToken::new();
        let active = Arc::new(AtomicBool::new(true));
        let session = Arc::clone(self);
        let cancel_child = cancel.clone();
        let active_flag = Arc::clone(&active);
        let wait_cancel = self.lock_inner().agent_end_wait_cancel.clone();

        let join = tokio::spawn(async move {
            let mut lag_recorded = false;
            loop {
                tokio::select! {
                    () = cancel_child.cancelled() => break,
                    event = rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        if rx.is_lagged() && !lag_recorded {
                            // Record once and abort, but keep draining: the
                            // bounded subscription retains the newest
                            // message_end/agent_end, and only a processed
                            // agent_end may drive the single settle. If the
                            // subscription closes instead, the loop breaks and
                            // wait_cancel unblocks the prompt barrier.
                            lag_recorded = true;
                            session.record_session_error(
                                crate::core::sessions::SessionError::Io {
                                    path: "agent event subscription".to_owned(),
                                    source: std::io::Error::other(
                                        "agent event subscription lagged; run lifecycle is incomplete",
                                    ),
                                },
                            );
                            session.agent.abort();
                        }
                        session.process_agent_event(event).await;
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
        let is_turn_end = matches!(&event, AgentEvent::TurnEnd { .. });
        if matches!(&event, AgentEvent::AgentStart) {
            // New persistence epoch: reset the TurnEnd counters before any
            // handler work so terminal markers from a previous run can never
            // release a later run.
            let mut inner = self.lock_inner();
            inner.persistence_epoch = inner.persistence_epoch.wrapping_add(1);
            inner.processed_turn_ends = 0;
            inner.claimed_turn_ends = 0;
        }

        if matches!(&event, AgentEvent::MessageStart { message } if message.role() == "user")
            && let Err(error) = self
                .handle_agent_event_side_effects(&event, &AgentSessionEvent::AgentStart)
                .await
        {
            // Record the typed failure and abort the run, but never settle
            // here: the aborted run still emits `agent_end`, and the prompt
            // lifecycle settles exactly once after processing it.
            self.record_session_error(error);
            self.agent.abort();
            return;
        }

        let (public, persistence_event) = match event {
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                let runner = self.hooks.runner();
                if runner.has_handlers("message_update")
                    && let Err(error) = runner
                        .emit_message_update_delta(assistant_message_event.as_ref())
                        .await
                {
                    runner.emit_error(error.to_string());
                }
                (
                    AgentSessionEvent::MessageUpdate {
                        message,
                        assistant_message_event,
                    },
                    None,
                )
            }
            AgentEvent::MessageEnd { message } => {
                let runner = self.hooks.runner();
                let replacement = if runner.has_handlers("message_end") {
                    match runner.emit_message_end(message.clone()).await {
                        Ok(replacement) => replacement,
                        Err(error) => {
                            runner.emit_error(error.to_string());
                            None
                        }
                    }
                } else {
                    None
                };
                let public_message = replacement.map_or_else(
                    || message.clone(),
                    |replacement| {
                        self.apply_message_end_replacement(message.clone(), Some(replacement))
                    },
                );
                (
                    AgentSessionEvent::MessageEnd {
                        message: public_message,
                    },
                    Some(AgentEvent::MessageEnd { message }),
                )
            }
            event => {
                let public = self.map_agent_event_for_public(event);
                let runner = self.hooks.runner();
                if runner.has_handlers(public.type_name())
                    && let Err(error) = runner.emit(public.clone()).await
                {
                    runner.emit_error(error.to_string());
                }
                (public, None)
            }
        };

        self.emit_public_awaited(&public).await;

        if let Some(event) = persistence_event
            && let Err(error) = self.handle_agent_event_side_effects(&event, &public).await
        {
            // Same invariant as the message_start path: record + abort, and
            // let the eventual `agent_end` drive the single settle.
            self.record_session_error(error);
            self.agent.abort();
        }

        if is_turn_end {
            // The TurnEnd has now run its full extension → public →
            // persistence sequence: publish the terminal marker. `notify_one`
            // stores a permit when no waiter is registered yet, so a waiter
            // racing between its locked check and registration is still woken.
            let notify = {
                let mut inner = self.lock_inner();
                inner.processed_turn_ends = inner.processed_turn_ends.saturating_add(1);
                Arc::clone(&inner.turn_end_notify)
            };
            notify.notify_one();
        }

        if is_agent_end {
            let (agent_end_notify, turn_end_notify) = {
                let mut inner = self.lock_inner();
                // The run is over: discard this run's unclaimed terminal
                // markers so they can never release a later run.
                inner.claimed_turn_ends = inner.processed_turn_ends;
                inner.processed_agent_ends = inner.processed_agent_ends.saturating_add(1);
                (
                    Arc::clone(&inner.agent_end_notify),
                    Arc::clone(&inner.turn_end_notify),
                )
            };
            turn_end_notify.notify_one();
            agent_end_notify.notify_waiters();
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

    /// Cancellation-aware barrier: atomically claim one fully persisted
    /// `TurnEnd` of the current epoch, or register for the next one.
    ///
    /// The claim/registration happens under the inner lock so no wake is
    /// lost; the await then selects without holding any lock against the
    /// active run token and the pump wait-cancel (pump exit/disconnect).
    /// Returns `true` when a processed `TurnEnd` was claimed.
    pub(super) async fn claim_processed_turn_end(&self, run_cancel: &CancellationToken) -> bool {
        loop {
            let (notified, pump_cancel) = {
                let mut inner = self.lock_inner();
                if inner.processed_turn_ends > inner.claimed_turn_ends {
                    inner.claimed_turn_ends += 1;
                    return true;
                }
                (
                    Arc::clone(&inner.turn_end_notify).notified_owned(),
                    inner.agent_end_wait_cancel.clone(),
                )
            };
            tokio::select! {
                biased;
                () = run_cancel.cancelled() => return false,
                () = pump_cancel.cancelled() => return false,
                () = notified => {}
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
    /// Uses the same retry.rs classifier and runtime attempt/settings gates as
    /// actual retry execution so public prediction cannot drift.
    pub(super) fn will_retry_after_agent_end(&self, messages: &[pi_agent::AgentMessage]) -> bool {
        let inner = self.lock_inner();
        if !inner.auto_retry_enabled || inner.retry_attempt >= inner.max_retries {
            return false;
        }
        for message in messages.iter().rev() {
            if message.role() == "assistant" {
                if let Some(pi_ai::Message::Assistant(assistant)) = message.as_llm() {
                    return self.is_retryable_error(assistant);
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
        self.emit_public_awaited(&AgentSessionEvent::AgentSettled)
            .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_agent::{AgentEventSink, AgentState, EventSink};
    use pi_ai::{
        AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider, ProviderError,
        StreamOptions,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

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

    fn model() -> Model {
        Model {
            id: "model".into(),
            name: "model".into(),
            api: "test".into(),
            provider: "test".into(),
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

    fn assistant_message() -> pi_agent::AgentMessage {
        let mut assistant = pi_ai::AssistantMessage::new("test", "test", "model", 0);
        assistant.stop_reason = pi_ai::StopReason::Stop;
        pi_agent::AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(assistant)))
    }

    fn update_event(index: i64) -> AgentEvent {
        let partial = pi_ai::AssistantMessage::new("test", "test", "model", index);
        AgentEvent::MessageUpdate {
            message: Arc::new(assistant_message()),
            assistant_message_event: Box::new(AssistantMessageEvent::Start {
                partial: Arc::new(partial),
            }),
        }
    }

    #[tokio::test]
    async fn lagged_subscription_drains_retained_terminals_before_settle() -> TestResult {
        let config =
            super::super::AgentSessionConfig::test_config(Arc::new(StubProvider), model())?;
        let session = super::super::AgentSession::new(config)?;
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_clone = Arc::clone(&observed);
        let _unsub = session.subscribe(move |event| {
            observed_clone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event.type_name().to_owned());
        });

        // Mirror the prompt lifecycle: the run flag gates the single settle.
        session.mark_agent_run_active();
        let sink = AgentEventSink::new(Arc::new(std::sync::Mutex::new(AgentState::new())));
        let rx = sink.subscribe_with_capacity(2);
        let ends_before = session.processed_agent_end_count();
        // Overflow with streaming updates; the bounded subscription must keep
        // the newest message_end and agent_end deliverable.
        for index in 0..4 {
            sink.emit(update_event(index));
        }
        sink.emit(AgentEvent::MessageEnd {
            message: assistant_message(),
        });
        sink.emit(AgentEvent::AgentEnd {
            messages: Vec::new(),
        });
        drop(sink);

        let pump = session.spawn_event_pump_with_subscription(rx);
        pump.join.await?;

        let error = session
            .take_session_error()
            .ok_or("lag must record a typed session error")?;
        assert!(error.to_string().contains("subscription lagged"), "{error}");
        assert!(session.take_session_error().is_none(), "error is one-shot");
        assert_eq!(
            session.processed_agent_end_count(),
            ends_before + 1,
            "retained agent_end must still be processed after lag"
        );

        let snapshot = observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            snapshot.contains(&"message_end".to_owned()),
            "retained message_end must be published: {snapshot:?}"
        );
        assert!(
            snapshot.contains(&"agent_end".to_owned()),
            "retained agent_end must be published: {snapshot:?}"
        );
        assert!(
            !snapshot.contains(&"agent_settled".to_owned()),
            "the pump must never settle on lag: {snapshot:?}"
        );

        // Settle exactly once, strictly after the processed agent_end —
        // mirroring the prompt lifecycle that owns the settle.
        session.emit_agent_settled().await;
        let snapshot = observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let end = snapshot
            .iter()
            .position(|name| name == "agent_end")
            .ok_or("agent_end position")?;
        let settled = snapshot
            .iter()
            .position(|name| name == "agent_settled")
            .ok_or("agent_settled position")?;
        assert!(end < settled, "AgentEnd must precede settle: {snapshot:?}");
        assert_eq!(
            snapshot
                .iter()
                .filter(|name| *name == "agent_settled")
                .count(),
            1,
            "exactly one settle: {snapshot:?}"
        );
        Ok(())
    }

    fn scripted_user(text: &str) -> pi_agent::AgentMessage {
        pi_agent::user_text(text, std::iter::empty())
    }

    fn turn_end_event() -> AgentEvent {
        AgentEvent::TurnEnd {
            message: assistant_message(),
            tool_results: Vec::new(),
        }
    }

    /// Drive one scripted run through the real pump: `AgentStart`, a user
    /// message pair, an assistant message pair, and a `TurnEnd` (plus
    /// `AgentEnd` when `with_end` is set).
    fn emit_scripted_run(sink: &AgentEventSink, with_end: bool) {
        sink.emit(AgentEvent::AgentStart);
        sink.emit(AgentEvent::MessageStart {
            message: scripted_user("hi"),
        });
        sink.emit(AgentEvent::MessageEnd {
            message: scripted_user("hi"),
        });
        sink.emit(AgentEvent::MessageStart {
            message: assistant_message(),
        });
        sink.emit(AgentEvent::MessageEnd {
            message: assistant_message(),
        });
        sink.emit(turn_end_event());
        if with_end {
            sink.emit(AgentEvent::AgentEnd {
                messages: Vec::new(),
            });
        }
    }

    #[tokio::test]
    async fn turn_end_marker_released_only_after_full_pump_handling() -> TestResult {
        use std::time::Duration;
        use tokio::time::timeout;

        let config =
            super::super::AgentSessionConfig::test_config(Arc::new(StubProvider), model())?;
        let session = super::super::AgentSession::new(config)?;
        session.mark_agent_run_active();

        // Snapshot the terminal marker state at the moment the public
        // `turn_end` reaches listeners: the marker must still be unpublished
        // because the pump increments only after the full extension → public
        // → persistence sequence.
        let marker_at_public = Arc::new(std::sync::Mutex::new(None::<u64>));
        let marker_observer = Arc::clone(&marker_at_public);
        let session_for_marker = Arc::clone(&session);
        let _unsub = session.subscribe(move |event| {
            if matches!(event, AgentSessionEvent::TurnEnd { .. }) {
                let marker = session_for_marker.lock_inner().processed_turn_ends;
                *marker_observer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(marker);
            }
        });

        let sink = AgentEventSink::new(Arc::new(std::sync::Mutex::new(AgentState::new())));
        let rx = sink.subscribe_with_capacity(16);
        emit_scripted_run(&sink, false);

        let pump = session.spawn_event_pump_with_subscription(rx);
        let claimed = timeout(
            Duration::from_secs(2),
            session.claim_processed_turn_end(&CancellationToken::new()),
        )
        .await?;
        assert!(
            claimed,
            "processed TurnEnd must be claimable after the pump"
        );

        let at_public = marker_at_public
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            at_public,
            Some(0),
            "marker must be unpublished while the public turn_end is in flight"
        );
        {
            let inner = session.lock_inner();
            assert_eq!(inner.processed_turn_ends, 1);
            assert_eq!(inner.claimed_turn_ends, 1);
            assert_eq!(inner.persistence_epoch, 1);
        }

        drop(sink);
        pump.join.await?;
        Ok(())
    }

    #[tokio::test]
    async fn stale_terminal_turn_end_does_not_release_later_run() -> TestResult {
        use std::time::Duration;
        use tokio::time::timeout;

        let config =
            super::super::AgentSessionConfig::test_config(Arc::new(StubProvider), model())?;
        let session = super::super::AgentSession::new(config)?;
        session.mark_agent_run_active();

        let sink = AgentEventSink::new(Arc::new(std::sync::Mutex::new(AgentState::new())));
        let rx = sink.subscribe_with_capacity(32);
        let pump = session.spawn_event_pump_with_subscription(rx);

        // Run one: fully processed, terminal marker left unclaimed.
        let ends_before = session.processed_agent_end_count();
        emit_scripted_run(&sink, true);
        assert!(
            timeout(
                Duration::from_secs(2),
                session.wait_for_processed_agent_end(ends_before),
            )
            .await?
        );
        {
            let inner = session.lock_inner();
            assert_eq!(
                inner.claimed_turn_ends, inner.processed_turn_ends,
                "agent_end must discard unclaimed terminal markers"
            );
        }

        // The stale marker from run one must not satisfy a claim.
        let stale_claim = timeout(
            Duration::from_millis(150),
            session.claim_processed_turn_end(&CancellationToken::new()),
        )
        .await;
        assert!(
            stale_claim.is_err(),
            "stale terminal marker must not release a claim"
        );

        // Run two: a fresh epoch; its own TurnEnd releases the claim.
        emit_scripted_run(&sink, false);
        let claimed = timeout(
            Duration::from_secs(2),
            session.claim_processed_turn_end(&CancellationToken::new()),
        )
        .await?;
        assert!(
            claimed,
            "run two's persisted TurnEnd must release the claim"
        );
        {
            let inner = session.lock_inner();
            assert_eq!(inner.persistence_epoch, 2);
            assert_eq!(inner.processed_turn_ends, 1);
            assert_eq!(inner.claimed_turn_ends, 1);
        }

        drop(sink);
        pump.join.await?;
        Ok(())
    }
}
