//! Synchronous non-blocking agent event fan-out.
//!
//! The agent loop and provider drain never await presentation or extension
//! consumers. Agent subscribers have bounded queues with observable lag and
//! terminal retention; extension overflow disconnects only that extension with
//! exactly one [`ExtensionEvent::Lagged`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc};

use crate::event::AgentEvent;
use crate::state::AgentState;

/// Default per-subscriber event queue capacity.
pub const AGENT_EVENT_CAPACITY: usize = 256;

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
    tx: Option<mpsc::Sender<Arc<AgentEvent>>>,
    lagged: Arc<AtomicBool>,
}

struct SubscriberState {
    queue: VecDeque<Arc<AgentEvent>>,
    closed: bool,
    lagged: bool,
}

struct SubscriberInner {
    capacity: usize,
    state: Mutex<SubscriberState>,
    notify: Notify,
}

impl SubscriberInner {
    fn push(&self, event: Arc<AgentEvent>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }

        if state.queue.len() == self.capacity {
            state.lagged = true;
            if let Some(index) = state
                .queue
                .iter()
                .position(|queued| matches!(queued.as_ref(), AgentEvent::MessageUpdate { .. }))
            {
                state.queue.remove(index);
            } else if is_run_terminal(event.as_ref()) {
                // At the hard bound, the newest terminal snapshot is more
                // useful than an older buffered event for rebuilding state.
                state.queue.pop_front();
            } else {
                return true;
            }
        }

        state.queue.push_back(event);
        drop(state);
        self.notify.notify_one();
        true
    }

    fn close_sender(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.notify.notify_one();
    }

    fn close_receiver(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.queue.clear();
        drop(state);
        self.notify.notify_one();
    }
}

fn is_run_terminal(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::MessageEnd { .. } | AgentEvent::AgentEnd { .. }
    )
}

fn unwrap_event(event: Arc<AgentEvent>) -> AgentEvent {
    Arc::try_unwrap(event).unwrap_or_else(|shared| shared.as_ref().clone())
}

struct AgentEventSinkInner {
    subscribers: Vec<Weak<SubscriberInner>>,
    extensions: Vec<ExtensionSlot>,
}

/// Fan-out sink with bounded subscribers and bounded extension queues.
///
/// `emit` reduces agent state first, then publishes one shared event allocation:
/// - subscribers: bounded queues; streaming updates are coalesced under lag and
///   the newest assistant/agent terminal is always retained
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
                subscribers: Vec::new(),
                extensions: Vec::new(),
            }),
        }
    }

    /// Returns a shared handle to the agent state reduced by this sink.
    #[must_use]
    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        Arc::clone(&self.state)
    }

    /// Subscribes a bounded consumer with [`AGENT_EVENT_CAPACITY`].
    ///
    /// Ordered delivery is lossless while the receiver keeps pace. On overflow,
    /// streaming updates are coalesced and non-terminal events may be dropped;
    /// [`AgentEventSubscription::is_lagged`] reports that condition. The newest
    /// assistant and agent terminal events remain deliverable.
    #[must_use]
    pub fn subscribe(&self) -> AgentEventSubscription {
        self.subscribe_with_capacity(AGENT_EVENT_CAPACITY)
    }

    /// Subscribes a consumer with an explicit queue capacity.
    ///
    /// Capacity is clamped to at least 2 so both `message_end` and `agent_end`
    /// can remain buffered for a stalled consumer.
    #[must_use]
    pub fn subscribe_with_capacity(&self, capacity: usize) -> AgentEventSubscription {
        let capacity = capacity.max(2);
        let inner = Arc::new(SubscriberInner {
            capacity,
            state: Mutex::new(SubscriberState {
                queue: VecDeque::with_capacity(capacity),
                closed: false,
                lagged: false,
            }),
            notify: Notify::new(),
        });
        let mut sink = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sink.subscribers.push(Arc::downgrade(&inner));
        AgentEventSubscription { inner }
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
        let event = Arc::new(event);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reduce(event.as_ref());
        }

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        inner.subscribers.retain(|subscriber| {
            subscriber
                .upgrade()
                .is_some_and(|subscriber| subscriber.push(Arc::clone(&event)))
        });

        for slot in &mut inner.extensions {
            let Some(tx) = slot.tx.as_ref() else {
                continue;
            };
            match tx.try_send(Arc::clone(&event)) {
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

impl Drop for AgentEventSink {
    fn drop(&mut self) {
        let inner = self
            .inner
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for subscriber in &inner.subscribers {
            if let Some(subscriber) = subscriber.upgrade() {
                subscriber.close_sender();
            }
        }
    }
}

/// Bounded agent event subscription.
///
/// The queue remains at its configured hard bound. A stalled consumer may miss
/// intermediate updates, detectable through [`Self::is_lagged`], while the
/// newest `message_end` and `agent_end` remain deliverable.
pub struct AgentEventSubscription {
    inner: Arc<SubscriberInner>,
}

impl AgentEventSubscription {
    /// Receives the next retained event in source order.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        loop {
            let notified = self.inner.notify.notified();
            {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(event) = state.queue.pop_front() {
                    return Some(unwrap_event(event));
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Attempts to receive a retained event without waiting.
    pub fn try_recv(&mut self) -> Result<AgentEvent, mpsc::error::TryRecvError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(event) = state.queue.pop_front() {
            return Ok(unwrap_event(event));
        }
        if state.closed {
            Err(mpsc::error::TryRecvError::Disconnected)
        } else {
            Err(mpsc::error::TryRecvError::Empty)
        }
    }

    /// Returns true after any event was coalesced or dropped under backpressure.
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lagged
    }

    /// Returns the current number of buffered events.
    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .len()
    }
}

impl Drop for AgentEventSubscription {
    fn drop(&mut self) {
        self.inner.close_receiver();
    }
}

/// Bounded extension event subscription.
///
/// `recv` yields buffered [`ExtensionEvent::Event`] values until the sender is
/// dropped. If the sender was dropped due to overflow, the next `recv` after
/// the buffer is empty returns exactly one [`ExtensionEvent::Lagged`], then
/// `None` forever.
pub struct ExtensionSubscription {
    rx: mpsc::Receiver<Arc<AgentEvent>>,
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
            Some(event) => Some(ExtensionEvent::Event(Box::new(unwrap_event(event)))),
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
            Ok(event) => Ok(ExtensionEvent::Event(Box::new(unwrap_event(event)))),
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

    fn update(n: usize) -> AgentEvent {
        let assistant =
            pi_ai::AssistantMessage::new("api", "provider", format!("m{n}"), n as i64);
        AgentEvent::MessageUpdate {
            message: crate::message::AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(
                assistant.clone(),
            ))),
            assistant_message_event: Box::new(pi_ai::AssistantMessageEvent::Start {
                partial: assistant,
            }),
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
    async fn lagging_subscriber_stays_bounded_and_receives_terminals() -> TestResult {
        const CAPACITY: usize = 4;
        let state = Arc::new(Mutex::new(AgentState::new()));
        let sink = AgentEventSink::new(state);
        let mut rx = sink.subscribe_with_capacity(CAPACITY);

        for index in 0..100_000 {
            sink.emit(update(index));
            assert!(rx.queued_len() <= CAPACITY);
        }
        assert!(rx.is_lagged());

        sink.emit(msg(100_000));
        sink.emit(AgentEvent::AgentEnd {
            messages: Vec::new(),
        });
        assert_eq!(rx.queued_len(), CAPACITY);

        let mut retained = Vec::new();
        while let Ok(event) = rx.try_recv() {
            retained.push(event);
        }
        assert!(matches!(
            retained.get(retained.len().saturating_sub(2)),
            Some(AgentEvent::MessageEnd { .. })
        ));
        assert!(matches!(
            retained.last(),
            Some(AgentEvent::AgentEnd { .. })
        ));
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
