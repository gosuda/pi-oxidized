//! RPC server: JSONL stdin → command dispatch → JSONL stdout.
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/rpc/rpc-mode.ts`
//! (`runRpcMode`, `handleCommand`, `handleInputLine`, `shutdown`).
//!
//! # Dispatch contract
//!
//! - **prompt** — spawns the session run; writes `{success:true}` exactly once
//!   via the preflight callback (before events), returns `None`; failure error
//!   only if preflight never fired.
//! - **All other commands** — await the session call and return the exact
//!   [`RpcResponse`].
//! - **Unknown command** — `error(id, type, "Unknown command: {type}")`; the
//!   `id` is echoed.
//! - **Malformed JSON** — `error(None, "parse", …)`; the `id` is NOT echoed.
//! - **Backpressure** — awaited after every non-prompt response and after
//!   parse errors.
//!
//! # Shutdown
//!
//! - **SIGTERM** → exit 143 (no flush).
//! - **SIGHUP** (unix) → exit 129 (flush).
//! - **stdin EOF** → exit 0 (flush).
//! - **Extension shutdown handler** → exit 0 (flush), checked after each
//!   command and after `agent_settled`.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::Future;
use futures::future::BoxFuture;
use pi_agent::{AgentMessage, QueueMode};
use pi_ai::{ImageContent, Model, ModelThinkingLevel};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use pi_ext::client::{HostUiRequest, HostUiResponse};
use pi_ext::protocol::{NotifyLevel, SlotPlacement};

use crate::core::agent_session::events::AgentSessionEvent;
use crate::core::agent_session::extension::{ExtensionBindings, ExtensionMode};
use crate::core::agent_session::prompt::PreflightCallback;
use crate::core::agent_session_runtime::{ForkOutcome, ForkPosition};
use crate::core::compaction::CompactionResult;
use crate::core::extension_host::{ExtensionUiEvent, HostExtensionRunner};
use crate::core::output_guard as output_guard_mod;
use crate::core::sessions::SessionEntry;

use super::extension_ui::ExtensionUiProxy;
use super::jsonl::{JsonlLineReader, serialize_json_line};
use super::types::{
    BashResult, CycleModelData, CycleThinkingLevelData, ForkMessage, RpcCommand,
    RpcExtensionUiRequest, RpcExtensionUiResponse, RpcResponse, RpcResponseData, RpcSessionState,
    RpcSessionTreeNode, RpcSlashCommand, SessionStats, StreamingBehavior,
};

/// Serialize one RPC record using its native wire serialization.
fn to_jsonl<T: Serialize>(value: &T) -> String {
    serialize_json_line(value).unwrap_or_default()
}

/// Future returned by [`RpcSink`] methods.
type SinkFut = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

/// Protocol-stdout write/backpressure/flush abstraction.
///
/// All futures are `'static + Send` so they can be `tokio::spawn`'d from sync
/// closures (preflight callbacks, event listeners).
pub trait RpcSink: Send + Sync {
    /// Write `text` (including trailing `\n`) to the ordered stdout sink.
    fn write_stdout(&self, text: String) -> SinkFut;
    /// Wait until every previously accepted write has finished draining.
    fn backpressure(&self) -> SinkFut;
    /// Wait for drain, then flush the underlying sink.
    fn flush(&self) -> SinkFut;
}

/// [`OutputGuard`](crate::core::output_guard)-backed sink for production.
#[derive(Clone, Copy, Debug)]
pub struct OutputGuardSink;

impl RpcSink for OutputGuardSink {
    fn write_stdout(&self, text: String) -> SinkFut {
        Box::pin(async move {
            output_guard_mod::write_raw_stdout(text)
                .await
                .map_err(|e| io::Error::other(e.to_string()))
        })
    }
    fn backpressure(&self) -> SinkFut {
        Box::pin(async move {
            output_guard_mod::wait_for_raw_stdout_backpressure()
                .await
                .map_err(|e| io::Error::other(e.to_string()))
        })
    }
    fn flush(&self) -> SinkFut {
        Box::pin(async move {
            output_guard_mod::flush_raw_stdout()
                .await
                .map_err(|e| io::Error::other(e.to_string()))
        })
    }
}

/// In-memory sink for deterministic tests.
#[derive(Clone, Default)]
pub struct BufferSink {
    stdout: Arc<Mutex<Vec<u8>>>,
}

impl BufferSink {
    /// Create an empty buffer sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Return the accumulated stdout bytes as a string.
    #[must_use]
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
    /// Return accumulated stdout split into trimmed lines.
    #[must_use]
    pub fn stdout_lines(&self) -> Vec<String> {
        self.stdout_string()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

impl RpcSink for BufferSink {
    fn write_stdout(&self, text: String) -> SinkFut {
        let buf = Arc::clone(&self.stdout);
        Box::pin(async move {
            buf.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(text.as_bytes());
            Ok(())
        })
    }
    fn backpressure(&self) -> SinkFut {
        Box::pin(async { Ok(()) })
    }
    fn flush(&self) -> SinkFut {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct GatedSink {
    stdout: Arc<Mutex<Vec<u8>>>,
    writes_started: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[cfg(test)]
impl GatedSink {
    async fn wait_for_write(&self, target: usize) {
        loop {
            let started = self.started.notified();
            if self.writes_started.load(Ordering::SeqCst) >= target {
                return;
            }
            started.await;
        }
    }
}

#[cfg(test)]
impl RpcSink for GatedSink {
    fn write_stdout(&self, text: String) -> SinkFut {
        let sink = self.clone();
        Box::pin(async move {
            sink.writes_started.fetch_add(1, Ordering::SeqCst);
            sink.started.notify_waiters();
            sink.release.notified().await;
            sink.stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(text.as_bytes());
            Ok(())
        })
    }

    fn backpressure(&self) -> SinkFut {
        Box::pin(async { Ok(()) })
    }

    fn flush(&self) -> SinkFut {
        Box::pin(async { Ok(()) })
    }
}

/// Callback the host invokes after a session replacement.
pub type RebindCallback = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Result of model cycling.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelCycleResult {
    /// Model now active.
    pub model: Model,
    /// Effective thinking level after clamping.
    pub thinking_level: ModelThinkingLevel,
    /// Whether cycling was across scoped entries.
    pub is_scoped: bool,
}

/// Abstracts the `AgentSessionRuntime` + `AgentSession` surface consumed by the
/// 31 [`RpcCommand`] variants.
///
/// All async methods return [`BoxFuture<'static>`] so they can be `tokio::spawn`'d
/// without borrowing the host; the preflight callback fires synchronously
/// during the first poll of `prompt`, which is how the prompt-response
/// frame is enqueued before any agent event in the central write FIFO.
pub trait RpcSessionHost: Send + Sync {
    // ---- Prompt lifecycle ----
    /// Submit a user prompt. The `preflight` callback fires with `true` on
    /// accept (before the run starts) or `false` on reject.
    fn prompt(
        &self,
        message: String,
        images: Vec<ImageContent>,
        streaming_behavior: Option<StreamingBehavior>,
        preflight: PreflightCallback,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// Steer into the active turn.
    fn steer(
        &self,
        message: String,
        images: Vec<ImageContent>,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// Queue a follow-up after the current turn finishes.
    fn follow_up(
        &self,
        message: String,
        images: Vec<ImageContent>,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// Abort the active agent run + retry, wait for idle.
    fn abort(&self) -> BoxFuture<'static, ()>;

    // ---- State ----
    /// Snapshot the current session for the `get_state` RPC.
    fn get_state(&self) -> BoxFuture<'static, RpcSessionState>;

    // ---- Model ----
    /// List available models from the model runtime.
    fn get_available_models(&self) -> BoxFuture<'static, Vec<Model>>;
    /// Set the active model.
    fn set_model(&self, model: Model) -> BoxFuture<'static, Result<(), String>>;
    /// Cycle to the next model (scoped or all-available).
    fn cycle_model(&self) -> BoxFuture<'static, Option<ModelCycleResult>>;
    /// Set the thinking level. Resolves `true` only when the level change was
    /// durably committed (or was already effective).
    fn set_thinking_level(&self, level: ModelThinkingLevel) -> BoxFuture<'static, bool>;
    /// Cycle to the next thinking level.
    fn cycle_thinking_level(&self) -> BoxFuture<'static, Option<ModelThinkingLevel>>;

    // ---- Queue modes ----
    /// Set steering queue drain mode.
    fn set_steering_mode(&self, mode: QueueMode) -> BoxFuture<'static, ()>;
    /// Set follow-up queue drain mode.
    fn set_follow_up_mode(&self, mode: QueueMode) -> BoxFuture<'static, ()>;

    // ---- Compaction ----
    /// Compact the session.
    fn compact(
        &self,
        custom_instructions: Option<String>,
    ) -> BoxFuture<'static, Result<CompactionResult, String>>;
    /// Enable / disable auto-compaction.
    fn set_auto_compaction(&self, enabled: bool) -> BoxFuture<'static, ()>;

    // ---- Retry ----
    /// Enable / disable auto-retry.
    fn set_auto_retry(&self, enabled: bool) -> BoxFuture<'static, ()>;
    /// Abort in-flight retry sleep.
    fn abort_retry(&self) -> BoxFuture<'static, ()>;

    // ---- Bash ----
    /// Execute a bash command.
    fn execute_bash(
        &self,
        command: String,
        exclude_from_context: Option<bool>,
    ) -> BoxFuture<'static, Result<BashResult, String>>;
    /// Abort a running bash command.
    fn abort_bash(&self) -> BoxFuture<'static, ()>;

    // ---- Session data ----
    /// Aggregate session statistics for `get_session_stats`.
    fn get_session_stats(&self) -> BoxFuture<'static, SessionStats>;
    /// Export the session to HTML. Returns the written file path.
    fn export_to_html(
        &self,
        output_path: Option<String>,
    ) -> BoxFuture<'static, Result<String, String>>;
    /// Set the session display name.
    fn set_session_name(&self, name: String) -> BoxFuture<'static, Result<(), String>>;

    // ---- Session mutations ----
    /// Start a new session. Returns `true` when cancelled by an extension hook.
    fn new_session(
        &self,
        parent_session: Option<String>,
    ) -> BoxFuture<'static, Result<bool, String>>;
    /// Switch to another session file. Returns `true` when cancelled.
    fn switch_session(&self, session_path: String) -> BoxFuture<'static, Result<bool, String>>;
    /// Fork the session. Returns the fork outcome.
    fn fork(
        &self,
        entry_id: String,
        position: ForkPosition,
    ) -> BoxFuture<'static, Result<ForkOutcome, String>>;

    // ---- Session tree / entries ----
    /// All session entries.
    fn get_entries(&self) -> BoxFuture<'static, Vec<SessionEntry>>;
    /// Current leaf entry id.
    fn get_leaf_id(&self) -> BoxFuture<'static, Option<String>>;
    /// Session tree (wire-facing nodes).
    fn get_tree(&self) -> BoxFuture<'static, Vec<RpcSessionTreeNode>>;
    /// Forkable user messages.
    fn get_fork_messages(&self) -> BoxFuture<'static, Vec<ForkMessage>>;
    /// Last assistant text content, if any.
    fn get_last_assistant_text(&self) -> BoxFuture<'static, Option<String>>;
    /// All agent messages.
    fn get_messages(&self) -> BoxFuture<'static, Vec<AgentMessage>>;
    /// Available slash commands (extension + prompt + skill).
    fn get_commands(&self) -> BoxFuture<'static, Vec<RpcSlashCommand>>;

    // ---- Subscribe / extensions ----
    /// Subscribe to public session events. Returns an unsubscribe closure.
    fn subscribe(
        &self,
        listener: Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>,
    ) -> Box<dyn Fn() + Send + Sync>;
    /// Register a backpressure hook invoked when the agent has pending events.
    fn register_backpressure_hook(
        &self,
        hook: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    ) -> Box<dyn Fn() + Send + Sync>;
    /// Bind extensions for RPC mode.
    fn bind_extensions_rpc(
        &self,
        bindings: ExtensionBindings,
    ) -> BoxFuture<'static, Result<(), String>>;
    /// Current concrete extension host, when this session uses one.
    fn host_extension_runner(&self) -> Option<Arc<HostExtensionRunner>> {
        None
    }

    // ---- Lifecycle ----
    /// Dispose the current session and runtime.
    fn dispose(&self) -> BoxFuture<'static, ()>;
    /// Set the rebind callback invoked after session replacement.
    fn set_rebind(&self, callback: Option<RebindCallback>);
}

/// Extension error notification (`type: "extension_error"`).
#[derive(Clone, Debug, Serialize)]
pub struct ExtensionErrorOutput {
    #[serde(rename = "type")]
    type_name: &'static str,
    #[serde(rename = "extensionPath")]
    extension_path: String,
    event: String,
    error: String,
}

impl ExtensionErrorOutput {
    /// Create a new extension error output.
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        event: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            type_name: "extension_error",
            extension_path: path.into(),
            event: event.into(),
            error: error.into(),
        }
    }
}

/// Tagged stdout frame for test deserialization.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ServerOutput {
    /// Command response.
    Response(RpcResponse),
    /// Raw agent event.
    Event(Box<AgentSessionEvent>),
    /// Extension UI request.
    UiRequest(RpcExtensionUiRequest),
    /// Extension error notification.
    ExtensionError(ExtensionErrorOutput),
}

type UnsubSlot = Mutex<Option<Box<dyn Fn() + Send + Sync>>>;

fn lock_unsub(slot: &UnsubSlot) -> std::sync::MutexGuard<'_, Option<Box<dyn Fn() + Send + Sync>>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn take_unsub(slot: &UnsubSlot) {
    if let Some(unsub) = lock_unsub(slot).take() {
        unsub();
    }
}

/// Mutable server state shared between the event loop, event subscriber, and
/// rebind callback.
///
/// All stdout writes go through `write_tx`. A dedicated writer actor consumes
/// line and drain-barrier messages in FIFO order, preserving prompt-response
/// ordering while allowing producers to await the actual server queue.
enum WriteMessage {
    Line(String),
    Drain(oneshot::Sender<()>),
}

pub(crate) struct ServerState {
    sink: Arc<dyn RpcSink>,
    write_tx: mpsc::UnboundedSender<WriteMessage>,
    proxy: ExtensionUiProxy,
    shutdown_requested: Arc<AtomicBool>,
    needs_rebind: Arc<AtomicBool>,
    signal: Arc<Notify>,
    unsubscribe_events: UnsubSlot,
    unsubscribe_backpressure: UnsubSlot,
    unsubscribe_extension_ui: UnsubSlot,
}

impl ServerState {
    fn new(
        sink: Arc<dyn RpcSink>,
        write_tx: mpsc::UnboundedSender<WriteMessage>,
        proxy: ExtensionUiProxy,
    ) -> Self {
        Self {
            sink,
            write_tx,
            proxy,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            needs_rebind: Arc::new(AtomicBool::new(false)),
            signal: Arc::new(Notify::new()),
            unsubscribe_events: Mutex::new(None),
            unsubscribe_backpressure: Mutex::new(None),
            unsubscribe_extension_ui: Mutex::new(None),
        }
    }

    /// Enqueue a JSONL line for ordered stdout emission (synchronous, FIFO).
    fn enqueue(&self, line: String) {
        let _ = self.write_tx.send(WriteMessage::Line(line));
    }

    async fn wait_for_output(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.write_tx.send(WriteMessage::Drain(done_tx)).is_ok() {
            let _ = done_rx.await;
        }
    }

    /// Rebind extensions + subscriptions on the current session.
    ///
    /// All stdout emission (events, extension errors) goes through `write_tx`
    /// so the event loop drains them in FIFO order.
    pub(crate) async fn rebind<H>(&self, host: &H)
    where
        H: RpcSessionHost + ?Sized,
    {
        take_unsub(&self.unsubscribe_events);
        take_unsub(&self.unsubscribe_backpressure);
        take_unsub(&self.unsubscribe_extension_ui);

        let shutdown_flag = Arc::clone(&self.shutdown_requested);
        let shutdown_signal = Arc::clone(&self.signal);
        let error_tx = self.write_tx.clone();
        let bindings = ExtensionBindings {
            mode: Some(ExtensionMode::Rpc),
            shutdown_handler: Some(Arc::new(move || {
                shutdown_flag.store(true, Ordering::SeqCst);
                shutdown_signal.notify_one();
            })),
            on_error: Some(Arc::new(move |err: &str| {
                let output = ExtensionErrorOutput::new(err, "", err);
                let _ = error_tx.send(WriteMessage::Line(to_jsonl(&output)));
            })),
            ..Default::default()
        };
        let _ = host.bind_extensions_rpc(bindings).await;

        let event_tx = self.write_tx.clone();
        let signal = Arc::clone(&self.signal);
        let unsub = host.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            // Upstream emits `entry_appended` on the public session stream only
            // for extension custom entries (agent-session.ts appendCustomEntry);
            // the Rust core also emits it for internal view projection of
            // regular persisted entries. Preserve the typed boundary: an
            // unknown payload that claims `"type": "custom"` is not custom.
            let should_publish = match event {
                AgentSessionEvent::EntryAppended { entry } => {
                    matches!(entry, SessionEntry::Custom(_))
                }
                _ => true,
            };
            if should_publish {
                let _ = event_tx.send(WriteMessage::Line(to_jsonl(event)));
            }
            if matches!(event, AgentSessionEvent::AgentSettled) {
                signal.notify_one();
            }
        }));
        *lock_unsub(&self.unsubscribe_events) = Some(unsub);

        let backpressure_tx = self.write_tx.clone();
        let unsub_bp = host.register_backpressure_hook(Arc::new(move || {
            let write_tx = backpressure_tx.clone();
            Box::pin(async move {
                let (done_tx, done_rx) = oneshot::channel();
                if write_tx.send(WriteMessage::Drain(done_tx)).is_ok() {
                    let _ = done_rx.await;
                }
            })
        }));
        *lock_unsub(&self.unsubscribe_backpressure) = Some(unsub_bp);

        if let Some(runner) = host.host_extension_runner() {
            let cancel = CancellationToken::new();
            let cancel_on_unbind = cancel.clone();
            *lock_unsub(&self.unsubscribe_extension_ui) = Some(Box::new(move || {
                cancel_on_unbind.cancel();
            }));

            if let Some(requests) = runner.take_ui_requests() {
                tokio::spawn(run_extension_dialog_bridge(
                    Arc::clone(&runner),
                    requests,
                    self.proxy.clone(),
                    self.write_tx.clone(),
                    cancel.clone(),
                ));
            }
            tokio::spawn(run_extension_event_bridge(
                runner,
                self.write_tx.clone(),
                cancel,
            ));
        }
    }

    /// Cleanup before exit: cancel UI, unsubscribe, dispose, flush.
    async fn cleanup<H>(&self, host: &H, exit_code: i32)
    where
        H: RpcSessionHost + ?Sized,
    {
        self.proxy.cancel_all();
        take_unsub(&self.unsubscribe_events);
        take_unsub(&self.unsubscribe_backpressure);
        take_unsub(&self.unsubscribe_extension_ui);
        host.dispose().await;
        self.wait_for_output().await;
        if exit_code != 143 {
            let _ = self.sink.flush().await;
        }
    }
}

async fn run_extension_dialog_bridge(
    runner: Arc<HostExtensionRunner>,
    mut requests: mpsc::Receiver<HostUiRequest>,
    proxy: ExtensionUiProxy,
    write_tx: mpsc::UnboundedSender<WriteMessage>,
    cancel: CancellationToken,
) {
    loop {
        let request = tokio::select! {
            () = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
        };
        let runner = Arc::clone(&runner);
        let proxy = proxy.clone();
        let write_tx = write_tx.clone();
        let request_cancel = cancel.child_token();
        tokio::spawn(async move {
            bridge_extension_dialog(runner, request, proxy, write_tx, request_cancel).await;
        });
    }
}

async fn bridge_extension_dialog(
    runner: Arc<HostExtensionRunner>,
    request: HostUiRequest,
    proxy: ExtensionUiProxy,
    write_tx: mpsc::UnboundedSender<WriteMessage>,
    cancel: CancellationToken,
) {
    let timeout_ms = match &request {
        HostUiRequest::Select { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Confirm { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Input { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Editor { .. } => None,
    };
    let (rpc_request, response_rx) = proxy.create_dialog(|id| match &request {
        HostUiRequest::Select { request, .. } => RpcExtensionUiRequest::Select {
            id: id.to_owned(),
            title: request.title.clone(),
            options: request.options.clone(),
            timeout: request.options_meta.timeout_ms,
        },
        HostUiRequest::Confirm { request, .. } => RpcExtensionUiRequest::Confirm {
            id: id.to_owned(),
            title: request.title.clone(),
            message: request.message.clone(),
            timeout: request.options_meta.timeout_ms,
        },
        HostUiRequest::Input { request, .. } => RpcExtensionUiRequest::Input {
            id: id.to_owned(),
            title: request.title.clone(),
            placeholder: request.placeholder.clone(),
            timeout: request.options_meta.timeout_ms,
        },
        HostUiRequest::Editor { request, .. } => RpcExtensionUiRequest::Editor {
            id: id.to_owned(),
            title: request.title.clone(),
            prefill: request.prefill.clone(),
        },
    });
    let rpc_id = rpc_request.id().to_owned();
    let _ = write_tx.send(WriteMessage::Line(to_jsonl(&ServerOutput::UiRequest(
        rpc_request,
    ))));

    let response = if let Some(timeout_ms) = timeout_ms {
        tokio::select! {
            () = cancel.cancelled() => None,
            result = tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                response_rx,
            ) => result.ok().and_then(Result::ok),
        }
    } else {
        tokio::select! {
            () = cancel.cancelled() => None,
            result = response_rx => result.ok(),
        }
    };
    if response.is_none() {
        let _ = proxy.route_response(RpcExtensionUiResponse::Cancelled { id: rpc_id });
    }
    let host_response = map_rpc_ui_response(&request, response);
    let _ = runner.respond_ui(host_response).await;
}

fn map_rpc_ui_response(
    request: &HostUiRequest,
    response: Option<RpcExtensionUiResponse>,
) -> HostUiResponse {
    match request {
        HostUiRequest::Select { id, .. } => HostUiResponse::Select {
            id: *id,
            value: match response {
                Some(RpcExtensionUiResponse::Value { value, .. }) => Some(value),
                _ => None,
            },
        },
        HostUiRequest::Confirm { id, .. } => HostUiResponse::Confirm {
            id: *id,
            confirmed: match response {
                Some(RpcExtensionUiResponse::Confirmed { confirmed, .. }) => confirmed,
                _ => false,
            },
        },
        HostUiRequest::Input { id, .. } => HostUiResponse::Input {
            id: *id,
            value: match response {
                Some(RpcExtensionUiResponse::Value { value, .. }) => Some(value),
                _ => None,
            },
        },
        HostUiRequest::Editor { id, .. } => HostUiResponse::Editor {
            id: *id,
            value: match response {
                Some(RpcExtensionUiResponse::Value { value, .. }) => Some(value),
                _ => None,
            },
        },
    }
}

async fn run_extension_event_bridge(
    runner: Arc<HostExtensionRunner>,
    write_tx: mpsc::UnboundedSender<WriteMessage>,
    cancel: CancellationToken,
) {
    let mut events = runner.subscribe_ui();
    for slot in runner.current_slots() {
        let Some(request) = map_extension_ui_event(ExtensionUiEvent::Slot(slot)) else {
            continue;
        };
        let _ = write_tx.send(WriteMessage::Line(to_jsonl(&ServerOutput::UiRequest(
            request,
        ))));
    }
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => break,
            event = events.recv() => match event {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        };
        let Some(request) = map_extension_ui_event(event) else {
            continue;
        };
        let _ = write_tx.send(WriteMessage::Line(to_jsonl(&ServerOutput::UiRequest(
            request,
        ))));
    }
}

fn map_extension_ui_event(event: ExtensionUiEvent) -> Option<RpcExtensionUiRequest> {
    Some(match event {
        ExtensionUiEvent::Notify(notification) => ExtensionUiProxy::notify(
            &notification.message,
            Some(match notification.level {
                NotifyLevel::Info => super::types::NotifyType::Info,
                NotifyLevel::Warning => super::types::NotifyType::Warning,
                NotifyLevel::Error => super::types::NotifyType::Error,
            }),
        ),
        ExtensionUiEvent::Slot(slot) => {
            let lines = slot
                .lines
                .iter()
                .map(|line| line.iter().map(|run| run.text.as_str()).collect::<String>())
                .collect::<Vec<_>>();
            let placement = match slot.placement {
                SlotPlacement::Footer | SlotPlacement::BelowEditor => {
                    Some(super::types::WidgetPlacement::BelowEditor)
                }
                _ => Some(super::types::WidgetPlacement::AboveEditor),
            };
            ExtensionUiProxy::set_widget(&slot.key, Some(&lines), placement)
        }
        ExtensionUiEvent::Dispose { key } => ExtensionUiProxy::set_widget(&key, None, None),
        // Theme switching is an interactive-mode surface; headless RPC has no
        // paint path to apply it to.
        ExtensionUiEvent::ThemeSet(_) => return None,
        // Forward the controls the RPC extension-ui surface already names;
        // interactive-only controls (working indicator, thinking label,
        // paste, tool expansion) have no RPC counterpart.
        ExtensionUiEvent::UiControl(control) => match control {
            pi_ext::protocol::UiControl::SetStatus { key, text } => {
                ExtensionUiProxy::set_status(&key, text.as_deref())
            }
            pi_ext::protocol::UiControl::SetTitle { title } => {
                ExtensionUiProxy::set_title(title.as_deref().unwrap_or_default())
            }
            pi_ext::protocol::UiControl::SetEditorText { text } => {
                ExtensionUiProxy::set_editor_text(&text)
            }
            _ => return None,
        },
    })
}

/// Dispatch a single [`RpcCommand`].
///
/// Returns `Some(response)` for synchronous commands. Returns `None` for
/// `prompt` (response emitted asynchronously via preflight).
#[allow(clippy::too_many_lines)]
pub(crate) async fn handle_command<H>(
    command: &RpcCommand,
    host: &H,
    state: &ServerState,
) -> Option<RpcResponse>
where
    H: RpcSessionHost + ?Sized,
{
    let id = command.id().map(str::to_owned);

    match command {
        RpcCommand::Prompt {
            message,
            images,
            streaming_behavior,
            ..
        } => {
            // Wait until preflight enqueues the prompt response before returning,
            // so a subsequent command's response cannot overtake it in the FIFO.
            spawn_prompt(
                id,
                message.clone(),
                images.clone().unwrap_or_default(),
                *streaming_behavior,
                host,
                state.write_tx.clone(),
            )
            .await;
            None
        }

        RpcCommand::Steer {
            message, images, ..
        } => match host
            .steer(message.clone(), images.clone().unwrap_or_default())
            .await
        {
            Ok(()) => Some(RpcResponse::ok(id, "steer")),
            Err(e) => Some(RpcResponse::err(id, "steer", e)),
        },

        RpcCommand::FollowUp {
            message, images, ..
        } => match host
            .follow_up(message.clone(), images.clone().unwrap_or_default())
            .await
        {
            Ok(()) => Some(RpcResponse::ok(id, "follow_up")),
            Err(e) => Some(RpcResponse::err(id, "follow_up", e)),
        },

        RpcCommand::Abort { .. } => {
            host.abort().await;
            Some(RpcResponse::ok(id, "abort"))
        }

        RpcCommand::NewSession { parent_session, .. } => {
            match host.new_session(parent_session.clone()).await {
                Ok(cancelled) => {
                    if !cancelled {
                        state.rebind(host).await;
                    }
                    Some(RpcResponse::ok_data(
                        id,
                        "new_session",
                        RpcResponseData::Cancelled { cancelled },
                    ))
                }
                Err(e) => Some(RpcResponse::err(id, "new_session", e)),
            }
        }

        RpcCommand::GetState { .. } => {
            let s = host.get_state().await;
            Some(RpcResponse::ok_data(
                id,
                "get_state",
                RpcResponseData::SessionState(s),
            ))
        }

        RpcCommand::SetModel {
            provider, model_id, ..
        } => {
            let models = host.get_available_models().await;
            match models
                .into_iter()
                .find(|m| m.provider == *provider && m.id == *model_id)
            {
                Some(model) => match host.set_model(model.clone()).await {
                    Ok(()) => Some(RpcResponse::ok_data(
                        id,
                        "set_model",
                        RpcResponseData::Model(model),
                    )),
                    Err(e) => Some(RpcResponse::err(id, "set_model", e)),
                },
                None => Some(RpcResponse::err(
                    id,
                    "set_model",
                    format!("Model not found: {provider}/{model_id}"),
                )),
            }
        }

        RpcCommand::CycleModel { .. } => {
            let r = host.cycle_model().await;
            let data = r.map(|x| {
                RpcResponseData::CycleModel(Some(CycleModelData {
                    model: x.model,
                    thinking_level: x.thinking_level,
                    is_scoped: x.is_scoped,
                }))
            });
            Some(RpcResponse::ok_data(
                id,
                "cycle_model",
                data.unwrap_or(RpcResponseData::CycleModel(None)),
            ))
        }

        RpcCommand::GetAvailableModels { .. } => {
            let models = host.get_available_models().await;
            Some(RpcResponse::ok_data(
                id,
                "get_available_models",
                RpcResponseData::AvailableModels { models },
            ))
        }

        RpcCommand::SetThinkingLevel { level, .. } => {
            if host.set_thinking_level(*level).await {
                Some(RpcResponse::ok(id, "set_thinking_level"))
            } else {
                Some(RpcResponse::err(
                    id,
                    "set_thinking_level",
                    "Failed to persist thinking level change",
                ))
            }
        }

        RpcCommand::CycleThinkingLevel { .. } => {
            let level = host.cycle_thinking_level().await;
            let data = level.map(|l| {
                RpcResponseData::CycleThinkingLevel(Some(CycleThinkingLevelData { level: l }))
            });
            Some(RpcResponse::ok_data(
                id,
                "cycle_thinking_level",
                data.unwrap_or(RpcResponseData::CycleThinkingLevel(None)),
            ))
        }

        RpcCommand::SetSteeringMode { mode, .. } => {
            host.set_steering_mode(*mode).await;
            Some(RpcResponse::ok(id, "set_steering_mode"))
        }

        RpcCommand::SetFollowUpMode { mode, .. } => {
            host.set_follow_up_mode(*mode).await;
            Some(RpcResponse::ok(id, "set_follow_up_mode"))
        }

        RpcCommand::Compact {
            custom_instructions,
            ..
        } => match host.compact(custom_instructions.clone()).await {
            Ok(result) => Some(RpcResponse::ok_data(
                id,
                "compact",
                RpcResponseData::Compaction(result),
            )),
            Err(e) => Some(RpcResponse::err(id, "compact", e)),
        },

        RpcCommand::SetAutoCompaction { enabled, .. } => {
            host.set_auto_compaction(*enabled).await;
            Some(RpcResponse::ok(id, "set_auto_compaction"))
        }

        RpcCommand::SetAutoRetry { enabled, .. } => {
            host.set_auto_retry(*enabled).await;
            Some(RpcResponse::ok(id, "set_auto_retry"))
        }

        RpcCommand::AbortRetry { .. } => {
            host.abort_retry().await;
            Some(RpcResponse::ok(id, "abort_retry"))
        }

        RpcCommand::Bash {
            command: cmd,
            exclude_from_context,
            ..
        } => match host.execute_bash(cmd.clone(), *exclude_from_context).await {
            Ok(result) => Some(RpcResponse::ok_data(
                id,
                "bash",
                RpcResponseData::Bash(result),
            )),
            Err(e) => Some(RpcResponse::err(id, "bash", e)),
        },

        RpcCommand::AbortBash { .. } => {
            host.abort_bash().await;
            Some(RpcResponse::ok(id, "abort_bash"))
        }

        RpcCommand::GetSessionStats { .. } => {
            let session_stats = host.get_session_stats().await;
            Some(RpcResponse::ok_data(
                id,
                "get_session_stats",
                RpcResponseData::SessionStats(session_stats),
            ))
        }

        RpcCommand::ExportHtml { output_path, .. } => {
            match host.export_to_html(output_path.clone()).await {
                Ok(path) => Some(RpcResponse::ok_data(
                    id,
                    "export_html",
                    RpcResponseData::ExportHtml { path },
                )),
                Err(e) => Some(RpcResponse::err(id, "export_html", e)),
            }
        }

        RpcCommand::SwitchSession { session_path, .. } => {
            match host.switch_session(session_path.clone()).await {
                Ok(cancelled) => {
                    if !cancelled {
                        state.rebind(host).await;
                    }
                    Some(RpcResponse::ok_data(
                        id,
                        "switch_session",
                        RpcResponseData::Cancelled { cancelled },
                    ))
                }
                Err(e) => Some(RpcResponse::err(id, "switch_session", e)),
            }
        }

        RpcCommand::Fork { entry_id, .. } => {
            match host.fork(entry_id.clone(), ForkPosition::Before).await {
                Ok(outcome) => {
                    if !outcome.cancelled {
                        state.rebind(host).await;
                    }
                    Some(RpcResponse::ok_data(
                        id,
                        "fork",
                        RpcResponseData::Fork {
                            text: outcome.selected_text.unwrap_or_default(),
                            cancelled: outcome.cancelled,
                        },
                    ))
                }
                Err(e) => Some(RpcResponse::err(id, "fork", e)),
            }
        }

        RpcCommand::Clone { .. } => {
            let leaf = host.get_leaf_id().await;
            match leaf {
                None => Some(RpcResponse::err(
                    id,
                    "clone",
                    "Cannot clone session: no current entry selected",
                )),
                Some(leaf_id) => match host.fork(leaf_id, ForkPosition::At).await {
                    Ok(outcome) => {
                        if !outcome.cancelled {
                            state.rebind(host).await;
                        }
                        Some(RpcResponse::ok_data(
                            id,
                            "clone",
                            RpcResponseData::Cancelled {
                                cancelled: outcome.cancelled,
                            },
                        ))
                    }
                    Err(e) => Some(RpcResponse::err(id, "clone", e)),
                },
            }
        }

        RpcCommand::GetForkMessages { .. } => {
            let messages = host.get_fork_messages().await;
            Some(RpcResponse::ok_data(
                id,
                "get_fork_messages",
                RpcResponseData::ForkMessages { messages },
            ))
        }

        RpcCommand::GetEntries { since, .. } => {
            let entries = host.get_entries().await;
            let filtered = if let Some(since_id) = since {
                match entries
                    .iter()
                    .position(|e| e.id() == Some(since_id.as_str()))
                {
                    None => {
                        return Some(RpcResponse::err(
                            id,
                            "get_entries",
                            format!("Entry not found: {since_id}"),
                        ));
                    }
                    Some(i) => entries.into_iter().skip(i + 1).collect::<Vec<_>>(),
                }
            } else {
                entries
            };
            let leaf_id = host.get_leaf_id().await;
            Some(RpcResponse::ok_data(
                id,
                "get_entries",
                RpcResponseData::Entries {
                    entries: filtered,
                    leaf_id,
                },
            ))
        }

        RpcCommand::GetTree { .. } => {
            let tree = host.get_tree().await;
            let leaf_id = host.get_leaf_id().await;
            Some(RpcResponse::ok_data(
                id,
                "get_tree",
                RpcResponseData::Tree { tree, leaf_id },
            ))
        }

        RpcCommand::GetLastAssistantText { .. } => {
            let text = host.get_last_assistant_text().await;
            Some(RpcResponse::ok_data(
                id,
                "get_last_assistant_text",
                RpcResponseData::LastAssistantText { text },
            ))
        }

        RpcCommand::SetSessionName { name, .. } => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Some(RpcResponse::err(
                    id,
                    "set_session_name",
                    "Session name cannot be empty",
                ));
            }
            match host.set_session_name(trimmed.to_owned()).await {
                Ok(()) => Some(RpcResponse::ok(id, "set_session_name")),
                Err(e) => Some(RpcResponse::err(id, "set_session_name", e)),
            }
        }

        RpcCommand::GetMessages { .. } => {
            let messages = host.get_messages().await;
            Some(RpcResponse::ok_data(
                id,
                "get_messages",
                RpcResponseData::Messages { messages },
            ))
        }

        RpcCommand::GetCommands { .. } => {
            let commands = host.get_commands().await;
            Some(RpcResponse::ok_data(
                id,
                "get_commands",
                RpcResponseData::Commands { commands },
            ))
        }

        RpcCommand::Unknown {
            command_type: ct, ..
        } => Some(RpcResponse::err(id, ct, format!("Unknown command: {ct}"))),
    }
}

/// Spawn the prompt run. Emits exactly one success on first preflight `true`,
/// or one error if preflight never fired.
///
/// The returned future resolves once the prompt future has either:
/// - fired preflight (success response already enqueued), or
/// - failed without preflight (error response enqueued).
///
/// Waiting here keeps subsequent command responses behind the prompt response
/// in the central write FIFO. Agent events still enqueue through the same
/// channel after preflight, preserving response-before-events ordering.
async fn spawn_prompt<H>(
    id: Option<String>,
    message: String,
    images: Vec<ImageContent>,
    streaming_behavior: Option<StreamingBehavior>,
    host: &H,
    write_tx: mpsc::UnboundedSender<WriteMessage>,
) where
    H: RpcSessionHost + ?Sized,
{
    let preflight_succeeded = Arc::new(AtomicBool::new(false));
    let (preflight_tx, preflight_rx) = tokio::sync::oneshot::channel::<()>();
    let preflight_notify = Arc::new(Mutex::new(Some(preflight_tx)));

    let success_flag = Arc::clone(&preflight_succeeded);
    let success_tx = write_tx.clone();
    let success_id = id.clone();
    let notify_on_preflight = Arc::clone(&preflight_notify);
    let preflight: PreflightCallback = Arc::new(move |did_succeed: bool| {
        if did_succeed
            && success_flag
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            let response = RpcResponse::ok(success_id.clone(), "prompt");
            let _ = success_tx.send(WriteMessage::Line(to_jsonl(&response)));
        }
        // Unblock the dispatcher after preflight so later commands cannot
        // enqueue ahead of the prompt response.
        if let Some(tx) = notify_on_preflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(());
        }
    });

    let prompt_future = host.prompt(message, images, streaming_behavior, preflight);

    let error_flag = Arc::clone(&preflight_succeeded);
    let error_tx = write_tx;
    let error_id = id;
    let notify_on_error = Arc::clone(&preflight_notify);

    tokio::spawn(async move {
        if let Err(msg) = prompt_future.await
            && !error_flag.load(Ordering::SeqCst)
        {
            let response = RpcResponse::err(error_id, "prompt", msg);
            let _ = error_tx.send(WriteMessage::Line(to_jsonl(&response)));
        }
        // If preflight never fired (host bug / panic path), still release the
        // dispatcher so the command loop cannot hang forever.
        if let Some(tx) = notify_on_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(());
        }
    });

    let _ = preflight_rx.await;
}

/// Outcome of processing one input line.
#[derive(Debug, PartialEq, Eq)]
pub enum LineOutcome {
    /// Command handled (or UI response routed).
    Done,
    /// Shutdown requested — stop reading.
    Shutdown,
}

/// Parse and process one JSONL input line.
pub(crate) async fn process_input_line<H>(line: &str, host: &H, state: &ServerState) -> LineOutcome
where
    H: RpcSessionHost + ?Sized,
{
    let parsed: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            let response = RpcResponse::err(None, "parse", format!("Failed to parse command: {e}"));
            state.enqueue(to_jsonl(&response));
            return LineOutcome::Done;
        }
    };

    // Route extension UI responses before command dispatch.
    if parsed.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        if let Ok(ui_resp) = serde_json::from_value::<RpcExtensionUiResponse>(parsed.clone()) {
            let _ = state.proxy.route_response(ui_resp);
        }
        return LineOutcome::Done;
    }

    let command = match RpcCommand::parse_value(&parsed) {
        Ok(command) => command,
        Err(error) => {
            let response = RpcResponse::err(
                error.id,
                "parse",
                format!("Failed to parse command: {}", error.message),
            );
            state.enqueue(to_jsonl(&response));
            return LineOutcome::Done;
        }
    };

    if let Some(resp) = handle_command(&command, host, state).await {
        state.enqueue(to_jsonl(&resp));
    }

    if state.shutdown_requested.load(Ordering::SeqCst) {
        return LineOutcome::Shutdown;
    }
    LineOutcome::Done
}

enum LineMsg {
    Line(String),
    ReadError(String),
    Eof,
}

async fn stdin_reader<R>(input: R, tx: mpsc::Sender<LineMsg>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut reader = JsonlLineReader::new(input);
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                if tx.send(LineMsg::Line(line)).await.is_err() {
                    break;
                }
            }
            Ok(None) => {
                let _ = tx.send(LineMsg::Eof).await;
                break;
            }
            Err(error) => {
                let _ = tx.send(LineMsg::ReadError(error.to_string())).await;
                break;
            }
        }
    }
}

/// Run the RPC event loop until a shutdown condition is reached.
///
/// Returns the process exit code.
///
/// All stdout frames (responses, events, extension errors, prompt preflight)
/// flow through one writer actor. Ordered drain barriers provide backpressure
/// without making the input/signal loop responsible for stdout progress.
pub async fn run_rpc_loop<H, R>(host: H, sink: Arc<dyn RpcSink>, input: R) -> i32
where
    H: RpcSessionHost,
    R: AsyncRead + Unpin + Send + 'static,
{
    let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteMessage>();
    let state = Arc::new(ServerState::new(
        Arc::clone(&sink),
        write_tx,
        ExtensionUiProxy::new(),
    ));
    let writer_task = tokio::spawn(writer_actor(write_rx, Arc::clone(&sink)));

    let rebind_signal = Arc::clone(&state.signal);
    let rebind_flag = Arc::clone(&state.needs_rebind);
    host.set_rebind(Some(Arc::new(move || {
        let signal = Arc::clone(&rebind_signal);
        let flag = Arc::clone(&rebind_flag);
        Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
            signal.notify_one();
        })
    })));

    state.rebind(&host).await;

    let (line_tx, mut line_rx) = mpsc::channel::<LineMsg>(64);
    tokio::spawn(stdin_reader(input, line_tx));

    let exit_code = loop {
        tokio::select! {
            biased;
            () = state.signal.notified() => {
                if state.shutdown_requested.load(Ordering::SeqCst) {
                    break 0;
                }
                if state.needs_rebind.swap(false, Ordering::SeqCst) {
                    state.rebind(&host).await;
                }
            }
            msg = line_rx.recv() => {
                match msg {
                    Some(LineMsg::Line(line)) => {
                        let outcome = process_input_line(&line, &host, &state).await;
                        state.wait_for_output().await;
                        if outcome == LineOutcome::Shutdown {
                            break 0;
                        }
                    }
                    Some(LineMsg::ReadError(error)) => {
                        let response = RpcResponse::err(
                            None,
                            "transport",
                            format!("Failed to read stdin: {error}"),
                        );
                        state.enqueue(to_jsonl(&response));
                        break 1;
                    }
                    Some(LineMsg::Eof) | None => break 0,
                }
            }
        }
    };

    state.cleanup(&host, exit_code).await;
    writer_task.abort();
    exit_code
}

/// Production entry point. Takes over stdout, reads stdin, dispatches
/// commands, handles signals/EOF, and returns the exit code.
///
/// The caller (bootstrap) is responsible for `std::process::exit(code)`.
pub async fn run_rpc_mode<H>(host: H) -> i32
where
    H: RpcSessionHost,
{
    let _ = output_guard_mod::take_over_stdout();
    let sink: Arc<dyn RpcSink> = Arc::new(OutputGuardSink);

    let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteMessage>();
    let state = Arc::new(ServerState::new(
        Arc::clone(&sink),
        write_tx,
        ExtensionUiProxy::new(),
    ));
    let writer_task = tokio::spawn(writer_actor(write_rx, Arc::clone(&sink)));

    let rs = Arc::clone(&state.signal);
    let rf = Arc::clone(&state.needs_rebind);
    host.set_rebind(Some(Arc::new(move || {
        let s = Arc::clone(&rs);
        let f = Arc::clone(&rf);
        Box::pin(async move {
            f.store(true, Ordering::SeqCst);
            s.notify_one();
        })
    })));
    state.rebind(&host).await;

    let (signal_tx, mut signal_rx) = mpsc::channel::<i32>(1);
    spawn_signal_handlers(signal_tx);

    let stdin = tokio::io::stdin();
    let (line_tx, mut line_rx) = mpsc::channel::<LineMsg>(64);
    tokio::spawn(stdin_reader(stdin, line_tx));

    let exit_code = loop {
        tokio::select! {
            biased;
            code = signal_rx.recv() => {
                break code.unwrap_or(0);
            }
            () = state.signal.notified() => {
                if state.shutdown_requested.load(Ordering::SeqCst) {
                    break 0;
                }
                if state.needs_rebind.swap(false, Ordering::SeqCst) {
                    state.rebind(&host).await;
                }
            }
            msg = line_rx.recv() => {
                match msg {
                    Some(LineMsg::Line(line)) => {
                        let outcome = process_input_line(&line, &host, &state).await;
                        state.wait_for_output().await;
                        if outcome == LineOutcome::Shutdown {
                            break 0;
                        }
                    }
                    Some(LineMsg::ReadError(error)) => {
                        let response = RpcResponse::err(
                            None,
                            "transport",
                            format!("Failed to read stdin: {error}"),
                        );
                        state.enqueue(to_jsonl(&response));
                        break 1;
                    }
                    Some(LineMsg::Eof) | None => break 0,
                }
            }
        }
    };

    state.cleanup(&host, exit_code).await;
    writer_task.abort();
    output_guard_mod::restore_stdout();
    exit_code
}

async fn writer_actor(mut write_rx: mpsc::UnboundedReceiver<WriteMessage>, sink: Arc<dyn RpcSink>) {
    while let Some(message) = write_rx.recv().await {
        match message {
            WriteMessage::Line(line) => {
                let _ = sink.write_stdout(line).await;
            }
            WriteMessage::Drain(done) => {
                let _ = sink.backpressure().await;
                let _ = done.send(());
            }
        }
    }
}

/// Spawn SIGTERM (→143) and SIGHUP (→129, unix-only) handlers.
fn spawn_signal_handlers(tx: mpsc::Sender<i32>) {
    #[cfg(unix)]
    {
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                if let Ok(mut sig) = signal(SignalKind::terminate()) {
                    sig.recv().await;
                    let _ = tx.send(143).await;
                }
            });
        }
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sig) = signal(SignalKind::hangup()) {
                sig.recv().await;
                // tx is moved here; SIGHUP is the last handler.
                let _ = tx.send(129).await;
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = tx;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::core::agent_session::extension::ExtensionBindings;
    use crate::core::agent_session_runtime::{ForkOutcome, ForkPosition};
    use crate::core::compaction::CompactionResult;
    use crate::core::sessions::SessionEntry;
    use crate::modes::rpc::types::{
        RpcSessionState, RpcSessionTreeNode, RpcSlashCommand, RpcSlashCommandSource, RpcSourceInfo,
        RpcSourceOrigin, RpcSourceScope, SessionStats, SessionStatsTokens,
    };
    use pi_agent::QueueMode;
    use pi_ai::{ImageContent, Model, ModelThinkingLevel};
    use pi_ext::client::HostClient;
    use pi_ext::protocol::{Frame, FrameKind, HelloAck, Method, decode_frame_str, encode_frame};
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    #[tokio::test]
    async fn rpc_entry_appended_only_for_structural_custom_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let (state, sink_clone, write_rx) = make_state(sink);
        let writer = tokio::spawn(writer_actor(
            write_rx,
            Arc::new(sink_clone.clone()) as Arc<dyn RpcSink>,
        ));
        state.rebind(&host).await;

        let event_tx = host
            .events_tx
            .lock()
            .unwrap()
            .clone()
            .expect("rebind should install an event subscription");
        let custom = serde_json::from_value::<SessionEntry>(serde_json::json!({
            "type": "custom",
            "customType": "extension-state",
            "id": "custom-entry",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00Z"
        }))?;
        let custom_message = serde_json::from_value::<SessionEntry>(serde_json::json!({
            "type": "custom_message",
            "customType": "internal-transcript",
            "content": "do not publish",
            "display": true,
            "id": "custom-message-entry",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:01Z"
        }))?;
        let standard = serde_json::from_value::<SessionEntry>(serde_json::json!({
            "type": "thinking_level_change",
            "thinkingLevel": "high",
            "id": "thinking-entry",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:02Z"
        }))?;
        let unknown_custom = SessionEntry::Unknown(serde_json::json!({
            "type": "custom",
            "id": "unknown-custom-entry"
        }));
        assert!(matches!(&custom, SessionEntry::Custom(_)));
        assert!(matches!(&custom_message, SessionEntry::CustomMessage(_)));
        assert!(matches!(&standard, SessionEntry::ThinkingLevelChange(_)));
        assert!(matches!(&unknown_custom, SessionEntry::Unknown(_)));
        assert_eq!(unknown_custom.discriminant(), "custom");

        for entry in [custom, custom_message, standard, unknown_custom] {
            event_tx
                .send(AgentSessionEvent::EntryAppended { entry })
                .expect("fake event receiver should remain active");
        }
        event_tx
            .send(AgentSessionEvent::AgentSettled)
            .expect("fake event receiver should remain active");
        tokio::time::timeout(std::time::Duration::from_secs(1), state.signal.notified())
            .await
            .expect("agent-settled sentinel should pass through the RPC subscriber");
        state.wait_for_output().await;
        writer.abort();

        let records = sink_clone
            .stdout_lines()
            .into_iter()
            .map(|line| serde_json::from_str::<Value>(&line))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            records
                .iter()
                .map(|record| record["type"].as_str())
                .collect::<Vec<_>>(),
            [Some("entry_appended"), Some("agent_settled")]
        );
        let entry_records = records
            .iter()
            .filter(|record| record["type"] == "entry_appended")
            .collect::<Vec<_>>();
        assert_eq!(
            entry_records.len(),
            1,
            "only extension custom entries may cross the RPC event stream"
        );
        assert_eq!(entry_records[0]["entry"]["type"], "custom");
        assert_eq!(entry_records[0]["entry"]["id"], "custom-entry");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // FakeRpcHost
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct FakeConfig {
        state: RpcSessionState,
        models: Vec<Model>,
        commands: Vec<RpcSlashCommand>,
        cycle_model_result: Option<ModelCycleResult>,
        cycle_thinking_result: Option<ModelThinkingLevel>,
        set_thinking_result: bool,
        compact_result: Option<Result<CompactionResult, String>>,
        bash_result: Option<Result<BashResult, String>>,
        fork_outcome: Result<ForkOutcome, String>,
        leaf_id: Option<String>,
        prompt_error: Option<String>,
        session_op_cancelled: bool,
    }

    impl Default for FakeConfig {
        fn default() -> Self {
            Self {
                state: test_state(),
                models: vec![],
                commands: vec![],
                cycle_model_result: None,
                cycle_thinking_result: None,
                set_thinking_result: true,
                compact_result: None,
                bash_result: None,
                fork_outcome: Ok(ForkOutcome::default()),
                leaf_id: Some("leaf1".into()),
                prompt_error: None,
                session_op_cancelled: false,
            }
        }
    }

    fn test_state() -> RpcSessionState {
        RpcSessionState {
            model: None,
            thinking_level: ModelThinkingLevel::Medium,
            is_streaming: false,
            is_compacting: false,
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::OneAtATime,
            session_file: None,
            session_id: "test-session".into(),
            session_name: None,
            auto_compaction_enabled: true,
            message_count: 0,
            pending_message_count: 0,
        }
    }

    fn test_stats() -> SessionStats {
        SessionStats {
            session_file: None,
            session_id: "test-session".into(),
            user_messages: 0,
            assistant_messages: 0,
            tool_calls: 0,
            tool_results: 0,
            total_messages: 0,
            tokens: SessionStatsTokens {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                total: 0,
            },
            cost: 0.0,
            context_usage: None,
        }
    }

    struct FailingInput;

    impl AsyncRead for FailingInput {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "stdin transport failed",
            )))
        }
    }

    #[derive(Clone)]
    struct FakeRpcHost {
        cfg: Arc<Mutex<FakeConfig>>,
        calls: Arc<Mutex<Vec<String>>>,
        disposed: Arc<AtomicBool>,
        events_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<AgentSessionEvent>>>>,
        bindings: Arc<Mutex<Option<ExtensionBindings>>>,
        extension_runner: Arc<Mutex<Option<Arc<HostExtensionRunner>>>>,
    }

    impl FakeRpcHost {
        fn new(cfg: FakeConfig) -> Self {
            Self {
                cfg: Arc::new(Mutex::new(cfg)),
                calls: Arc::new(Mutex::new(Vec::new())),
                disposed: Arc::new(AtomicBool::new(false)),
                events_tx: Arc::new(Mutex::new(None)),
                bindings: Arc::new(Mutex::new(None)),
                extension_runner: Arc::new(Mutex::new(None)),
            }
        }

        fn set_extension_runner(&self, runner: Arc<HostExtensionRunner>) {
            *self.extension_runner.lock().unwrap() = Some(runner);
        }
        fn rec(&self, name: &str) {
            self.calls.lock().unwrap().push(name.to_owned());
        }
    }

    impl RpcSessionHost for FakeRpcHost {
        fn prompt(
            &self,
            _msg: String,
            _img: Vec<ImageContent>,
            _sb: Option<StreamingBehavior>,
            preflight: PreflightCallback,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.rec("prompt");
            let err = self.cfg.lock().unwrap().prompt_error.clone();
            let events_tx = Arc::clone(&self.events_tx);
            Box::pin(async move {
                if let Some(e) = err {
                    preflight(false);
                    Err(e)
                } else {
                    preflight(true);
                    // Emit an event synchronously immediately after preflight!
                    if let Some(tx) = events_tx.lock().unwrap().as_ref() {
                        let _ = tx.send(AgentSessionEvent::TurnStart);
                    }
                    Ok(())
                }
            })
        }
        fn steer(
            &self,
            _m: String,
            _i: Vec<ImageContent>,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.rec("steer");
            Box::pin(async { Ok(()) })
        }
        fn follow_up(
            &self,
            _m: String,
            _i: Vec<ImageContent>,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.rec("follow_up");
            Box::pin(async { Ok(()) })
        }
        fn abort(&self) -> BoxFuture<'static, ()> {
            self.rec("abort");
            Box::pin(async {})
        }
        fn get_state(&self) -> BoxFuture<'static, RpcSessionState> {
            self.rec("get_state");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().state.clone() })
        }
        fn get_available_models(&self) -> BoxFuture<'static, Vec<Model>> {
            self.rec("get_available_models");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().models.clone() })
        }
        fn set_model(&self, _m: Model) -> BoxFuture<'static, Result<(), String>> {
            self.rec("set_model");
            Box::pin(async { Ok(()) })
        }
        fn cycle_model(&self) -> BoxFuture<'static, Option<ModelCycleResult>> {
            self.rec("cycle_model");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().cycle_model_result.clone() })
        }
        fn set_thinking_level(&self, _l: ModelThinkingLevel) -> BoxFuture<'static, bool> {
            self.rec("set_thinking_level");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().set_thinking_result })
        }
        fn cycle_thinking_level(&self) -> BoxFuture<'static, Option<ModelThinkingLevel>> {
            self.rec("cycle_thinking_level");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().cycle_thinking_result })
        }
        fn set_steering_mode(&self, _m: QueueMode) -> BoxFuture<'static, ()> {
            self.rec("set_steering_mode");
            Box::pin(async {})
        }
        fn set_follow_up_mode(&self, _m: QueueMode) -> BoxFuture<'static, ()> {
            self.rec("set_follow_up_mode");
            Box::pin(async {})
        }
        fn compact(
            &self,
            _ci: Option<String>,
        ) -> BoxFuture<'static, Result<CompactionResult, String>> {
            self.rec("compact");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move {
                cfg.lock()
                    .unwrap()
                    .compact_result
                    .clone()
                    .unwrap_or(Ok(test_compaction_result()))
            })
        }
        fn set_auto_compaction(&self, _e: bool) -> BoxFuture<'static, ()> {
            self.rec("set_auto_compaction");
            Box::pin(async {})
        }
        fn set_auto_retry(&self, _e: bool) -> BoxFuture<'static, ()> {
            self.rec("set_auto_retry");
            Box::pin(async {})
        }
        fn abort_retry(&self) -> BoxFuture<'static, ()> {
            self.rec("abort_retry");
            Box::pin(async {})
        }
        fn execute_bash(
            &self,
            _c: String,
            _e: Option<bool>,
        ) -> BoxFuture<'static, Result<BashResult, String>> {
            self.rec("bash");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move {
                cfg.lock()
                    .unwrap()
                    .bash_result
                    .clone()
                    .unwrap_or(Ok(test_bash_result()))
            })
        }
        fn abort_bash(&self) -> BoxFuture<'static, ()> {
            self.rec("abort_bash");
            Box::pin(async {})
        }
        fn get_session_stats(&self) -> BoxFuture<'static, SessionStats> {
            self.rec("get_session_stats");
            Box::pin(async { test_stats() })
        }
        fn export_to_html(&self, _p: Option<String>) -> BoxFuture<'static, Result<String, String>> {
            self.rec("export_html");
            Box::pin(async { Ok("/tmp/out.html".into()) })
        }
        fn set_session_name(&self, _n: String) -> BoxFuture<'static, Result<(), String>> {
            self.rec("set_session_name");
            Box::pin(async { Ok(()) })
        }
        fn new_session(&self, _p: Option<String>) -> BoxFuture<'static, Result<bool, String>> {
            self.rec("new_session");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { Ok(cfg.lock().unwrap().session_op_cancelled) })
        }
        fn switch_session(&self, _p: String) -> BoxFuture<'static, Result<bool, String>> {
            self.rec("switch_session");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { Ok(cfg.lock().unwrap().session_op_cancelled) })
        }
        fn fork(
            &self,
            _e: String,
            _p: ForkPosition,
        ) -> BoxFuture<'static, Result<ForkOutcome, String>> {
            self.rec("fork");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().fork_outcome.clone() })
        }
        fn get_entries(&self) -> BoxFuture<'static, Vec<SessionEntry>> {
            self.rec("get_entries");
            Box::pin(async { vec![] })
        }
        fn get_leaf_id(&self) -> BoxFuture<'static, Option<String>> {
            self.rec("get_leaf_id");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().leaf_id.clone() })
        }
        fn get_tree(&self) -> BoxFuture<'static, Vec<RpcSessionTreeNode>> {
            self.rec("get_tree");
            Box::pin(async { vec![] })
        }
        fn get_fork_messages(&self) -> BoxFuture<'static, Vec<ForkMessage>> {
            self.rec("get_fork_messages");
            Box::pin(async { vec![] })
        }
        fn get_last_assistant_text(&self) -> BoxFuture<'static, Option<String>> {
            self.rec("get_last_assistant_text");
            Box::pin(async { None })
        }
        fn get_messages(&self) -> BoxFuture<'static, Vec<AgentMessage>> {
            self.rec("get_messages");
            Box::pin(async { vec![] })
        }
        fn get_commands(&self) -> BoxFuture<'static, Vec<RpcSlashCommand>> {
            self.rec("get_commands");
            let cfg = Arc::clone(&self.cfg);
            Box::pin(async move { cfg.lock().unwrap().commands.clone() })
        }
        fn subscribe(
            &self,
            listener: Arc<dyn Fn(&AgentSessionEvent) + Send + Sync>,
        ) -> Box<dyn Fn() + Send + Sync> {
            self.rec("subscribe");
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentSessionEvent>();
            *self.events_tx.lock().unwrap() = Some(tx);
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    listener(&event);
                }
            });
            Box::new(|| {})
        }
        fn register_backpressure_hook(
            &self,
            _hook: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
        ) -> Box<dyn Fn() + Send + Sync> {
            self.rec("register_backpressure_hook");
            Box::new(|| {})
        }
        fn bind_extensions_rpc(
            &self,
            bindings: ExtensionBindings,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.rec("bind_extensions_rpc");
            *self.bindings.lock().unwrap() = Some(bindings);
            Box::pin(async { Ok(()) })
        }
        fn host_extension_runner(&self) -> Option<Arc<HostExtensionRunner>> {
            self.extension_runner.lock().unwrap().clone()
        }
        fn dispose(&self) -> BoxFuture<'static, ()> {
            self.rec("dispose");
            self.disposed.store(true, Ordering::SeqCst);
            Box::pin(async {})
        }
        fn set_rebind(&self, _cb: Option<RebindCallback>) {
            self.rec("set_rebind");
        }
    }
    fn test_compaction_result() -> CompactionResult {
        CompactionResult {
            summary: "Summary".into(),
            first_kept_entry_id: "entry1".into(),
            tokens_before: 1000,
            estimated_tokens_after: Some(500),
            details: None,
            from_hook: None,
        }
    }
    fn test_bash_result() -> BashResult {
        BashResult {
            output: "done".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
        }
    }
    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_state(
        sink: BufferSink,
    ) -> (
        Arc<ServerState>,
        BufferSink,
        mpsc::UnboundedReceiver<WriteMessage>,
    ) {
        let s = sink.clone();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteMessage>();
        let state = Arc::new(ServerState::new(
            Arc::new(sink) as Arc<dyn RpcSink>,
            write_tx,
            ExtensionUiProxy::new(),
        ));
        (state, s, write_rx)
    }

    /// Drain all pending write-channel frames into the sink.
    async fn drain(sink: &BufferSink, write_rx: &mut mpsc::UnboundedReceiver<WriteMessage>) {
        while let Ok(message) = write_rx.try_recv() {
            match message {
                WriteMessage::Line(line) => {
                    let _ = sink.write_stdout(line).await;
                }
                WriteMessage::Drain(done) => {
                    let _ = sink.backpressure().await;
                    let _ = done.send(());
                }
            }
        }
    }

    async fn dispatch(cmd_json: &str, cfg: FakeConfig) -> (Value, BufferSink) {
        let host = FakeRpcHost::new(cfg);
        let sink = BufferSink::new();
        let (state, sink_clone, mut write_rx) = make_state(sink);
        process_input_line(cmd_json, &host, &state).await;
        drain(&sink_clone, &mut write_rx).await;
        let lines = sink_clone.stdout_lines();
        assert!(!lines.is_empty(), "expected at least one response line");
        let resp: Value = serde_json::from_str(&lines[0]).unwrap();
        (resp, sink_clone)
    }

    async fn dispatch_no_response(
        cmd_json: &str,
        cfg: FakeConfig,
    ) -> (BufferSink, mpsc::UnboundedReceiver<WriteMessage>) {
        let host = FakeRpcHost::new(cfg);
        let sink = BufferSink::new();
        let (state, sink_clone, mut write_rx) = make_state(sink);
        process_input_line(cmd_json, &host, &state).await;
        drain(&sink_clone, &mut write_rx).await;
        (sink_clone, write_rx)
    }

    // -----------------------------------------------------------------------
    // Unknown command
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unknown_command_echoes_id_and_type() {
        let (resp, _) = dispatch(
            r#"{"type":"totally_unknown","id":"abc","foo":"bar"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(resp["type"], "response");
        assert_eq!(resp["id"], "abc");
        assert_eq!(resp["command"], "totally_unknown");
        assert_eq!(resp["success"], false);
        assert_eq!(resp["error"], "Unknown command: totally_unknown");
    }

    // -----------------------------------------------------------------------
    // Malformed JSON
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn malformed_json_parse_error_no_id() {
        let (resp, _) = dispatch("{not valid json", FakeConfig::default()).await;
        assert_eq!(resp["command"], "parse");
        assert_eq!(resp["success"], false);
        assert!(resp.get("id").is_none() || resp["id"].is_null());
        assert!(
            resp["error"]
                .as_str()
                .unwrap()
                .starts_with("Failed to parse")
        );
    }

    #[tokio::test]
    async fn field_validation_error_echoes_valid_id() {
        let (response, _) = dispatch(
            r#"{"type":"bash","id":"req-17","command":"true","excludeFromContext":"yes"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(response["id"], "req-17");
        assert_eq!(response["command"], "parse");
        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("excludeFromContext")
        );
    }

    // -----------------------------------------------------------------------
    // Prompt preflight semantics
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn prompt_emits_success_once() {
        let (sink, mut write_rx) = dispatch_no_response(
            r#"{"type":"prompt","id":"p1","message":"hello"}"#,
            FakeConfig::default(),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drain(&sink, &mut write_rx).await;
        let lines = sink.stdout_lines();
        assert_eq!(lines.len(), 1, "exactly one success response");
        let resp: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(resp["command"], "prompt");
        assert_eq!(resp["success"], true);
    }

    #[tokio::test]
    async fn prompt_emits_error() {
        let cfg = FakeConfig {
            prompt_error: Some("No model".into()),
            ..FakeConfig::default()
        };
        let (sink, mut write_rx) =
            dispatch_no_response(r#"{"type":"prompt","id":"p2","message":"hi"}"#, cfg).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drain(&sink, &mut write_rx).await;
        let lines = sink.stdout_lines();
        assert_eq!(lines.len(), 1);
        let resp: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(resp["success"], false);
        assert_eq!(resp["error"], "No model");
    }

    // -----------------------------------------------------------------------
    // Simple success responses
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn steer_success() {
        let (r, _) = dispatch(
            r#"{"type":"steer","id":"s1","message":"left"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "steer");
        assert_eq!(r["success"], true);
    }

    #[tokio::test]
    async fn follow_up_success() {
        let (r, _) = dispatch(
            r#"{"type":"follow_up","id":"f1","message":"next"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "follow_up");
    }

    #[tokio::test]
    async fn abort_success() {
        let (r, _) = dispatch(r#"{"type":"abort","id":"a1"}"#, FakeConfig::default()).await;
        assert_eq!(r["command"], "abort");
    }

    #[tokio::test]
    async fn set_thinking_level_success() {
        let (r, _) = dispatch(
            r#"{"type":"set_thinking_level","id":"t1","level":"high"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "set_thinking_level");
        assert_eq!(r["success"], true);
    }

    #[tokio::test]
    async fn set_thinking_level_uncommitted_returns_error() {
        let (r, _) = dispatch(
            r#"{"type":"set_thinking_level","id":"t2","level":"high"}"#,
            FakeConfig {
                set_thinking_result: false,
                ..FakeConfig::default()
            },
        )
        .await;
        assert_eq!(r["command"], "set_thinking_level");
        assert_eq!(r["success"], false);
        assert_eq!(r["error"], "Failed to persist thinking level change");
    }

    #[tokio::test]
    async fn set_steering_mode_success() {
        let (r, _) = dispatch(
            r#"{"type":"set_steering_mode","id":"sm1","mode":"all"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "set_steering_mode");
    }

    #[tokio::test]
    async fn set_follow_up_mode_success() {
        let (r, _) = dispatch(
            r#"{"type":"set_follow_up_mode","id":"fm1","mode":"one-at-a-time"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "set_follow_up_mode");
    }

    #[tokio::test]
    async fn set_auto_compaction_success() {
        let (r, _) = dispatch(
            r#"{"type":"set_auto_compaction","id":"ac1","enabled":true}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "set_auto_compaction");
    }

    #[tokio::test]
    async fn set_auto_retry_success() {
        let (r, _) = dispatch(
            r#"{"type":"set_auto_retry","id":"ar1","enabled":false}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "set_auto_retry");
    }

    #[tokio::test]
    async fn abort_retry_success() {
        let (r, _) = dispatch(
            r#"{"type":"abort_retry","id":"abr1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["command"], "abort_retry");
    }

    #[tokio::test]
    async fn abort_bash_success() {
        let (r, _) = dispatch(r#"{"type":"abort_bash","id":"ab1"}"#, FakeConfig::default()).await;
        assert_eq!(r["command"], "abort_bash");
    }

    // -----------------------------------------------------------------------
    // Data responses
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_state_returns_snapshot() {
        let (r, _) = dispatch(r#"{"type":"get_state","id":"gs1"}"#, FakeConfig::default()).await;
        assert_eq!(r["data"]["sessionId"], "test-session");
        assert_eq!(r["data"]["thinkingLevel"], "medium");
    }

    #[tokio::test]
    async fn get_session_stats_returns_data() {
        let (r, _) = dispatch(
            r#"{"type":"get_session_stats","id":"sst1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["sessionId"], "test-session");
    }

    #[tokio::test]
    async fn export_html_returns_path() {
        let (r, _) = dispatch(
            r#"{"type":"export_html","id":"eh1","outputPath":"/tmp/x.html"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["path"], "/tmp/out.html");
    }

    #[tokio::test]
    async fn get_last_assistant_text_null() {
        let (r, _) = dispatch(
            r#"{"type":"get_last_assistant_text","id":"lat1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert!(r["data"]["text"].is_null());
    }

    #[tokio::test]
    async fn get_messages_empty() {
        let (r, _) = dispatch(
            r#"{"type":"get_messages","id":"gm1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["messages"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_commands_returns_complete_catalog() {
        let source_info = RpcSourceInfo {
            path: "/tmp/resource".into(),
            source: "test".into(),
            scope: RpcSourceScope::Temporary,
            origin: RpcSourceOrigin::TopLevel,
            base_dir: None,
        };
        let commands = [
            ("ext-command", RpcSlashCommandSource::Extension),
            ("deploy", RpcSlashCommandSource::Prompt),
            ("skill:review", RpcSlashCommandSource::Skill),
        ]
        .into_iter()
        .map(|(name, source)| RpcSlashCommand {
            name: name.into(),
            description: Some(format!("{name} description")),
            source,
            source_info: source_info.clone(),
        })
        .collect();
        let (response, _) = dispatch(
            r#"{"type":"get_commands","id":"gc1"}"#,
            FakeConfig {
                commands,
                ..FakeConfig::default()
            },
        )
        .await;
        let catalog = response["data"]["commands"].as_array().unwrap();
        assert_eq!(catalog.len(), 3);
        assert_eq!(catalog[0]["source"], "extension");
        assert_eq!(catalog[1]["source"], "prompt");
        assert_eq!(catalog[2]["name"], "skill:review");
        assert_eq!(catalog[2]["source"], "skill");
    }

    #[tokio::test]
    async fn bash_returns_result() {
        let (r, _) = dispatch(
            r#"{"type":"bash","id":"b1","command":"echo hi"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["output"], "done");
    }

    #[tokio::test]
    async fn compact_returns_result() {
        let (r, _) = dispatch(r#"{"type":"compact","id":"c1"}"#, FakeConfig::default()).await;
        assert_eq!(r["data"]["summary"], "Summary");
    }

    #[tokio::test]
    async fn cycle_model_null() {
        let (r, _) = dispatch(
            r#"{"type":"cycle_model","id":"cm1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert!(r["data"].is_null());
    }

    #[tokio::test]
    async fn cycle_thinking_null() {
        let (r, _) = dispatch(
            r#"{"type":"cycle_thinking_level","id":"ct1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert!(r["data"].is_null());
    }

    #[tokio::test]
    async fn get_available_models_empty() {
        let (r, _) = dispatch(
            r#"{"type":"get_available_models","id":"gam1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["models"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn get_fork_messages_empty() {
        let (r, _) = dispatch(
            r#"{"type":"get_fork_messages","id":"gfm1"}"#,
            FakeConfig::default(),
        )
        .await;
        assert!(r["data"]["messages"].is_array());
    }

    #[tokio::test]
    async fn get_tree_empty() {
        let (r, _) = dispatch(r#"{"type":"get_tree","id":"gt1"}"#, FakeConfig::default()).await;
        assert!(r["data"]["tree"].is_array());
    }

    // -----------------------------------------------------------------------
    // Error cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_model_not_found() {
        let (r, _) = dispatch(
            r#"{"type":"set_model","id":"sm1","provider":"openai","modelId":"gpt-999"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["success"], false);
        assert_eq!(r["error"], "Model not found: openai/gpt-999");
    }
    #[tokio::test]
    async fn clone_no_leaf() {
        let cfg = FakeConfig {
            leaf_id: None,
            ..Default::default()
        };
        let (r, _) = dispatch(r#"{"type":"clone","id":"cl1"}"#, cfg).await;
        assert_eq!(
            r["error"],
            "Cannot clone session: no current entry selected"
        );
    }

    #[tokio::test]
    async fn set_session_name_empty() {
        let (r, _) = dispatch(
            r#"{"type":"set_session_name","id":"ssn1","name":"   "}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["error"], "Session name cannot be empty");
    }

    #[tokio::test]
    async fn get_entries_since_not_found() {
        let (r, _) = dispatch(
            r#"{"type":"get_entries","id":"ge1","since":"nope"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["error"], "Entry not found: nope");
    }

    // -----------------------------------------------------------------------
    // Cancelled mutations
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn new_session_cancelled() {
        let cfg = FakeConfig {
            session_op_cancelled: true,
            ..Default::default()
        };
        let (r, _) = dispatch(r#"{"type":"new_session","id":"ns1"}"#, cfg).await;
        assert_eq!(r["data"]["cancelled"], true);
    }

    #[tokio::test]
    async fn fork_returns_text() {
        let cfg = FakeConfig {
            fork_outcome: Ok(ForkOutcome {
                cancelled: false,
                selected_text: Some("fork here".into()),
            }),
            ..Default::default()
        };
        let (r, _) = dispatch(r#"{"type":"fork","id":"fk1","entryId":"e1"}"#, cfg).await;
        assert_eq!(r["data"]["text"], "fork here");
    }

    #[tokio::test]
    async fn clone_with_leaf() {
        let (r, host) = dispatch(r#"{"type":"clone","id":"cl2"}"#, FakeConfig::default()).await;
        let _ = r;
        let _ = host;
    }

    #[tokio::test]
    async fn switch_session_success() {
        let (r, _) = dispatch(
            r#"{"type":"switch_session","id":"sw1","sessionPath":"/tmp/s.jsonl"}"#,
            FakeConfig::default(),
        )
        .await;
        assert_eq!(r["data"]["cancelled"], false);
    }

    // -----------------------------------------------------------------------
    // Extension UI routing
    // -----------------------------------------------------------------------

    struct RpcHostPeer {
        read: BufReader<DuplexStream>,
        write: DuplexStream,
    }

    impl RpcHostPeer {
        async fn read_frame(&mut self) -> Result<Frame, Box<dyn std::error::Error>> {
            let mut line = String::new();
            self.read.read_line(&mut line).await?;
            Ok(decode_frame_str(line.trim_end())?)
        }

        async fn write_frame(&mut self, frame: &Frame) -> Result<(), Box<dyn std::error::Error>> {
            self.write.write_all(&encode_frame(frame)?).await?;
            self.write.flush().await?;
            Ok(())
        }
    }

    async fn make_rpc_extension_runner()
    -> Result<(Arc<HostExtensionRunner>, RpcHostPeer), Box<dyn std::error::Error>> {
        let (client_stdout, host_stdout) = tokio::io::duplex(64 * 1024);
        let (host_stdin, client_stdin) = tokio::io::duplex(64 * 1024);
        let client = Arc::new(HostClient::connect_boxed(
            Box::new(client_stdin),
            Box::new(client_stdout),
            Box::new(tokio::io::empty()),
            None,
        ));
        let connect_client = Arc::clone(&client);
        let connect = tokio::spawn(async move {
            HostExtensionRunner::connect_with_cwd_and_trust(
                connect_client,
                Vec::new(),
                "/workspace",
                false,
                std::time::Duration::from_secs(1),
            )
            .await
        });
        let mut peer = RpcHostPeer {
            read: BufReader::new(host_stdin),
            write: host_stdout,
        };
        let hello = peer.read_frame().await?;
        peer.write_frame(&Frame::response(
            hello.id,
            Method::Hello,
            serde_json::to_value(HelloAck::local())?,
        ))
        .await?;
        let load = peer.read_frame().await?;
        assert_eq!(load.method, "extensions.load");
        peer.write_frame(&Frame::response(
            load.id,
            Method::Notify,
            serde_json::json!({
                "tools": [],
                "commands": [],
                "shortcuts": [],
                "flags": [],
                "renderers": [],
                "providers": [],
                "handlers": [],
                "errors": [],
                "terminalInput": false
            }),
        ))
        .await?;
        Ok((connect.await??, peer))
    }

    #[tokio::test]
    async fn host_dialog_round_trips_through_rpc_stdout_and_stdin()
    -> Result<(), Box<dyn std::error::Error>> {
        let (runner, mut peer) = make_rpc_extension_runner().await?;
        let host = FakeRpcHost::new(FakeConfig::default());
        host.set_extension_runner(Arc::clone(&runner));
        let sink = BufferSink::new();
        let sink_arc = Arc::new(sink.clone()) as Arc<dyn RpcSink>;
        let (write_tx, write_rx) = mpsc::unbounded_channel::<WriteMessage>();
        let state = ServerState::new(sink_arc.clone(), write_tx, ExtensionUiProxy::new());
        let writer = tokio::spawn(writer_actor(write_rx, sink_arc));
        state.rebind(&host).await;

        peer.write_frame(&Frame {
            id: 901,
            kind: FrameKind::Req,
            method: Method::Select.as_str().to_owned(),
            payload: serde_json::json!({
                "title": "Pick",
                "options": ["a", "b"],
                "timeoutMs": 1000
            }),
        })
        .await?;

        let ui_request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                state.wait_for_output().await;
                if let Some(line) = sink.stdout_lines().last().cloned()
                    && serde_json::from_str::<Value>(&line)
                        .ok()
                        .and_then(|value| {
                            value.get("type").and_then(Value::as_str).map(str::to_owned)
                        })
                        .as_deref()
                        == Some("extension_ui_request")
                {
                    break line;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let request_json: Value = serde_json::from_str(&ui_request)?;
        assert_eq!(request_json["method"], "select");
        assert_eq!(request_json["title"], "Pick");
        let rpc_id = request_json["id"].as_str().ok_or("missing RPC UI id")?;
        let response = serde_json::json!({
            "type": "extension_ui_response",
            "id": rpc_id,
            "value": "b"
        })
        .to_string();
        assert_eq!(
            process_input_line(&response, &host, &state).await,
            LineOutcome::Done
        );

        let host_response =
            tokio::time::timeout(std::time::Duration::from_secs(1), peer.read_frame()).await??;
        assert_eq!(host_response.kind, FrameKind::Res);
        assert_eq!(host_response.id, 901);
        assert_eq!(host_response.method, "select");
        assert_eq!(host_response.payload["value"], "b");

        state.cleanup(&host, 0).await;
        writer.abort();
        runner.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn ui_response_routes_to_proxy() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let proxy = ExtensionUiProxy::new();
        let (req, rx) = proxy.create_dialog(|id| RpcExtensionUiRequest::Select {
            id: id.to_owned(),
            title: "Pick".into(),
            options: vec!["a".into()],
            timeout: None,
        });
        let pending_id = req.id().to_owned();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<WriteMessage>();
        let state = ServerState::new(Arc::new(sink.clone()) as Arc<dyn RpcSink>, write_tx, proxy);
        let resp_json =
            format!(r#"{{"type":"extension_ui_response","id":"{pending_id}","value":"picked"}}"#);
        process_input_line(&resp_json, &host, &state).await;
        assert!(sink.stdout_lines().is_empty());
        let resp = rx.await.unwrap();
        match resp {
            RpcExtensionUiResponse::Value { value, .. } => assert_eq!(value, "picked"),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn orphan_ui_response_dropped() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let (state, _, _) = make_state(sink.clone());
        process_input_line(
            r#"{"type":"extension_ui_response","id":"orphan","value":"x"}"#,
            &host,
            &state,
        )
        .await;
        assert!(sink.stdout_lines().is_empty());
    }

    // -----------------------------------------------------------------------
    // Queued prompt ordering
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn prompt_then_steer_ordering() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let (state, sink_clone, mut write_rx) = make_state(sink);
        process_input_line(
            r#"{"type":"prompt","id":"q1","message":"first"}"#,
            &host,
            &state,
        )
        .await;
        process_input_line(
            r#"{"type":"steer","id":"q2","message":"second"}"#,
            &host,
            &state,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drain(&sink_clone, &mut write_rx).await;
        let lines = sink_clone.stdout_lines();
        assert_eq!(lines.len(), 2);
        let r1: Value = serde_json::from_str(&lines[0]).unwrap();
        let r2: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(r1["command"], "prompt");
        assert_eq!(r2["command"], "steer");
    }
    #[tokio::test]
    async fn prompt_response_precedes_agent_events() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let (state, sink_clone, mut write_rx) = make_state(sink);

        // Setup event subscriber exactly as the real rebind() does.
        // This ensures the host's emitted events go into the same write_tx queue.
        let event_tx = state.write_tx.clone();
        let unsub = host.subscribe(Arc::new(move |event: &AgentSessionEvent| {
            let _ = event_tx.send(WriteMessage::Line(to_jsonl(event)));
        }));
        *state.unsubscribe_events.lock().unwrap() = Some(unsub);

        // Dispatch prompt. The FakeRpcHost will call preflight(true) and then
        // IMMEDIATELY emit TurnStart.
        process_input_line(
            r#"{"type":"prompt","id":"q3","message":"test"}"#,
            &host,
            &state,
        )
        .await;

        // Wait for the spawned prompt task to run.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drain(&sink_clone, &mut write_rx).await;

        let lines = sink_clone.stdout_lines();
        assert_eq!(
            lines.len(),
            2,
            "Expected exactly 2 frames: response and event"
        );

        // First line MUST be the prompt success response (from preflight).
        let r1: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(r1["type"], "response");
        assert_eq!(r1["command"], "prompt");
        assert_eq!(r1["success"], true);

        // Second line MUST be the event (emitted right after preflight).
        let r2: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(r2["type"], "turn_start");
    }
    // -----------------------------------------------------------------------
    // Shutdown after extension handler
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn shutdown_flag_triggers_exit() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = BufferSink::new();
        let (state, _, _) = make_state(sink);
        // Simulate an extension invoking the RPC shutdown handler.
        state.shutdown_requested.store(true, Ordering::SeqCst);
        let outcome = process_input_line(r#"{"type":"get_state","id":"x"}"#, &host, &state).await;
        assert_eq!(outcome, LineOutcome::Shutdown);
    }

    #[tokio::test]
    async fn extension_shutdown_wakes_idle_loop() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let observer = host.clone();
        let sink = Arc::new(BufferSink::new()) as Arc<dyn RpcSink>;
        let (_input_writer, input_reader) = tokio::io::duplex(64);
        let loop_task = tokio::spawn(run_rpc_loop(host, sink, input_reader));

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let handler = observer
                    .bindings
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|bindings| bindings.shutdown_handler.clone());
                if let Some(handler) = handler {
                    handler();
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("extension bindings were not installed");

        let code = tokio::time::timeout(std::time::Duration::from_secs(1), loop_task)
            .await
            .expect("idle RPC loop did not wake")
            .expect("RPC loop task panicked");
        assert_eq!(code, 0);
    }

    // -----------------------------------------------------------------------
    // Event loop with EOF
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn loop_eof_exit_zero() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = Arc::new(BufferSink::new()) as Arc<dyn RpcSink>;
        let input: &[u8] = b"";
        let code = run_rpc_loop(host, sink, input).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn loop_stdin_read_error_is_protocol_visible_and_nonzero() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let buffer = Arc::new(BufferSink::new());
        let sink = Arc::clone(&buffer) as Arc<dyn RpcSink>;
        let code = run_rpc_loop(host, sink, FailingInput).await;
        assert_eq!(code, 1);
        let lines = buffer.stdout_lines();
        assert_eq!(lines.len(), 1);
        let response: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(response["type"], "response");
        assert_eq!(response["command"], "transport");
        assert_eq!(response["success"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("stdin transport failed")
        );
    }

    #[tokio::test]
    async fn loop_command_then_eof() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let buf = BufferSink::new();
        let sink: Arc<dyn RpcSink> = Arc::new(buf.clone());
        let input: &[u8] = b"{\"type\":\"get_state\",\"id\":\"l1\"}\n";
        let code = run_rpc_loop(host, Arc::clone(&sink), input).await;
        assert_eq!(code, 0);
        assert_eq!(buf.stdout_lines().len(), 1);
    }

    #[tokio::test]
    async fn command_dispatch_waits_for_writer_drain() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let observer = host.clone();
        let gated = GatedSink::default();
        let sink = Arc::new(gated.clone()) as Arc<dyn RpcSink>;
        let input =
            &b"{\"type\":\"get_state\",\"id\":\"one\"}\n{\"type\":\"get_state\",\"id\":\"two\"}\n"
                [..];
        let loop_task = tokio::spawn(run_rpc_loop(host, sink, input));

        gated.wait_for_write(1).await;
        assert_eq!(
            observer
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == "get_state")
                .count(),
            1,
            "second request dispatched before first response drained"
        );
        gated.release.notify_one();

        gated.wait_for_write(2).await;
        gated.release.notify_one();
        let code = tokio::time::timeout(std::time::Duration::from_secs(1), loop_task)
            .await
            .expect("RPC loop did not finish")
            .expect("RPC loop task panicked");
        assert_eq!(code, 0);
        assert_eq!(
            observer
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == "get_state")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn loop_disposes_on_exit() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = Arc::new(BufferSink::new()) as Arc<dyn RpcSink>;
        let input: &[u8] = b"";
        let _ = run_rpc_loop(host, sink, input).await;
        // host was moved; we can't check disposed flag after move.
        // Instead, verify via a shared flag.
    }

    #[tokio::test]
    async fn loop_disposes_host() {
        let host = Arc::new(FakeRpcHost::new(FakeConfig::default()));
        let disposed = {
            let h = Arc::clone(&host);
            // We can't easily pass Arc<FakeRpcHost> to run_rpc_loop since it
            // takes H: RpcSessionHost by value. Instead, test rebind+dispose
            // via direct calls.
            let sink = Arc::new(BufferSink::new()) as Arc<dyn RpcSink>;
            let (write_tx, _) = mpsc::unbounded_channel::<WriteMessage>();
            let state = ServerState::new(sink, write_tx, ExtensionUiProxy::new());
            state.rebind(&*h).await;
            state.cleanup(&*h, 0).await;
            h.disposed.load(Ordering::SeqCst)
        };
        assert!(disposed);
    }

    // -----------------------------------------------------------------------
    // BufferSink ordering
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn buffer_sink_fifo_order() {
        let sink = BufferSink::new();
        sink.clone().write_stdout("a\n".into()).await.unwrap();
        sink.clone().write_stdout("b\n".into()).await.unwrap();
        sink.clone().write_stdout("c\n".into()).await.unwrap();
        assert_eq!(sink.stdout_lines(), vec!["a", "b", "c"]);
    }

    // -----------------------------------------------------------------------
    // No-id command
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn command_without_id() {
        let (r, _) = dispatch(r#"{"type":"get_state"}"#, FakeConfig::default()).await;
        assert!(r.get("id").is_none() || r["id"].is_null());
    }

    // -----------------------------------------------------------------------
    // Rebind binds extensions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rebind_binds_and_subscribes() {
        let host = FakeRpcHost::new(FakeConfig::default());
        let sink = Arc::new(BufferSink::new()) as Arc<dyn RpcSink>;
        let (write_tx, _) = mpsc::unbounded_channel::<WriteMessage>();
        let state = ServerState::new(sink, write_tx, ExtensionUiProxy::new());
        state.rebind(&host).await;
        let calls = host.calls.lock().unwrap();
        assert!(calls.contains(&"bind_extensions_rpc".to_owned()));
        assert!(calls.contains(&"subscribe".to_owned()));
        assert!(calls.contains(&"register_backpressure_hook".to_owned()));
    }

    // -----------------------------------------------------------------------
    // ExtensionErrorOutput serialization
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn extension_error_serializes() {
        let output = ExtensionErrorOutput::new("ext/path", "event_type", "boom");
        let line = to_jsonl(&output);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "extension_error");
        assert_eq!(parsed["extensionPath"], "ext/path");
        assert_eq!(parsed["error"], "boom");
    }
}
