//! Text renderer: drain an [`AgentSessionEvent`] stream to stdout/stderr text.
//!
//! Mirrors the text-mode branch of `.references/pi/packages/coding-agent/src/
//! modes/print-mode.ts`: the session runs to completion, then the final
//! assistant message's text blocks are written to stdout (one line each). An
//! `error` or `aborted` stop reason writes the failure text to stderr and
//! yields a nonzero exit code. Every other event variant (thinking deltas,
//! tool-execution lifecycle, compaction, retry, queue updates, …) is consumed
//! losslessly so fragmented streaming updates coalesce into the final message.
//!
//! The renderer is split into a synchronous, allocation-light state machine
//! ([`TextRenderer`]) and an async drain ([`render_text`]) so tests can feed
//! fragmented events directly and assert output without touching real I/O.

use std::io;

use futures::Stream;
use futures::StreamExt;
use pi_agent::AgentMessage;
use pi_ai::{AssistantMessage, Message, StopReason};

use super::PrintSink;
use crate::core::agent_session::AgentSessionEvent;

/// Outcome of draining an event stream through the text renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextOutcome {
    /// Process exit code (`0` on success, `1` on error / abort).
    pub exit_code: i32,
}

impl TextOutcome {
    /// Success exit code.
    pub const SUCCESS: i32 = 0;
    /// Failure exit code (error or aborted stop reason).
    pub const FAILURE: i32 = 1;
}

/// Stateful text-mode renderer.
///
/// Tracks the latest assistant message observed across the whole stream so that
/// multiple prompts, retries, and compaction rounds collapse to a single final
/// answer. No output is produced until [`finish`](Self::finish); intermediate
/// streaming events only update internal state.
#[derive(Default)]
pub struct TextRenderer {
    last_assistant: Option<AssistantMessage>,
}

impl TextRenderer {
    /// Create an empty renderer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one event.
    ///
    /// Every [`AgentSessionEvent`] variant is handled. Authoritative assistant
    /// snapshots come from turn-end, message-end, message-update, and agent-end
    /// so fragmented streaming and multi-turn runs coalesce into a single final
    /// answer. Other variants are consumed without side effects so the renderer
    /// never panics on thinking, tool, compaction, retry, or queue events.
    pub fn handle(&mut self, event: &AgentSessionEvent) {
        match event {
            AgentSessionEvent::TurnEnd { message, .. }
            | AgentSessionEvent::MessageStart { message }
            | AgentSessionEvent::MessageEnd { message } => {
                self.record_assistant(message);
            }
            AgentSessionEvent::MessageUpdate { message, .. } => {
                // `message` is `&Arc<AgentMessage>` here; deref coercion
                // yields `&AgentMessage` for `record_assistant`.
                self.record_assistant(message);
            }
            AgentSessionEvent::AgentEnd { messages, .. } => {
                for message in messages {
                    self.record_assistant(message);
                }
            }
            // Lifecycle / tool / session events update no text state.
            AgentSessionEvent::AgentStart
            | AgentSessionEvent::SessionBeforeSwitch { .. }
            | AgentSessionEvent::SessionBeforeFork { .. }
            | AgentSessionEvent::SessionStart { .. }
            | AgentSessionEvent::SessionShutdown { .. }
            | AgentSessionEvent::ModelSelect { .. }
            | AgentSessionEvent::TurnStart
            | AgentSessionEvent::ToolExecutionStart { .. }
            | AgentSessionEvent::ToolExecutionUpdate { .. }
            | AgentSessionEvent::ToolExecutionEnd { .. }
            | AgentSessionEvent::AgentSettled
            | AgentSessionEvent::QueueUpdate { .. }
            | AgentSessionEvent::CompactionStart { .. }
            | AgentSessionEvent::CompactionEnd { .. }
            | AgentSessionEvent::EntryAppended { .. }
            | AgentSessionEvent::SessionInfoChanged { .. }
            | AgentSessionEvent::ThinkingLevelChanged { .. }
            | AgentSessionEvent::AutoRetryStart { .. }
            | AgentSessionEvent::AutoRetryEnd { .. }
            | AgentSessionEvent::SummarizationRetryScheduled { .. }
            | AgentSessionEvent::SummarizationRetryAttemptStart { .. }
            | AgentSessionEvent::SummarizationRetryFinished
            | AgentSessionEvent::BashExecutionUpdate { .. } => {}
        }
    }

    fn record_assistant(&mut self, message: &AgentMessage) {
        if let Some(assistant) = assistant_of(message) {
            self.last_assistant = Some(assistant.clone());
        }
    }

    /// Borrow the tracked final assistant message, if any.
    #[must_use]
    pub fn last_assistant(&self) -> Option<&AssistantMessage> {
        self.last_assistant.as_ref()
    }

    /// Emit the final text (or error) and return the exit code.
    ///
    /// # Errors
    ///
    /// Propagates sink write failures.
    pub async fn finish<K>(&self, sink: &K) -> io::Result<i32>
    where
        K: PrintSink,
    {
        let Some(assistant) = self.last_assistant.as_ref() else {
            // No assistant message observed (empty run / no prompt).
            sink.flush().await?;
            return Ok(TextOutcome::SUCCESS);
        };

        match assistant.stop_reason {
            StopReason::Error | StopReason::Aborted => {
                let message = assistant.error_message.clone().unwrap_or_else(|| {
                    format!("Request {}", stop_reason_wire(assistant.stop_reason))
                });
                sink.write_stderr(&message).await?;
                sink.write_stderr("\n").await?;
                sink.flush().await?;
                Ok(TextOutcome::FAILURE)
            }
            StopReason::Stop | StopReason::Length | StopReason::ToolUse => {
                for content in &assistant.content {
                    if let pi_ai::AssistantContent::Text(text_block) = content {
                        sink.write_stdout(&text_block.text).await?;
                        sink.write_stdout("\n").await?;
                    }
                }
                sink.flush().await?;
                Ok(TextOutcome::SUCCESS)
            }
        }
    }
}

/// Drain `events` through a fresh [`TextRenderer`] and emit the final output.
///
/// # Errors
///
/// Propagates sink write failures from [`TextRenderer::finish`].
pub async fn render_text<S, K>(events: S, sink: &K) -> io::Result<i32>
where
    S: Stream<Item = AgentSessionEvent> + Send + Unpin,
    K: PrintSink,
{
    let mut renderer = TextRenderer::new();
    let mut events = events;
    while let Some(event) = events.next().await {
        renderer.handle(&event);
    }
    renderer.finish(sink).await
}

/// Return the assistant message inside a transcript message, if any.
fn assistant_of(message: &AgentMessage) -> Option<&AssistantMessage> {
    match message {
        AgentMessage::Llm(boxed) => match boxed.as_ref() {
            Message::Assistant(assistant) => Some(assistant),
            Message::User(_) | Message::ToolResult(_) => None,
        },
        AgentMessage::Custom(_) => None,
    }
}

/// Wire string for a stop reason, matching the TypeScript literal.
fn stop_reason_wire(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::print::BufferSink;
    use futures::stream;
    use pi_agent::user_text;
    use pi_ai::{AssistantContent, TextContent};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn assistant_with(text: &str, reason: StopReason) -> AgentMessage {
        let mut msg = AssistantMessage::new("api", "provider", "model", 2);
        if !text.is_empty() {
            msg.content
                .push(AssistantContent::Text(TextContent::new(text)));
        }
        msg.stop_reason = reason;
        AgentMessage::Llm(Box::new(Message::Assistant(msg)))
    }

    fn assistant_error(message: &str) -> AgentMessage {
        let mut msg = AssistantMessage::new("api", "provider", "model", 2);
        msg.stop_reason = StopReason::Error;
        msg.error_message = Some(message.to_owned());
        AgentMessage::Llm(Box::new(Message::Assistant(msg)))
    }

    #[tokio::test]
    async fn text_renders_final_assistant_text() -> TestResult {
        let events = vec![
            AgentSessionEvent::AgentStart,
            AgentSessionEvent::MessageEnd {
                message: assistant_with("Hello\nWorld", StopReason::Stop),
            },
            AgentSessionEvent::AgentEnd {
                messages: vec![assistant_with("Hello\nWorld", StopReason::Stop)],
                will_retry: false,
            },
        ];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 0);
        assert_eq!(sink.stdout_string(), "Hello\nWorld\n");
        assert!(sink.stderr_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn text_error_stop_reason_to_stderr_exit_one() -> TestResult {
        let events = vec![AgentSessionEvent::AgentEnd {
            messages: vec![assistant_error("rate limited")],
            will_retry: false,
        }];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 1);
        assert_eq!(sink.stderr_string(), "rate limited\n");
        assert!(sink.stdout_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn text_aborted_without_error_message_uses_request_prefix() -> TestResult {
        let mut msg = AssistantMessage::new("api", "provider", "model", 2);
        msg.stop_reason = StopReason::Aborted;
        let message = AgentMessage::Llm(Box::new(Message::Assistant(msg)));
        let events = vec![AgentSessionEvent::AgentEnd {
            messages: vec![message],
            will_retry: false,
        }];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 1);
        assert_eq!(sink.stderr_string(), "Request aborted\n");
        Ok(())
    }

    #[tokio::test]
    async fn text_fragmented_updates_coalesce() -> TestResult {
        let mut partial = AssistantMessage::new("api", "provider", "model", 2);
        partial
            .content
            .push(AssistantContent::Text(TextContent::new("Hel")));
        partial
            .content
            .push(AssistantContent::Text(TextContent::new("lo")));
        let final_msg = assistant_with("Hello", StopReason::Stop);
        let events = vec![
            AgentSessionEvent::MessageStart {
                message: AgentMessage::Llm(Box::new(Message::Assistant(partial))),
            },
            AgentSessionEvent::MessageEnd {
                message: final_msg.clone(),
            },
            AgentSessionEvent::AgentEnd {
                messages: vec![final_msg],
                will_retry: false,
            },
        ];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 0);
        assert_eq!(sink.stdout_string(), "Hello\n");
        Ok(())
    }

    #[tokio::test]
    async fn text_consumes_tool_and_thinking_events_without_output() -> TestResult {
        let tool_start = AgentSessionEvent::ToolExecutionStart {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            args: serde_json::Map::new(),
        };
        let final_msg = assistant_with("done", StopReason::Stop);
        let events = vec![
            AgentSessionEvent::AgentStart,
            tool_start,
            AgentSessionEvent::AgentEnd {
                messages: vec![final_msg],
                will_retry: false,
            },
        ];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 0);
        assert_eq!(sink.stdout_string(), "done\n");
        Ok(())
    }

    #[tokio::test]
    async fn text_empty_stream_no_output_exit_zero() -> TestResult {
        let sink = BufferSink::default();
        let code = render_text(stream::empty::<AgentSessionEvent>(), &sink).await?;
        assert_eq!(code, 0);
        assert!(sink.stdout_string().is_empty());
        assert!(sink.stderr_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn text_ignores_will_retry_agent_end_until_final() -> TestResult {
        // An agent_end with will_retry=true should not prematurely finalize;
        // a later agent_end with the real answer wins.
        let retry_msg = assistant_error("transient");
        let final_msg = assistant_with("recovered", StopReason::Stop);
        let events = vec![
            AgentSessionEvent::AgentEnd {
                messages: vec![retry_msg],
                will_retry: true,
            },
            AgentSessionEvent::AgentEnd {
                messages: vec![final_msg],
                will_retry: false,
            },
        ];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 0);
        assert_eq!(sink.stdout_string(), "recovered\n");
        Ok(())
    }

    #[tokio::test]
    async fn text_ignores_non_assistant_messages() -> TestResult {
        let events = vec![AgentSessionEvent::MessageEnd {
            message: user_text("hi", std::iter::empty()),
        }];
        let sink = BufferSink::default();
        let code = render_text(stream::iter(events), &sink).await?;
        assert_eq!(code, 0);
        assert!(sink.stdout_string().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn text_handles_all_session_event_variants() -> TestResult {
        // Smoke: every variant must be handleable without panic.
        let mut renderer = TextRenderer::new();
        renderer.handle(&AgentSessionEvent::TurnStart);
        renderer.handle(&AgentSessionEvent::TurnEnd {
            message: assistant_with("x", StopReason::Stop),
            tool_results: Vec::new(),
        });
        renderer.handle(&AgentSessionEvent::QueueUpdate {
            steering: vec!["s".into()],
            follow_up: vec!["f".into()],
        });
        renderer.handle(&AgentSessionEvent::CompactionStart {
            reason: crate::core::agent_session::CompactionReason::Manual,
        });
        renderer.handle(&AgentSessionEvent::CompactionEnd {
            reason: crate::core::agent_session::CompactionReason::Manual,
            result: None,
            aborted: false,
            will_retry: false,
            error_message: None,
        });
        renderer.handle(&AgentSessionEvent::AgentSettled);
        renderer.handle(&AgentSessionEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 100,
            error_message: "e".into(),
        });
        renderer.handle(&AgentSessionEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        });
        renderer.handle(&AgentSessionEvent::SessionStart {
            reason: crate::core::agent_session::SessionStartReason::Startup,
            previous_session_file: None,
        });
        renderer.handle(&AgentSessionEvent::SessionShutdown {
            reason: crate::core::agent_session::SessionShutdownReason::Quit,
            target_session_file: None,
        });
        assert!(renderer.last_assistant.is_some());
        Ok(())
    }
}
