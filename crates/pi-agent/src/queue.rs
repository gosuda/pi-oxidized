//! Steering and follow-up message queues with live drain modes.

use serde::{Deserialize, Serialize};

use crate::message::AgentMessage;

/// Controls how many queued messages are released at a drain point.
///
/// The mode is read at drain time, not enqueue time, so live changes take
/// effect on the next drain.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum QueueMode {
    /// Drain and inject every queued message at the drain point.
    #[serde(rename = "all")]
    All,
    /// Drain and inject only the oldest queued message.
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

/// FIFO queue used for steering and follow-up injection.
///
/// Mirrors the TypeScript `PendingMessageQueue`: messages are enqueued in order
/// and drained according to the current [`QueueMode`].
#[derive(Clone, Debug, Default)]
pub struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    /// Drain policy consulted on every [`PendingMessageQueue::drain`] call.
    pub mode: QueueMode,
}

impl PendingMessageQueue {
    /// Creates an empty queue with the given drain mode.
    #[must_use]
    pub const fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    /// Appends a message to the tail of the queue.
    pub fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    /// Returns true when at least one message is queued.
    #[must_use]
    pub fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Returns the number of queued messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns true when the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns the current drain mode.
    #[must_use]
    pub const fn mode(&self) -> QueueMode {
        self.mode
    }

    /// Replaces the drain mode. The new mode is used on the next drain.
    pub const fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }

    /// Drains messages according to the live [`QueueMode`].
    ///
    /// - [`QueueMode::All`]: removes and returns every queued message.
    /// - [`QueueMode::OneAtATime`]: removes and returns only the oldest message.
    #[must_use]
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => std::mem::take(&mut self.messages),
            QueueMode::OneAtATime => {
                if self.messages.is_empty() {
                    Vec::new()
                } else {
                    let first = self.messages.remove(0);
                    vec![first]
                }
            }
        }
    }

    /// Drops every queued message without returning them.
    pub fn clear(&mut self) {
        self.messages.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::user_text;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn msg(text: &str) -> AgentMessage {
        user_text(text, std::iter::empty())
    }

    #[test]
    fn queue_mode_serde_names() -> TestResult {
        assert_eq!(serde_json::to_string(&QueueMode::All)?, "\"all\"");
        assert_eq!(
            serde_json::to_string(&QueueMode::OneAtATime)?,
            "\"one-at-a-time\""
        );
        assert_eq!(
            serde_json::from_str::<QueueMode>("\"all\"")?,
            QueueMode::All
        );
        assert_eq!(
            serde_json::from_str::<QueueMode>("\"one-at-a-time\"")?,
            QueueMode::OneAtATime
        );
        Ok(())
    }

    #[test]
    fn default_mode_is_one_at_a_time() {
        assert_eq!(QueueMode::default(), QueueMode::OneAtATime);
        assert_eq!(PendingMessageQueue::default().mode, QueueMode::OneAtATime);
    }

    #[test]
    fn drain_all_returns_every_message_in_order() {
        let mut queue = PendingMessageQueue::new(QueueMode::All);
        queue.enqueue(msg("a"));
        queue.enqueue(msg("b"));
        queue.enqueue(msg("c"));

        let drained = queue.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].role(), "user");
        assert!(!queue.has_items());
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn drain_one_at_a_time_returns_oldest_only() {
        let mut queue = PendingMessageQueue::new(QueueMode::OneAtATime);
        queue.enqueue(msg("first"));
        queue.enqueue(msg("second"));
        queue.enqueue(msg("third"));

        let first = queue.drain();
        assert_eq!(first.len(), 1);
        assert_eq!(queue.len(), 2);

        let second = queue.drain();
        assert_eq!(second.len(), 1);
        assert_eq!(queue.len(), 1);

        let third = queue.drain();
        assert_eq!(third.len(), 1);
        assert!(!queue.has_items());
    }

    #[test]
    fn live_mode_change_affects_next_drain() {
        let mut queue = PendingMessageQueue::new(QueueMode::All);
        queue.enqueue(msg("a"));
        queue.enqueue(msg("b"));
        queue.enqueue(msg("c"));

        // Switch to one-at-a-time before draining: only the oldest leaves.
        queue.set_mode(QueueMode::OneAtATime);
        let one = queue.drain();
        assert_eq!(one.len(), 1);
        assert_eq!(queue.len(), 2);

        // Switch back to all: remaining messages drain together.
        queue.mode = QueueMode::All;
        let rest = queue.drain();
        assert_eq!(rest.len(), 2);
        assert!(!queue.has_items());
    }

    #[test]
    fn mode_change_after_partial_one_at_a_time_drain() {
        let mut queue = PendingMessageQueue::new(QueueMode::OneAtATime);
        queue.enqueue(msg("1"));
        queue.enqueue(msg("2"));
        queue.enqueue(msg("3"));

        assert_eq!(queue.drain().len(), 1);
        queue.set_mode(QueueMode::All);
        let rest = queue.drain();
        assert_eq!(rest.len(), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn clear_drops_all_messages() {
        let mut queue = PendingMessageQueue::new(QueueMode::All);
        queue.enqueue(msg("x"));
        queue.enqueue(msg("y"));
        queue.clear();
        assert!(!queue.has_items());
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn empty_drain_is_empty_for_both_modes() {
        let mut all = PendingMessageQueue::new(QueueMode::All);
        let mut one = PendingMessageQueue::new(QueueMode::OneAtATime);
        assert!(all.drain().is_empty());
        assert!(one.drain().is_empty());
    }
}
