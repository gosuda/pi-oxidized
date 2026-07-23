//! Agent-event persistence and mirror-queue side effects.
//!
//! Ownership: the event pump (subscribe.rs) is the sole caller of these helpers
//! while holding no `SessionManager` lock across await. `SessionManager` is wrapped
//! in `tokio::sync::Mutex` on `AgentSessionInner` so only the pump (or a public
//! method that awaits the same mutex) mutates the append-only tree.
//!
//! Lock order (documented in `mod.rs`):
//! 1. `AgentSessionInner` fields under `std::sync::Mutex` (flags, mirrors)
//! 2. `session_manager: tokio::sync::Mutex<SessionManager>` (async, never
//!    held while holding the std mutex)
//! 3. `SessionHooks` `RwLock`s (runner / prompt / tools) — never nested with 1/2

use std::io;
use std::sync::Arc;

use pi_agent::{AgentEvent, AgentMessage};
use pi_ai::{Message, StopReason, UserContent, UserMessageContent};

use super::AgentSession;
use super::events::AgentSessionEvent;
use crate::core::messages::{CustomMessage, CustomMessageContent};
use crate::core::sessions::{SessionError, SessionManager};

impl AgentSession {
    /// Handle one agent event for mirror queues + persistence side effects.
    ///
    /// Ordering contract:
    /// - `message_start:user` dequeues mirror text and emits `queue_update`
    ///   BEFORE the public session event is emitted.
    /// - `message_end` persists after extension + public emit (caller order).
    /// - successful assistant `message_end` with `retry_attempt > 0` emits
    ///   `auto_retry_end{success:true}` and resets the counter.
    pub(super) async fn handle_agent_event_side_effects(
        &self,
        event: &AgentEvent,
        public_event: &AgentSessionEvent,
    ) -> Result<(), SessionError> {
        match event {
            AgentEvent::MessageStart { message } if message.role() == "user" => {
                self.on_user_message_start(message);
            }
            AgentEvent::MessageEnd { message } => {
                // Prefer the (possibly replacement-mutated) public event message.
                let message = match public_event {
                    AgentSessionEvent::MessageEnd { message } => message,
                    _ => message,
                };
                self.on_message_end(message).await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn on_user_message_start(&self, message: &AgentMessage) {
        let text = user_message_text(message);
        let mut inner = self.lock_inner();
        inner.overflow_recovery_attempted = false;
        if text.is_empty() {
            return;
        }
        if let Some(idx) = inner.steering_messages.iter().position(|m| m == &text) {
            inner.steering_messages.remove(idx);
            let steering = inner.steering_messages.clone();
            let follow_up = inner.follow_up_messages.clone();
            drop(inner);
            self.emit_public(AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            });
            return;
        }
        if let Some(idx) = inner.follow_up_messages.iter().position(|m| m == &text) {
            inner.follow_up_messages.remove(idx);
            let steering = inner.steering_messages.clone();
            let follow_up = inner.follow_up_messages.clone();
            drop(inner);
            self.emit_public(AgentSessionEvent::QueueUpdate {
                steering,
                follow_up,
            });
        }
    }

    async fn on_message_end(&self, message: &AgentMessage) -> Result<(), SessionError> {
        self.persist_message_end(message).await?;

        if message.role() != "assistant" {
            return Ok(());
        }

        let assistant = match message.as_llm() {
            Some(Message::Assistant(a)) => a.clone(),
            _ => return Ok(()),
        };

        let mut inner = self.lock_inner();
        inner.last_assistant_message = Some(assistant.clone());
        if assistant.stop_reason != StopReason::Error {
            inner.overflow_recovery_attempted = false;
        }
        if assistant.stop_reason != StopReason::Error && inner.retry_attempt > 0 {
            let attempt = inner.retry_attempt;
            inner.retry_attempt = 0;
            drop(inner);
            self.emit_public(AgentSessionEvent::AutoRetryEnd {
                success: true,
                attempt,
                final_error: None,
            });
        }
        Ok(())
    }

    async fn persist_message_end(&self, message: &AgentMessage) -> Result<(), SessionError> {
        if self.lock_inner().pending_session_error.is_some() {
            return Err(SessionError::Io {
                path: "session persistence".to_owned(),
                source: io::Error::other("session persistence is blocked by an earlier failure"),
            });
        }
        // Reserve the manager in event order before handing the synchronous
        // filesystem work to the blocking pool. The owned guard preserves the
        // append linearization point while keeping blocking I/O off Tokio workers.
        let mut sm = Arc::clone(&self.session_manager).lock_owned().await;
        let message = message.clone();
        let persisted_entry = tokio::task::spawn_blocking(move || {
            let id = match message.role() {
                "custom" => Some(persist_custom_message(&mut sm, &message)?),
                "user" | "assistant" | "toolResult" => Some(sm.append_message(&message)?),
                // bashExecution / compactionSummary / branchSummary persist elsewhere
                _ => None,
            };
            Ok::<_, SessionError>(id.and_then(|id| sm.get_entry(&id).cloned()))
        })
        .await
        .map_err(|err| SessionError::Io {
            path: "session persistence worker".to_owned(),
            source: io::Error::other(err),
        })??;

        if let Some(entry) = persisted_entry {
            self.emit_public(AgentSessionEvent::EntryAppended { entry });
        }
        Ok(())
    }

    /// Apply a `message_end` replacement to agent state + the event message value.
    ///
    /// Returns the message that should be used for public emit + persistence.
    pub(super) fn apply_message_end_replacement(
        &self,
        original: AgentMessage,
        replacement: Option<AgentMessage>,
    ) -> AgentMessage {
        let Some(mut replacement) = replacement else {
            return original;
        };

        // Normalize missing content on untyped `custom` replacements (TS guard).
        replacement = normalize_replacement(replacement);

        // Update live agent transcript when the tail is the assistant being replaced.
        if replacement.role() == "assistant"
            && let Some(Message::Assistant(assistant)) = replacement.as_llm().cloned()
        {
            let _ = self.agent.replace_last_assistant(assistant);
        } else if matches!(replacement.role(), "user" | "toolResult" | "custom") {
            // For non-assistant replacements, rewrite the last matching role if present.
            // Agent only exposes replace_last_assistant; other roles stay on the event
            // path + persistence. Live transcript for those is already finalized by
            // the agent loop before message_end, so we only ensure persistence sees
            // the replacement value (returned below).
        }

        replacement
    }
}

fn persist_custom_message(
    sm: &mut SessionManager,
    message: &AgentMessage,
) -> Result<String, SessionError> {
    let Some(custom) = parse_custom_agent_message(message) else {
        // Fall back to opaque append via message entry.
        return sm.append_message(message);
    };
    sm.append_custom_message_entry(
        &custom.custom_type,
        &custom.content,
        custom.display,
        custom.details.clone(),
    )
}

fn parse_custom_agent_message(message: &AgentMessage) -> Option<CustomMessage> {
    let AgentMessage::Custom(custom) = message else {
        return None;
    };
    if custom.role != "custom" {
        return None;
    }
    let value = serde_json::to_value(custom).ok()?;
    serde_json::from_value(value).ok()
}

fn normalize_replacement(message: AgentMessage) -> AgentMessage {
    // Product custom messages must carry a content key (upstream contract).
    let AgentMessage::Custom(custom) = &message else {
        return message;
    };
    if custom.role != "custom" || custom.payload.get("content").is_some() {
        return message;
    }
    let mut payload = custom.payload.clone();
    payload.insert(
        "content".to_owned(),
        serde_json::to_value(CustomMessageContent::Blocks(Vec::new()))
            .unwrap_or(Value::Array(Vec::new())),
    );
    AgentMessage::Custom(pi_agent::CustomAgentMessage::new("custom", payload))
}

/// Extract plain text from a user message (joined text blocks).
pub(super) fn user_message_text(message: &AgentMessage) -> String {
    match message.as_llm() {
        Some(Message::User(user)) => match &user.content {
            UserMessageContent::Text(text) => text.clone(),
            UserMessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    UserContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        },
        _ => String::new(),
    }
}

use serde_json::Value;
