//! Synchronous non-blocking agent event fan-out.
//!
//! The agent loop and provider drain never await presentation or extension
//! consumers. Lossless subscribers (session, RPC, interactive) receive every
//! event. Each extension gets a bounded queue of capacity
//! [`EXTENSION_EVENT_CAPACITY`]; overflow disconnects only that extension with
//! exactly one [`ExtensionEvent::Lagged`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::event::AgentEvent;
use crate::state::AgentState;

/// Per-extension event queue capacity.
///
/// Matches the plan and verification check 14 isolation budget.
pub const EXTENSION_EVENT_CAPACITY: usize = 64;

/// Synchronous event consumer used by the agent loop.
///
/// Implementations must not await. Fan-out is non-blocking so a lagging
/// subscriber cannot stall provider reads or the loop.
pub trait EventSink: Send + Sync {
    /// Publishes one event to state and all live subscribers.
    fn emit(&self, event: AgentEvent);
}

/// Event delivered to an extension subscription.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionEvent {
    /// A normal agent event.
    Event(Box<AgentEvent>),
    /// The extension fell behind and was disconnected.
    ///
    /// Delivered exactly once after any buffered events have been drained, and
    /// only when the subscription was closed due to overflow.
    Lagged,
}

struct ExtensionSlot {
    tx: Option<mpsc::Sender<AgentEvent>>,
    lagged: Arc<AtomicBool>,
}

struct AgentEventSinkInner {
    lossless: Vec<mpsc::UnboundedSender<AgentEvent>>,
    extensions: Vec<ExtensionSlot>,
}

/// Fan-out sink with lossless unbounded subscribers and bounded extension queues.
///
/// `emit` reduces agent state first, then publishes:
/// - lossless: `UnboundedSender` (never drops while the receiver is alive)
/// - extensions: bounded `try_send`; on `Full`, that slot is disconnected and
///   flagged lagged without affecting other subscribers
pub struct AgentEventSink {
    state: Arc<Mutex<AgentState>>,
    inner: Mutex<AgentEventSinkInner>,
}

impl AgentEventSink {
    /// Creates a sink that reduces into the shared agent state on every emit.
    #[must_use]
    pub fn new(state: Arc<Mutex<AgentState>>) -> Self {
        Self {
            state,
            inner: Mutex::new(AgentEventSinkInner {
                lossless: Vec::new(),
                extensions: Vec::new(),
            }),
        }
    }

    /// Returns a shared handle to the agent state reduced by this sink.
    #[must_use]
    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        Arc::clone(&self.state)
    }

    /// Subscribes a lossless consumer that never drops events.
    ///
    /// Used by session persistence, RPC, and interactive transcript paths.
    #[must_use]
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.lossless.push(tx);
        rx
    }

    /// Subscribes an extension with the default capacity of
    /// [`EXTENSION_EVENT_CAPACITY`].
    #[must_use]
    pub fn subscribe_extension(&self) -> ExtensionSubscription {
        self.subscribe_extension_with_capacity(EXTENSION_EVENT_CAPACITY)
    }

    /// Subscribes an extension with an explicit queue capacity.
    ///
    /// Capacity is clamped to at least 1 so the channel can accept events.
    #[must_use]
    pub fn subscribe_extension_with_capacity(&self, capacity: usize) -> ExtensionSubscription {
        let capacity = capacity.max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let lagged = Arc::new(AtomicBool::new(false));
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.extensions.push(ExtensionSlot {
            tx: Some(tx),
            lagged: Arc::clone(&lagged),
        });
        ExtensionSubscription {
            rx,
            lagged,
            emitted_lag: false,
        }
    }
}

impl EventSink for AgentEventSink {
    fn emit(&self, event: AgentEvent) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reduce(&event);
        }

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Drop closed lossless senders and deliver to the rest.
        inner.lossless.retain(|tx| tx.send(event.clone()).is_ok());

        for slot in &mut inner.extensions {
            let Some(tx) = slot.tx.as_ref() else {
                continue;
            };
            match tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    // Overflow: disconnect only this extension. Buffered items
                    // remain readable; ExtensionSubscription then yields Lagged.
                    slot.lagged.store(true, Ordering::SeqCst);
                    slot.tx = None;
                }
                Err(TrySendError::Closed(_)) => {
                    slot.tx = None;
                }
            }
        }
    }
}

/// Bounded extension event subscription.
///
/// `recv` yields buffered [`ExtensionEvent::Event`] values until the sender is
/// dropped. If the sender was dropped due to overflow, the next `recv` after
/// the buffer is empty returns exactly one [`ExtensionEvent::Lagged`], then
/// `None` forever.
pub struct ExtensionSubscription {
    rx: mpsc::Receiver<AgentEvent>,
    lagged: Arc<AtomicBool>,
    emitted_lag: bool,
}

impl ExtensionSubscription {
    /// Receives the next extension event.
    ///
    /// Returns `None` after the subscription has ended (and after delivering
    /// the single lag signal when applicable).
    pub async fn recv(&mut self) -> Option<ExtensionEvent> {
        match self.rx.recv().await {
            Some(event) => Some(ExtensionEvent::Event(Box::new(event))),
            None => {
                if !self.emitted_lag && self.lagged.load(Ordering::SeqCst) {
                    self.emitted_lag = true;
                    Some(ExtensionEvent::Lagged)
                } else {
                    None
                }
            }
        }
    }

    /// Non-blocking receive used by tests and polled consumers.
    ///
    /// # Errors
    ///
    /// Returns [`mpsc::error::TryRecvError::Empty`] when no event is ready, or
    /// [`mpsc::error::TryRecvError::Disconnected`] after the subscription ends
    /// (and after the single lag signal has already been delivered).
    pub fn try_recv(&mut self) -> Result<ExtensionEvent, mpsc::error::TryRecvError> {
        match self.rx.try_recv() {
            Ok(event) => Ok(ExtensionEvent::Event(Box::new(event))),
            Err(mpsc::error::TryRecvError::Empty) => Err(mpsc::error::TryRecvError::Empty),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if !self.emitted_lag && self.lagged.load(Ordering::SeqCst) {
                    self.emitted_lag = true;
                    Ok(ExtensionEvent::Lagged)
                } else {
                    Err(mpsc::error::TryRecvError::Disconnected)
                }
            }
        }
    }

    /// Returns true when this subscription was disconnected due to overflow.
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        self.lagged.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::user_text;
    use crate::state::AgentState;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn msg(n: usize) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: user_text(format!("m{n}"), std::iter::empty()),
        }
    }

    fn start_event(text: &str) -> AgentEvent {
        AgentEvent::MessageStart {
            message: user_text(text, std::iter::empty()),
        }
    }

    fn user_text_content(message: &crate::message::AgentMessage) -> Result<String, String> {
        match message.as_llm() {
            Some(pi_ai::Message::User(user)) => match &user.content {
                pi_ai::UserMessageContent::Text(text) => Ok(text.clone()),
                other @ pi_ai::UserMessageContent::Blocks(_) => {
                    Err(format!("expected text user content, got {other:?}"))
                }
            },
            other => Err(format!("expected user message, got {other:?}")),
        }
    }

    fn lock_state(
        state: &Mutex<AgentState>,
    ) -> Result<std::sync::MutexGuard<'_, AgentState>, String> {
        state
            .lock()
            .map_err(|_| "agent state mutex poisoned".to_owned())
    }

    #[tokio::test]
    async fn emit_reduces_state_before_fan_out() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(Arc::clone(&state));
        let mut rx = sink.subscribe();

        sink.emit(start_event("hello"));

        {
            let guard = lock_state(&state)?;
            assert!(guard.streaming_message.is_some());
            assert_eq!(
                guard
                    .streaming_message
                    .as_ref()
                    .map(crate::message::AgentMessage::role),
                Some("user")
            );
        }

        let Some(event) = rx.recv().await else {
            return Err("expected event".into());
        };
        assert!(matches!(event, AgentEvent::MessageStart { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn lossless_subscribers_receive_ordered_events() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let mut a = sink.subscribe();
        let mut b = sink.subscribe();

        for i in 0..5 {
            sink.emit(msg(i));
        }

        for i in 0..5 {
            let Some(event_a) = a.recv().await else {
                return Err("subscriber a closed early".into());
            };
            match event_a {
                AgentEvent::MessageEnd { message } => {
                    assert_eq!(user_text_content(&message)?, format!("m{i}"));
                }
                other => return Err(format!("unexpected a event: {other:?}").into()),
            }

            let Some(event_b) = b.recv().await else {
                return Err("subscriber b closed early".into());
            };
            match event_b {
                AgentEvent::MessageEnd { message } => {
                    assert_eq!(user_text_content(&message)?, format!("m{i}"));
                }
                other => return Err(format!("unexpected b event: {other:?}").into()),
            }
        }

        let snapshot = lock_state(&sink.state)?.snapshot();
        assert_eq!(snapshot.messages.len(), 5);
        for (i, message) in snapshot.messages.iter().enumerate() {
            assert_eq!(user_text_content(message)?, format!("m{i}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn lossless_never_drops_under_burst() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let mut rx = sink.subscribe();

        for i in 0..200 {
            sink.emit(msg(i));
        }

        let mut count = 0usize;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::MessageEnd { .. } => count += 1,
                other => return Err(format!("unexpected event: {other:?}").into()),
            }
        }
        assert_eq!(count, 200);
        Ok(())
    }

    #[tokio::test]
    async fn extension_overflow_yields_exactly_one_lagged() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let mut ext = sink.subscribe_extension_with_capacity(EXTENSION_EVENT_CAPACITY);
        let mut lossless = sink.subscribe();

        // Fill the extension queue without reading.
        for i in 0..EXTENSION_EVENT_CAPACITY {
            sink.emit(msg(i));
        }
        // One more triggers Full -> disconnect this extension only.
        sink.emit(msg(EXTENSION_EVENT_CAPACITY));

        assert!(ext.is_lagged());

        // Drain buffered items (capacity worth), then exactly one Lagged.
        let mut events = 0usize;
        let mut lagged = 0usize;
        loop {
            match ext.recv().await {
                Some(ExtensionEvent::Event(_)) => events += 1,
                Some(ExtensionEvent::Lagged) => lagged += 1,
                None => break,
            }
        }
        assert_eq!(events, EXTENSION_EVENT_CAPACITY);
        assert_eq!(lagged, 1);
        // Subsequent recv stays closed.
        assert!(ext.recv().await.is_none());

        // Lossless received every event including the overflowing one.
        let mut lossless_count = 0usize;
        while lossless.try_recv().is_ok() {
            lossless_count += 1;
        }
        assert_eq!(lossless_count, EXTENSION_EVENT_CAPACITY + 1);
        Ok(())
    }

    #[tokio::test]
    async fn hung_extension_does_not_affect_other_extension() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let mut hung = sink.subscribe_extension_with_capacity(2);
        let mut healthy = sink.subscribe_extension_with_capacity(8);
        let mut lossless = sink.subscribe();

        // Overflow the hung extension (capacity 2).
        sink.emit(msg(0));
        sink.emit(msg(1));
        sink.emit(msg(2)); // Full for hung

        assert!(hung.is_lagged());
        assert!(!healthy.is_lagged());

        // Further events still reach healthy + lossless.
        sink.emit(msg(3));
        sink.emit(msg(4));

        // Healthy gets all 5 events (no lag).
        let mut healthy_events = Vec::new();
        for _ in 0..5 {
            match healthy.recv().await {
                Some(ExtensionEvent::Event(event)) => healthy_events.push(*event),
                Some(ExtensionEvent::Lagged) => {
                    return Err("healthy extension should not lag".into());
                }
                None => return Err("healthy extension closed early".into()),
            }
        }
        assert_eq!(healthy_events.len(), 5);
        assert!(!healthy.is_lagged());

        // Hung: 2 buffered + Lagged.
        let mut hung_events = 0usize;
        let mut hung_lagged = 0usize;
        loop {
            match hung.recv().await {
                Some(ExtensionEvent::Event(_)) => hung_events += 1,
                Some(ExtensionEvent::Lagged) => hung_lagged += 1,
                None => break,
            }
        }
        assert_eq!(hung_events, 2);
        assert_eq!(hung_lagged, 1);

        let mut lossless_count = 0usize;
        while lossless.try_recv().is_ok() {
            lossless_count += 1;
        }
        assert_eq!(lossless_count, 5);
        Ok(())
    }

    #[tokio::test]
    async fn emit_never_blocks_on_full_extension() -> TestResult {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let _hung = sink.subscribe_extension_with_capacity(1);

        // If emit awaited, this would hang. It must return immediately.
        let emit = tokio::spawn(async move {
            for i in 0..32 {
                sink.emit(msg(i));
            }
        });

        match tokio::time::timeout(Duration::from_secs(1), emit).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(join_err)) => Err(format!("emit task failed: {join_err}").into()),
            Err(_) => Err("emit timed out — fan-out must be non-blocking".into()),
        }
    }

    #[tokio::test]
    async fn closed_lossless_receiver_is_pruned() {
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let rx = sink.subscribe();
        drop(rx);

        // Must not panic when sending to a dropped receiver.
        sink.emit(msg(0));
        sink.emit(msg(1));
    }

    #[test]
    fn default_extension_capacity_is_64() {
        assert_eq!(EXTENSION_EVENT_CAPACITY, 64);
    }
}
