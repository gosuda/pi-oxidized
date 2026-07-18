//! Canonical assistant-message assembly and bounded event delivery.

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use serde_json::Map;
use tokio::sync::{Mutex, mpsc};

use crate::provider::ProviderError;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, DoneReason, ErrorReason, StopReason,
    TextContent, ThinkingContent, ToolCall,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    New,
    Started,
    Terminal,
}

/// The bounded provider event stream consumed by [`crate::Provider`] callers.
pub(crate) type ProviderEventStream =
    BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>;

/// A cloneable bounded event sender enforcing stream ordering and singular termination.
///
/// Every send awaits channel capacity. Clones share the same start/terminal state,
/// so only one clone can emit the start event and only one can emit a terminal event.
#[derive(Clone, Debug)]
pub(crate) struct ProviderEventSender {
    sender: mpsc::Sender<Result<AssistantMessageEvent, ProviderError>>,
    phase: Arc<Mutex<Phase>>,
}

impl ProviderEventSender {
    /// Create a sender and its receiving stream with a non-zero bounded capacity.
    pub(crate) fn channel(capacity: NonZeroUsize) -> (Self, ProviderEventStream) {
        let (sender, receiver) = mpsc::channel(capacity.get());
        let stream = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed();
        (
            Self {
                sender,
                phase: Arc::new(Mutex::new(Phase::New)),
            },
            stream,
        )
    }

    /// Emit the sole start event.
    pub(crate) async fn start(&self, partial: AssistantMessage) -> Result<(), EventSendError> {
        let mut phase = self.phase.lock().await;
        match *phase {
            Phase::New => {
                self.send_raw(AssistantMessageEvent::Start { partial })
                    .await?;
                *phase = Phase::Started;
                Ok(())
            }
            Phase::Started => Err(EventSendError::AlreadyStarted),
            Phase::Terminal => Err(EventSendError::AlreadyTerminated),
        }
    }

    /// Emit a non-terminal semantic event after the start event.
    pub(crate) async fn event(&self, event: AssistantMessageEvent) -> Result<(), EventSendError> {
        match event {
            AssistantMessageEvent::Start { .. } => return Err(EventSendError::AlreadyStarted),
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                return Err(EventSendError::TerminalRequiresDedicatedMethod);
            }
            _ => {}
        }

        let phase = self.phase.lock().await;
        match *phase {
            Phase::New => Err(EventSendError::NotStarted),
            Phase::Started => self.send_raw(event).await,
            Phase::Terminal => Err(EventSendError::AlreadyTerminated),
        }
    }

    /// Emit the sole successful terminal event.
    pub(crate) async fn done(
        &self,
        reason: DoneReason,
        message: AssistantMessage,
    ) -> Result<(), EventSendError> {
        self.send_terminal(AssistantMessageEvent::Done { reason, message })
            .await
    }

    /// Emit the sole failed or cancelled terminal event.
    pub(crate) async fn error(
        &self,
        reason: ErrorReason,
        error: AssistantMessage,
    ) -> Result<(), EventSendError> {
        self.send_terminal(AssistantMessageEvent::Error { reason, error })
            .await
    }

    async fn send_terminal(&self, event: AssistantMessageEvent) -> Result<(), EventSendError> {
        let mut phase = self.phase.lock().await;
        match *phase {
            Phase::New => Err(EventSendError::NotStarted),
            Phase::Terminal => Err(EventSendError::AlreadyTerminated),
            Phase::Started => {
                self.send_raw(event).await?;
                *phase = Phase::Terminal;
                Ok(())
            }
        }
    }

    async fn send_raw(&self, event: AssistantMessageEvent) -> Result<(), EventSendError> {
        self.sender
            .send(Ok(event))
            .await
            .map_err(|_| EventSendError::ReceiverClosed)
    }
}

/// A rejected event transition or closed receiving stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum EventSendError {
    /// A semantic or terminal event was attempted before start.
    #[error("provider event stream has not started")]
    NotStarted,
    /// A second start event was attempted.
    #[error("provider event stream already started")]
    AlreadyStarted,
    /// A second event was attempted after terminal reservation.
    #[error("provider event stream already terminated")]
    AlreadyTerminated,
    /// A terminal event was passed through the semantic-event method.
    #[error("terminal events require done or error")]
    TerminalRequiresDedicatedMethod,
    /// The receiving stream was dropped.
    #[error("provider event receiver closed")]
    ReceiverClosed,
}

/// Owns the canonical serializable assistant message during streaming.
///
/// Only finished content is stored in the message. Adapter scratch state such as
/// partial JSON strings and provider stream indexes must remain in adapter-owned
/// side tables and therefore cannot serialize with this state.
#[derive(Clone, Debug)]
pub(crate) struct AssistantState {
    message: AssistantMessage,
}

impl AssistantState {
    /// Begin assembling an existing assistant message.
    pub(crate) fn new(message: AssistantMessage) -> Self {
        Self { message }
    }

    /// Borrow the current canonical message.
    pub(crate) fn message(&self) -> &AssistantMessage {
        &self.message
    }

    /// Clone the current canonical message for an immutable event snapshot.
    pub(crate) fn snapshot(&self) -> AssistantMessage {
        self.message.clone()
    }

    /// Append an empty text block and return its start event.
    pub(crate) fn start_text(&mut self) -> Result<AssistantMessageEvent, AssistantStateError> {
        self.message
            .content
            .push(AssistantContent::Text(TextContent::new("")));
        let content_index = self.last_index()?;
        Ok(AssistantMessageEvent::TextStart {
            content_index,
            partial: self.snapshot(),
        })
    }

    /// Append text and return a delta event containing a fresh message snapshot.
    pub(crate) fn text_delta(
        &mut self,
        content_index: u64,
        delta: impl Into<String>,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        let delta = delta.into();
        self.text_mut(content_index)?.text.push_str(&delta);
        Ok(AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            partial: self.snapshot(),
        })
    }

    /// Finish a text block.
    pub(crate) fn end_text(
        &self,
        content_index: u64,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        let content = self.text(content_index)?.text.clone();
        Ok(AssistantMessageEvent::TextEnd {
            content_index,
            content,
            partial: self.snapshot(),
        })
    }

    /// Append an empty thinking block and return its start event.
    pub(crate) fn start_thinking(&mut self) -> Result<AssistantMessageEvent, AssistantStateError> {
        self.message
            .content
            .push(AssistantContent::Thinking(ThinkingContent::new("")));
        let content_index = self.last_index()?;
        Ok(AssistantMessageEvent::ThinkingStart {
            content_index,
            partial: self.snapshot(),
        })
    }

    /// Append reasoning text and return a delta event.
    pub(crate) fn thinking_delta(
        &mut self,
        content_index: u64,
        delta: impl Into<String>,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        let delta = delta.into();
        self.thinking_mut(content_index)?.thinking.push_str(&delta);
        Ok(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            partial: self.snapshot(),
        })
    }

    /// Finish a thinking block.
    pub(crate) fn end_thinking(
        &self,
        content_index: u64,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        let content = self.thinking(content_index)?.thinking.clone();
        Ok(AssistantMessageEvent::ThinkingEnd {
            content_index,
            content,
            partial: self.snapshot(),
        })
    }

    /// Append a tool-call block with canonical fields only.
    pub(crate) fn start_tool_call(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        self.message
            .content
            .push(AssistantContent::ToolCall(ToolCall::new(
                id,
                name,
                Map::new(),
            )));
        let content_index = self.last_index()?;
        Ok(AssistantMessageEvent::ToolCallStart {
            content_index,
            partial: self.snapshot(),
        })
    }

    /// Report a serialized argument fragment without storing scratch JSON.
    pub(crate) fn tool_call_delta(
        &self,
        content_index: u64,
        delta: impl Into<String>,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        let _tool_call = self.tool_call(content_index)?;
        Ok(AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta: delta.into(),
            partial: self.snapshot(),
        })
    }

    /// Store parsed arguments and finish a tool-call block.
    pub(crate) fn end_tool_call(
        &mut self,
        content_index: u64,
        arguments: Map<String, serde_json::Value>,
    ) -> Result<AssistantMessageEvent, AssistantStateError> {
        self.tool_call_mut(content_index)?.arguments = arguments;
        let tool_call = self.tool_call(content_index)?.clone();
        Ok(AssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call,
            partial: self.snapshot(),
        })
    }

    /// Set successful terminal state and return the final message.
    pub(crate) fn finish(&mut self, reason: DoneReason) -> AssistantMessage {
        self.message.stop_reason = match reason {
            DoneReason::Stop => StopReason::Stop,
            DoneReason::Length => StopReason::Length,
            DoneReason::ToolUse => StopReason::ToolUse,
        };
        self.message.error_message = None;
        self.snapshot()
    }

    /// Set failed terminal state and return the final message.
    pub(crate) fn fail(
        &mut self,
        reason: ErrorReason,
        message: impl Into<String>,
    ) -> AssistantMessage {
        self.message.stop_reason = match reason {
            ErrorReason::Aborted => StopReason::Aborted,
            ErrorReason::Error => StopReason::Error,
        };
        self.message.error_message = Some(message.into());
        self.snapshot()
    }

    fn last_index(&self) -> Result<u64, AssistantStateError> {
        let index = self
            .message
            .content
            .len()
            .checked_sub(1)
            .ok_or(AssistantStateError::MissingContent(0))?;
        u64::try_from(index).map_err(|_| AssistantStateError::ContentIndexOverflow)
    }

    fn index(content_index: u64) -> Result<usize, AssistantStateError> {
        usize::try_from(content_index).map_err(|_| AssistantStateError::ContentIndexOverflow)
    }

    fn text(&self, index: u64) -> Result<&TextContent, AssistantStateError> {
        match self.message.content.get(Self::index(index)?) {
            Some(AssistantContent::Text(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }

    fn text_mut(&mut self, index: u64) -> Result<&mut TextContent, AssistantStateError> {
        match self.message.content.get_mut(Self::index(index)?) {
            Some(AssistantContent::Text(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }

    fn thinking(&self, index: u64) -> Result<&ThinkingContent, AssistantStateError> {
        match self.message.content.get(Self::index(index)?) {
            Some(AssistantContent::Thinking(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }

    fn thinking_mut(&mut self, index: u64) -> Result<&mut ThinkingContent, AssistantStateError> {
        match self.message.content.get_mut(Self::index(index)?) {
            Some(AssistantContent::Thinking(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }

    fn tool_call(&self, index: u64) -> Result<&ToolCall, AssistantStateError> {
        match self.message.content.get(Self::index(index)?) {
            Some(AssistantContent::ToolCall(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }

    fn tool_call_mut(&mut self, index: u64) -> Result<&mut ToolCall, AssistantStateError> {
        match self.message.content.get_mut(Self::index(index)?) {
            Some(AssistantContent::ToolCall(content)) => Ok(content),
            Some(_) => Err(AssistantStateError::WrongBlockKind(index)),
            None => Err(AssistantStateError::MissingContent(index)),
        }
    }
}

/// A content-block lifecycle violation while assembling a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AssistantStateError {
    /// The content index cannot be represented on this platform.
    #[error("assistant content index overflow")]
    ContentIndexOverflow,
    /// No block exists at the requested index.
    #[error("assistant content block {0} does not exist")]
    MissingContent(u64),
    /// The requested operation does not match the block kind.
    #[error("assistant content block {0} has the wrong kind")]
    WrongBlockKind(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sender_requires_start_and_allows_exactly_one_terminal()
    -> Result<(), Box<dyn std::error::Error>> {
        let capacity = NonZeroUsize::new(4).ok_or("non-zero capacity")?;
        let (sender, mut stream) = ProviderEventSender::channel(capacity);
        let assistant = AssistantMessage::new("api", "provider", "model", 1);
        assert_eq!(
            sender.done(DoneReason::Stop, assistant.clone()).await,
            Err(EventSendError::NotStarted)
        );
        sender.start(assistant.clone()).await?;
        let clone = sender.clone();
        sender.done(DoneReason::Stop, assistant.clone()).await?;
        assert_eq!(
            clone.error(ErrorReason::Error, assistant).await,
            Err(EventSendError::AlreadyTerminated)
        );
        drop(sender);
        drop(clone);

        let mut terminal_count = 0;
        while let Some(event) = stream.next().await {
            if matches!(
                event?,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                terminal_count += 1;
            }
        }
        assert_eq!(terminal_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn terminal_never_overtakes_a_concurrent_event() -> Result<(), Box<dyn std::error::Error>>
    {
        let capacity = NonZeroUsize::new(4).ok_or("non-zero capacity")?;
        let (sender, mut stream) = ProviderEventSender::channel(capacity);
        let assistant = AssistantMessage::new("api", "provider", "model", 1);
        sender.start(assistant.clone()).await?;

        let event_sender = sender.clone();
        let terminal_sender = sender.clone();
        let event = AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: assistant.clone(),
        };
        let (event_result, terminal_result) = tokio::join!(
            event_sender.event(event),
            terminal_sender.done(DoneReason::Stop, assistant)
        );
        assert!(event_result.is_ok() || event_result == Err(EventSendError::AlreadyTerminated));
        terminal_result?;

        drop(sender);
        drop(event_sender);
        drop(terminal_sender);

        let mut saw_terminal = false;
        while let Some(event) = stream.next().await {
            match event? {
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                    assert!(!saw_terminal);
                    saw_terminal = true;
                }
                _ => assert!(!saw_terminal, "semantic event followed terminal"),
            }
        }
        assert!(saw_terminal);
        Ok(())
    }

    #[tokio::test]
    async fn clone_races_preserve_start_event_terminal_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let capacity = NonZeroUsize::new(64).ok_or("non-zero capacity")?;
        let (sender, mut stream) = ProviderEventSender::channel(capacity);
        let assistant = AssistantMessage::new("api", "provider", "model", 1);

        let starters: Vec<_> = (0..8).map(|_| sender.clone()).collect();
        let mut start_handles = Vec::new();
        for starter in starters {
            let message = assistant.clone();
            start_handles.push(tokio::spawn(async move { starter.start(message).await }));
        }
        let mut start_ok = 0;
        for handle in start_handles {
            if handle.await?.is_ok() {
                start_ok += 1;
            }
        }
        assert_eq!(start_ok, 1);

        let event_senders: Vec<_> = (0..8).map(|_| sender.clone()).collect();
        let mut event_handles = Vec::new();
        for (index, event_sender) in event_senders.into_iter().enumerate() {
            let message = assistant.clone();
            event_handles.push(tokio::spawn(async move {
                event_sender
                    .event(AssistantMessageEvent::TextStart {
                        content_index: index as u64,
                        partial: message,
                    })
                    .await
            }));
        }
        for handle in event_handles {
            let _ = handle.await?;
        }

        let terminals: Vec<_> = (0..8).map(|_| sender.clone()).collect();
        let mut terminal_handles = Vec::new();
        for (index, terminal) in terminals.into_iter().enumerate() {
            let message = assistant.clone();
            terminal_handles.push(tokio::spawn(async move {
                if index % 2 == 0 {
                    terminal.done(DoneReason::Stop, message).await
                } else {
                    terminal.error(ErrorReason::Error, message).await
                }
            }));
        }
        let mut terminal_ok = 0;
        for handle in terminal_handles {
            if handle.await?.is_ok() {
                terminal_ok += 1;
            }
        }
        assert_eq!(terminal_ok, 1);

        drop(sender);

        let mut saw_start = false;
        let mut saw_terminal = false;
        let mut terminal_count = 0;
        while let Some(event) = stream.next().await {
            match event? {
                AssistantMessageEvent::Start { .. } => {
                    assert!(!saw_start);
                    assert!(!saw_terminal);
                    saw_start = true;
                }
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                    assert!(saw_start);
                    assert!(!saw_terminal);
                    saw_terminal = true;
                    terminal_count += 1;
                }
                _ => {
                    assert!(saw_start);
                    assert!(!saw_terminal, "semantic event followed terminal");
                }
            }
        }
        assert!(saw_start);
        assert!(saw_terminal);
        assert_eq!(terminal_count, 1);
        Ok(())
    }

    #[test]
    fn state_keeps_scratch_json_out_of_canonical_message() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut state = AssistantState::new(AssistantMessage::new("api", "provider", "model", 1));
        let _start = state.start_tool_call("call", "read")?;
        let _delta = state.tool_call_delta(0, "{\"path\":")?;
        assert!(!serde_json::to_string(state.message())?.contains("partial"));
        let mut arguments = Map::new();
        arguments.insert("path".into(), serde_json::Value::String("file".into()));
        let _end = state.end_tool_call(0, arguments)?;
        let final_message = state.finish(DoneReason::ToolUse);
        assert_eq!(final_message.stop_reason, StopReason::ToolUse);
        Ok(())
    }
}
