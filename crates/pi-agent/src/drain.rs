//! Dedicated provider-stream drain task.
//!
//! [`ProviderDrain`] continuously consumes a `pi-ai` provider stream and fans
//! items into two independent channels:
//!
//! - a lossless, capacity-64 [`tokio::sync::mpsc`] of [`DrainItem`]s for the
//!   agent loop (semantic events plus distinct infrastructure errors)
//! - a lossy [`tokio::sync::watch`] of the latest partial
//!   [`pi_ai::AssistantMessage`] for native UI
//!
//! The drain never emits [`crate::AgentEvent`] values. Terminal
//! `message_end` / `agent_end` ownership stays with the agent loop. Exactly one
//! final item is delivered: the first `Done`/`Error` event, or the first
//! infrastructure `Err`. Duplicate provider terminals after that final are
//! ignored under an explicit contract (the drain exits after the first final,
//! so later stream items are dropped with the task).

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use pi_ai::{AssistantMessage, AssistantMessageEvent, ProviderError};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Bounded capacity of the drain-to-loop event channel.
///
/// Matches the provider event channel capacity so backpressure applies only to
/// the provider/drain path, never to presentation consumers.
pub const DRAIN_EVENT_CAPACITY: usize = 64;

/// One item produced by [`ProviderDrain`] for the agent loop.
#[derive(Clone, Debug)]
pub enum DrainItem {
    /// A semantic provider event, including the terminal `Done` / `Error`
    /// variants that carry the canonical final [`AssistantMessage`].
    ///
    /// Boxed because [`AssistantMessageEvent`] is large relative to
    /// [`ProviderError`]; boxing keeps the enum size small without lint
    /// suppression.
    Event(Box<AssistantMessageEvent>),
    /// An undeliverable stream infrastructure failure.
    ///
    /// Distinct from `AssistantMessageEvent::Error`, which is a semantic
    /// terminal event delivered as [`DrainItem::Event`].
    Infra(ProviderError),
}

impl DrainItem {
    /// Returns `true` when this item ends the provider stream for the loop.
    #[must_use]
    pub fn is_final(&self) -> bool {
        match self {
            Self::Event(event) => is_terminal_event(event),
            Self::Infra(_) => true,
        }
    }

    /// Borrow the semantic event when this item is [`DrainItem::Event`].
    #[must_use]
    pub fn as_event(&self) -> Option<&AssistantMessageEvent> {
        match self {
            Self::Event(event) => Some(event.as_ref()),
            Self::Infra(_) => None,
        }
    }
}

/// Spawns the dedicated task that drains one provider stream.
#[derive(Debug, Default)]
pub struct ProviderDrain;

impl ProviderDrain {
    /// Spawn a drain over `stream`.
    ///
    /// # Channels
    ///
    /// - `partial_tx` receives the latest partial (or final) assistant snapshot
    ///   as each semantic event is observed. Updates are coalesced by watch
    ///   semantics and never block the drain.
    /// - `event_tx` receives every semantic event and infrastructure error in
    ///   source order. Capacity should be [`DRAIN_EVENT_CAPACITY`]; send
    ///   backpressure applies only to this drain task.
    ///
    /// # Termination
    ///
    /// The task ends when any of the following occurs:
    /// - the provider stream ends
    /// - the first final item has been delivered
    /// - `cancel` is cancelled (including while awaiting channel capacity)
    /// - `event_tx`'s receiver is dropped
    ///
    /// # Final-item contract
    ///
    /// Exactly one final can be observed on `event_tx`:
    /// 1. The first `AssistantMessageEvent::Done` or `::Error` is forwarded as
    ///    [`DrainItem::Event`] and ends the drain.
    /// 2. The first stream `Err(ProviderError)` is forwarded as
    ///    [`DrainItem::Infra`] when no final has been observed yet, and ends
    ///    the drain.
    /// 3. Duplicate provider terminals (and any later items) after the first
    ///    final are ignored: the drain exits immediately after delivering the
    ///    first final, so they are never forwarded. This is the explicit
    ///    post-final ignore contract.
    ///
    /// The drain never emits agent lifecycle events (`AgentEvent`,
    /// `message_end`, `agent_end`).
    #[must_use]
    pub fn spawn(
        stream: BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>,
        partial_tx: watch::Sender<Option<Arc<AssistantMessage>>>,
        event_tx: mpsc::Sender<DrainItem>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            run_drain(stream, partial_tx, event_tx, cancel).await;
        })
    }
}

async fn run_drain(
    mut stream: BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>,
    partial_tx: watch::Sender<Option<Arc<AssistantMessage>>>,
    event_tx: mpsc::Sender<DrainItem>,
    cancel: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => return,
            item = stream.next() => item,
        };

        let Some(item) = next else {
            return;
        };

        match item {
            Ok(event) => {
                let terminal = is_terminal_event(&event);
                if terminal {
                    if let Some(message) = terminal_message(&event) {
                        publish_terminal(&partial_tx, message);
                    }
                } else if let Some(partial) = event_partial(&event) {
                    publish_partial(&partial_tx, partial);
                }

                if !send_item(&event_tx, &cancel, DrainItem::Event(Box::new(event))).await {
                    return;
                }

                // First terminal ends the drain. Later provider terminals are
                // ignored by dropping the stream with this task (explicit
                // post-final ignore contract).
                if terminal {
                    return;
                }
            }
            Err(error) => {
                let _ = send_item(&event_tx, &cancel, DrainItem::Infra(error)).await;
                // First infrastructure error is the final.
                return;
            }
        }
    }
}

/// Send one item, aborting if cancelled or the receiver is closed.
///
/// Returns `false` when the drain should stop.
async fn send_item(
    event_tx: &mpsc::Sender<DrainItem>,
    cancel: &CancellationToken,
    item: DrainItem,
) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        result = event_tx.send(item) => result.is_ok(),
    }
}

fn is_terminal_event(event: &AssistantMessageEvent) -> bool {
    matches!(
        event,
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
    )
}

fn event_partial(event: &AssistantMessageEvent) -> Option<&Arc<AssistantMessage>> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

fn terminal_message(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Done { message, .. } => Some(message),
        AssistantMessageEvent::Error { error, .. } => Some(error),
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::TextStart { .. }
        | AssistantMessageEvent::TextDelta { .. }
        | AssistantMessageEvent::TextEnd { .. }
        | AssistantMessageEvent::ThinkingStart { .. }
        | AssistantMessageEvent::ThinkingDelta { .. }
        | AssistantMessageEvent::ThinkingEnd { .. }
        | AssistantMessageEvent::ToolCallStart { .. }
        | AssistantMessageEvent::ToolCallDelta { .. }
        | AssistantMessageEvent::ToolCallEnd { .. } => None,
    }
}

/// Publishes a streaming partial snapshot to the watch channel.
///
/// Refcount-only: the partial already arrives shared as an [`Arc`] on the
/// provider event, so publication clones the pointer and never the message.
fn publish_partial(
    partial_tx: &watch::Sender<Option<Arc<AssistantMessage>>>,
    partial: &Arc<AssistantMessage>,
) {
    // watch::send never blocks; lagging UI receivers observe the latest value.
    let _ = partial_tx.send(Some(Arc::clone(partial)));
}

/// Publishes the terminal assistant message to the watch channel.
///
/// The terminal event keeps ownership of the canonical final message (it is
/// still forwarded to the agent loop), so this materializes exactly one owned
/// copy for the UI watch — the only message clone the drain performs.
fn publish_terminal(
    partial_tx: &watch::Sender<Option<Arc<AssistantMessage>>>,
    message: &AssistantMessage,
) {
    let _ = partial_tx.send(Some(Arc::new(message.clone())));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason,
        ErrorReason, Model, Provider, ProviderError, StopReason, StreamOptions, TextContent,
        ToolCall,
    };
    use serde_json::Map;
    use tokio::sync::{mpsc, watch};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use super::{DRAIN_EVENT_CAPACITY, DrainItem, ProviderDrain};

    type TestResult = Result<(), String>;

    #[derive(Clone)]
    struct MockProvider {
        items: Arc<Vec<Result<AssistantMessageEvent, ProviderError>>>,
        delivered: Arc<AtomicUsize>,
        hang_after: Option<usize>,
    }

    impl MockProvider {
        fn new(items: Vec<Result<AssistantMessageEvent, ProviderError>>) -> Self {
            Self {
                items: Arc::new(items),
                delivered: Arc::new(AtomicUsize::new(0)),
                hang_after: None,
            }
        }

        fn hanging(
            items: Vec<Result<AssistantMessageEvent, ProviderError>>,
            hang_after: usize,
        ) -> Self {
            Self {
                items: Arc::new(items),
                delivered: Arc::new(AtomicUsize::new(0)),
                hang_after: Some(hang_after),
            }
        }

        fn stream_items(&self) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            let items = Arc::clone(&self.items);
            let delivered = Arc::clone(&self.delivered);
            let hang_after = self.hang_after;
            stream::unfold(0usize, move |index| {
                let items = Arc::clone(&items);
                let delivered = Arc::clone(&delivered);
                async move {
                    if hang_after.is_some_and(|limit| index >= limit) {
                        std::future::pending::<()>().await;
                        return None;
                    }
                    if index >= items.len() {
                        return None;
                    }
                    delivered.fetch_add(1, Ordering::SeqCst);
                    Some((items[index].clone(), index + 1))
                }
            })
            .boxed()
        }

        fn delivered(&self) -> usize {
            self.delivered.load(Ordering::SeqCst)
        }
    }

    impl Provider for MockProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            self.stream_items()
        }
    }

    // Compile-time proof that MockProvider satisfies the pi-ai boundary used by
    // the agent loop. Runtime tests drive `stream_items` directly.
    const _: fn() = || {
        fn assert_provider<T: Provider>() {}
        assert_provider::<MockProvider>();
    };

    fn assistant(text: &str) -> AssistantMessage {
        let mut message = AssistantMessage::new("test-api", "test-provider", "test-model", 1);
        if !text.is_empty() {
            message
                .content
                .push(AssistantContent::Text(TextContent::new(text)));
        }
        message
    }

    fn start(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::Start {
            partial: Arc::new(assistant(text)),
        }
    }

    fn text_delta(text: &str, delta: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: delta.into(),
            partial: Arc::new(assistant(text)),
        }
    }

    fn text_end(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text.into(),
            partial: Arc::new(assistant(text)),
        }
    }

    fn done(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Stop;
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        }
    }

    fn error_event(text: &str) -> AssistantMessageEvent {
        let mut message = assistant(text);
        message.stop_reason = StopReason::Error;
        message.error_message = Some(text.into());
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: message,
        }
    }

    fn thinking_start() -> AssistantMessageEvent {
        AssistantMessageEvent::ThinkingStart {
            content_index: 1,
            partial: Arc::new(assistant("")),
        }
    }

    fn thinking_delta(delta: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::ThinkingDelta {
            content_index: 1,
            delta: delta.into(),
            partial: Arc::new(assistant("")),
        }
    }

    fn thinking_end(content: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::ThinkingEnd {
            content_index: 1,
            content: content.into(),
            partial: Arc::new(assistant("")),
        }
    }

    fn tool_call_start() -> AssistantMessageEvent {
        AssistantMessageEvent::ToolCallStart {
            content_index: 2,
            partial: Arc::new(assistant("")),
        }
    }

    fn tool_call_delta(delta: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::ToolCallDelta {
            content_index: 2,
            delta: delta.into(),
            partial: Arc::new(assistant("")),
        }
    }

    fn tool_call_end() -> AssistantMessageEvent {
        AssistantMessageEvent::ToolCallEnd {
            content_index: 2,
            tool_call: ToolCall::new("call-1", "read", Map::new()),
            partial: Arc::new(assistant("")),
        }
    }

    fn text_start(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(assistant(text)),
        }
    }

    fn text_of(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .find_map(|block| match block {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
            })
            .unwrap_or("")
            .to_owned()
    }

    fn event_kind(item: &DrainItem) -> &'static str {
        match item {
            DrainItem::Event(event) => match event.as_ref() {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
                AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
                AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
                AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
                AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
                AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
                AssistantMessageEvent::Done { .. } => "done",
                AssistantMessageEvent::Error { .. } => "error",
            },
            DrainItem::Infra(_) => "infra",
        }
    }

    async fn collect_until_idle(
        rx: &mut mpsc::Receiver<DrainItem>,
        mut handle: JoinHandle<()>,
    ) -> Vec<DrainItem> {
        let mut items = Vec::new();
        loop {
            tokio::select! {
                item = rx.recv() => {
                    if let Some(item) = item {
                        items.push(item);
                    } else {
                        let _ = handle.await;
                        break;
                    }
                }
                result = &mut handle => {
                    let _ = result;
                    while let Ok(item) = rx.try_recv() {
                        items.push(item);
                    }
                    break;
                }
            }
        }
        items
    }

    #[tokio::test]
    async fn long_stream_advances_watch_and_preserves_events() -> TestResult {
        // Emit slowly so the UI watch consumer can observe multiple distinct
        // partial snapshots (watch is lossy/coalescing by design).
        let mut script = Vec::new();
        script.push(Ok(start("")));
        for index in 1..=20 {
            let text = format!("chunk-{index}");
            script.push(Ok(text_delta(&text, &format!("x{index}"))));
        }
        script.push(Ok(text_end("chunk-20")));
        script.push(Ok(done("chunk-20")));

        let script = Arc::new(script);
        let stream = {
            let script = Arc::clone(&script);
            stream::unfold(0usize, move |index| {
                let script = Arc::clone(&script);
                async move {
                    if index >= script.len() {
                        return None;
                    }
                    if index > 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Some((script[index].clone(), index + 1))
                }
            })
            .boxed()
        };

        let (partial_tx, mut partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(stream, partial_tx, event_tx, CancellationToken::new());

        let mut distinct_partials = 0usize;
        let mut last_text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let changed =
                tokio::time::timeout(Duration::from_millis(50), partial_rx.changed()).await;
            if changed.is_ok()
                && let Some(partial) = partial_rx.borrow_and_update().clone()
            {
                let text = text_of(partial.as_ref());
                if text != last_text {
                    last_text = text;
                    distinct_partials += 1;
                }
            }
            if handle.is_finished() && changed.is_err() {
                if let Some(partial) = partial_rx.borrow().clone() {
                    let text = text_of(partial.as_ref());
                    if text != last_text {
                        last_text = text;
                        distinct_partials += 1;
                    }
                }
                break;
            }
        }
        let _ = last_text;

        let items = collect_until_idle(&mut event_rx, handle).await;
        if distinct_partials <= 1 {
            return Err(format!(
                "watch must advance across the long stream, got {distinct_partials} distinct partials"
            ));
        }
        if items.len() < 23 {
            return Err(format!(
                "expected start + 20 deltas + text_end + done, got {}",
                items.len()
            ));
        }

        let mut saw_done = false;
        for item in &items {
            match item {
                DrainItem::Event(event) => {
                    if let AssistantMessageEvent::Done { message, .. } = event.as_ref() {
                        saw_done = true;
                        if message.stop_reason != StopReason::Stop {
                            return Err(format!("Done stop_reason = {:?}", message.stop_reason));
                        }
                    }
                }
                DrainItem::Infra(error) => {
                    return Err(format!("unexpected infra error: {}", error.message()));
                }
            }
        }
        if !saw_done {
            return Err("Done must be forwarded as the exact final event".into());
        }
        let finals = items.iter().filter(|item| item.is_final()).count();
        if finals != 1 {
            return Err(format!("exactly one final item expected, got {finals}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn all_semantic_event_kinds_are_preserved_in_order() -> TestResult {
        let script = vec![
            Ok(start("")),
            Ok(text_start("")),
            Ok(text_delta("a", "a")),
            Ok(text_end("a")),
            Ok(thinking_start()),
            Ok(thinking_delta("t")),
            Ok(thinking_end("t")),
            Ok(tool_call_start()),
            Ok(tool_call_delta("{}")),
            Ok(tool_call_end()),
            Ok(done("a")),
        ];
        let provider = MockProvider::new(script);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        let kinds: Vec<&'static str> = items.iter().map(event_kind).collect();
        let expected = vec![
            "start",
            "text_start",
            "text_delta",
            "text_end",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done",
        ];
        if kinds != expected {
            return Err(format!("kinds = {kinds:?}, expected {expected:?}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn done_is_exact_final_and_publishes_message() -> TestResult {
        let final_message = {
            let mut message = assistant("final");
            message.stop_reason = StopReason::Stop;
            message
        };
        let provider = MockProvider::new(vec![
            Ok(start("")),
            Ok(text_delta("final", "final")),
            Ok(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: final_message.clone(),
            }),
        ]);
        let (partial_tx, partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        if items.len() != 3 {
            return Err(format!("expected 3 items, got {}", items.len()));
        }
        match items[2].as_event() {
            Some(AssistantMessageEvent::Done { message, reason }) => {
                if *reason != DoneReason::Stop {
                    return Err(format!("reason = {reason:?}"));
                }
                if message != &final_message {
                    return Err("Done message mismatch".into());
                }
            }
            Some(other) => return Err(format!("expected Done final, got {other:?}")),
            None => return Err("expected Done final, got Infra".into()),
        }
        if items.iter().filter(|item| item.is_final()).count() != 1 {
            return Err("exactly one final expected".into());
        }

        let latest = partial_rx
            .borrow()
            .clone()
            .ok_or_else(|| "final partial published".to_owned())?;
        if latest.as_ref() != &final_message {
            return Err("watch final snapshot mismatch".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn error_event_is_exact_final() -> TestResult {
        let provider = MockProvider::new(vec![Ok(start("")), Ok(error_event("boom"))]);
        let (partial_tx, partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        if items.len() != 2 {
            return Err(format!("expected 2 items, got {}", items.len()));
        }
        match items[1].as_event() {
            Some(AssistantMessageEvent::Error { error, reason }) => {
                if *reason != ErrorReason::Error {
                    return Err(format!("reason = {reason:?}"));
                }
                if error.error_message.as_deref() != Some("boom") {
                    return Err(format!("error_message = {:?}", error.error_message));
                }
            }
            Some(other) => return Err(format!("expected Error final, got {other:?}")),
            None => return Err("expected Error final, got Infra".into()),
        }
        if !matches!(items[1], DrainItem::Event(_)) {
            return Err("final must remain DrainItem::Event".into());
        }
        if partial_rx.borrow().is_none() {
            return Err("error final should publish partial".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn infrastructure_error_is_distinct_final() -> TestResult {
        let provider = MockProvider::new(vec![
            Ok(start("")),
            Err(ProviderError::new("transport reset")),
        ]);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        if items.len() != 2 {
            return Err(format!("expected 2 items, got {}", items.len()));
        }
        match &items[1] {
            DrainItem::Infra(error) => {
                if error.message() != "transport reset" {
                    return Err(format!("infra message = {}", error.message()));
                }
            }
            DrainItem::Event(event) => {
                return Err(format!("expected Infra final, got Event({event:?})"));
            }
        }
        if !items[1].is_final() {
            return Err("infra must be final".into());
        }
        if matches!(
            items[1].as_event(),
            Some(AssistantMessageEvent::Error { .. })
        ) {
            return Err("infra must not look like semantic Error".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancel_terminates_without_task_leak() -> TestResult {
        let provider = MockProvider::hanging(vec![Ok(start(""))], 1);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let cancel = CancellationToken::new();
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            cancel.clone(),
        );

        let first = event_rx
            .recv()
            .await
            .ok_or_else(|| "start event".to_owned())?;
        if !matches!(first.as_event(), Some(AssistantMessageEvent::Start { .. })) {
            return Err(format!("expected Start, got {first:?}"));
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .map_err(|_| "drain task must finish after cancel".to_owned())?
            .map_err(|join| format!("drain task must not panic: {join}"))?;
        if event_rx.recv().await.is_some() {
            return Err("channel should close after cancel".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancel_unblocks_full_event_channel() -> TestResult {
        let provider = MockProvider::new(vec![
            Ok(start("")),
            Ok(text_delta("a", "a")),
            Ok(text_delta("ab", "b")),
            Ok(done("ab")),
        ]);
        let (partial_tx, _partial_rx) = watch::channel(None);
        // Capacity 1: drain will block on the second send while the receiver
        // holds the first item without draining further.
        let (event_tx, event_rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            cancel.clone(),
        );

        // Give the drain time to fill the channel and block on send.
        tokio::time::sleep(Duration::from_millis(20)).await;
        if handle.is_finished() {
            return Err("drain should still be blocked on full channel".into());
        }

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .map_err(|_| "cancel must unblock a full event channel send".to_owned())?
            .map_err(|join| format!("drain task must not panic: {join}"))?;
        drop(event_rx);
        Ok(())
    }

    #[tokio::test]
    async fn receiver_closure_terminates_without_task_leak() -> TestResult {
        let mut script = Vec::new();
        for index in 0..32 {
            script.push(Ok(text_delta(&format!("t{index}"), "x")));
        }
        script.push(Ok(done("end")));
        let provider = MockProvider::new(script);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, event_rx) = mpsc::channel(1);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        drop(event_rx);

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .map_err(|_| "drain task must finish after receiver closure".to_owned())?
            .map_err(|join| format!("drain task must not panic: {join}"))?;
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_terminals_are_ignored_after_first_final() -> TestResult {
        let provider = MockProvider::new(vec![
            Ok(start("")),
            Ok(done("first")),
            Ok(done("second")),
            Ok(error_event("third")),
            Err(ProviderError::new("late infra")),
        ]);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        if items.len() != 2 {
            return Err(format!("start + first Done only, got {}", items.len()));
        }
        match items[1].as_event() {
            Some(AssistantMessageEvent::Done { message, .. }) => {
                if text_of(message) != "first" {
                    return Err(format!("first Done text = {}", text_of(message)));
                }
            }
            Some(other) => return Err(format!("expected first Done, got {other:?}")),
            None => return Err("expected first Done, got Infra".into()),
        }
        if items.iter().filter(|item| item.is_final()).count() != 1 {
            return Err("exactly one final expected".into());
        }
        // Explicit post-final ignore contract: the drain exits after the first
        // final, so the duplicate terminal is never pulled from the provider.
        if provider.delivered() != 2 {
            return Err(format!(
                "only start + first Done should be pulled, got {}",
                provider.delivered()
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn infra_is_sole_final_when_first() -> TestResult {
        let provider = MockProvider::new(vec![
            Err(ProviderError::new("boom")),
            Ok(done("unreachable")),
        ]);
        let (partial_tx, _partial_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::channel(DRAIN_EVENT_CAPACITY);
        let handle = ProviderDrain::spawn(
            provider.stream_items(),
            partial_tx,
            event_tx,
            CancellationToken::new(),
        );

        let items = collect_until_idle(&mut event_rx, handle).await;
        if items.len() != 1 {
            return Err(format!("expected 1 item, got {}", items.len()));
        }
        match &items[0] {
            DrainItem::Infra(error) => {
                if error.message() != "boom" {
                    return Err(format!("infra message = {}", error.message()));
                }
            }
            DrainItem::Event(event) => {
                return Err(format!("expected Infra, got Event({event:?})"));
            }
        }
        if provider.delivered() != 1 {
            return Err(format!(
                "only first infra should be pulled, got {}",
                provider.delivered()
            ));
        }
        Ok(())
    }
}
