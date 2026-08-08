//! Multiplexed extension-host client.
//!
//! [`HostClient`] owns one spawned source-pinned host process (see
//! [`crate::host`]) and multiplexes request/response, streaming, and event
//! traffic over a single JSONL pipe. Responsibilities, per the host contract:
//!
//! - **One stdin writer.** A single task owns the host stdin and serializes
//!   every outbound frame in arrival order.
//! - **Frame decoder.** One task decodes stdout chunks with the bounded
//!   [`crate::protocol::FrameDecoder`].
//! - **Request ids + generations.** A monotonic id allocator correlates
//!   requests; per-key generation tracking discards stale `uiSlot` pushes.
//! - **Concurrent pending oneshots.** Each in-flight call owns a
//!   `oneshot::Receiver`; the reader dispatches matched responses.
//! - **Cancel / timeout.** Every call has a deadline; cancellation keeps its
//!   pending route until its control frame queues or the transport closes.
//!   Each pending route carries a monotonic generation so a delayed
//!   background cancel cleanup cannot remove a newer entry reusing the same
//!   frame id.
//! - **Bounded event broadcast.** Unsolicited events fan out through a
//!   `broadcast` channel with a fixed capacity.
//! - **Stderr capture.** A tail is retained for crash diagnostics.
//! - **EOF / crash diagnostics.** On stdout EOF, protocol error, or stdin write
//!   failure, every pending call fails once with a non-retryable error carrying
//!   the stderr tail.
//! - **Dispose / kill / reap.** [`HostClient::shutdown`] closes stdin, waits
//!   briefly for graceful exit, then kills and reaps the child.
//!
//! No host process starts until [`HostClient::spawn`] is called.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::host::{HostError, HostSpec};
use crate::protocol::{
    COMPATIBILITY_VERSION, ConfirmRequest, ConfirmResponse, EditorRequest, EditorResponse, Frame,
    FrameDecoder, FrameId, FrameKind, Hello, HelloAck, InputRequest, InputResponse,
    MeasureResponse, Method, NotifyRequest, PROTOCOL_VERSION, SelectRequest, SelectResponse,
    encode_frame, from_payload,
};

/// Default bounded capacity for the outbound (client → host) frame channel.
pub const OUTBOUND_CAPACITY: usize = 128;
/// Default bounded capacity for the unsolicited event broadcast.
pub const EVENT_CAPACITY: usize = 256;
/// Default bounded capacity for per-call streaming event channels.
pub const STREAM_EVENT_CAPACITY: usize = 64;
/// Grace period before killing the host on shutdown.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Default handshake timeout.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum retained stderr tail in bytes.
pub const STDERR_TAIL_BYTES: usize = 16 * 1024;
/// Maximum time to wait when enqueueing a cancel frame on a saturated command channel.
pub const CANCEL_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// Result type for host client operations.
pub type HostResult<T> = Result<T, HostClientError>;

/// A terminal frame result delivered to a pending caller.
type FrameResult = HostResult<Frame>;

/// One in-flight call.
struct PendingEntry {
    /// Terminal response/error sender (taken on Res/Error).
    terminal: Option<oneshot::Sender<FrameResult>>,
    /// Optional streaming event channel for intermediate events.
    stream: Option<mpsc::Sender<Frame>>,
    /// True once cancellation delivery owns this correlation id.
    cancelling: bool,
    /// Monotonic generation assigned by `insert_pending`. Delayed background
    /// cancel cleanup matches it so a newer entry reusing the same frame id
    /// is not removed underneath a stale cancellation.
    generation: u64,
}

/// Outcome of asking the outbound writer to cancel a pending route.
enum CancellationStart {
    Queued,
    QueuedInBackground,
    Closed,
    NotRunning,
    AlreadyCancelling,
}

/// A typed, correlated UI request initiated by the TypeScript host.
#[derive(Debug, Clone)]
pub enum HostUiRequest {
    /// Native select dialog.
    Select {
        /// Original host correlation id.
        id: FrameId,
        /// Dialog request payload.
        request: SelectRequest,
    },
    /// Native confirmation dialog.
    Confirm {
        /// Original host correlation id.
        id: FrameId,
        /// Dialog request payload.
        request: ConfirmRequest,
    },
    /// Native single-line input dialog.
    Input {
        /// Original host correlation id.
        id: FrameId,
        /// Dialog request payload.
        request: InputRequest,
    },
    /// Native multi-line editor dialog.
    Editor {
        /// Original host correlation id.
        id: FrameId,
        /// Dialog request payload.
        request: EditorRequest,
    },
}

impl HostUiRequest {
    /// Original host correlation id.
    #[must_use]
    pub const fn id(&self) -> FrameId {
        match self {
            Self::Select { id, .. }
            | Self::Confirm { id, .. }
            | Self::Input { id, .. }
            | Self::Editor { id, .. } => *id,
        }
    }
}

/// Typed response to a host-initiated UI request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostUiResponse {
    /// Selected value, or dismissal.
    Select {
        /// Original host correlation id.
        id: FrameId,
        /// Selected value, or `None` when dismissed.
        value: Option<String>,
    },
    /// Confirmation result.
    Confirm {
        /// Original host correlation id.
        id: FrameId,
        /// Whether the user confirmed.
        confirmed: bool,
    },
    /// Entered value, or dismissal.
    Input {
        /// Original host correlation id.
        id: FrameId,
        /// Entered value, or `None` when dismissed.
        value: Option<String>,
    },
    /// Edited value, or dismissal.
    Editor {
        /// Original host correlation id.
        id: FrameId,
        /// Edited value, or `None` when dismissed.
        value: Option<String>,
    },
}

/// Cross-task shared state.
struct Shared {
    /// id → pending call. `std::sync::Mutex` because critical sections never await.
    pending: StdMutex<HashMap<FrameId, PendingEntry>>,
    /// Runtime that owns background cancellation sends, including drops made
    /// from threads that are not currently entered into Tokio.
    runtime: tokio::runtime::Handle,
    /// slot key → latest accepted generation. Stale pushes are discarded.
    slot_generations: StdMutex<HashMap<String, u64>>,
    /// Unsolicited event fan-out.
    events: broadcast::Sender<HostEvent>,
    /// Monotonic request id allocator.
    next_id: AtomicU64,
    /// Monotonic generation stamped onto each `PendingEntry` so a delayed
    /// background cancel cleanup cannot remove a newer same-id entry.
    next_pending_generation: AtomicU64,
    /// Retained stderr tail (most recent `STDERR_TAIL_BYTES` bytes).
    stderr: StdMutex<String>,
    /// Cleared once the reader or writer observes end-of-stream.
    running: AtomicBool,
    /// Test-only: signaled once a background cancel cleanup completes so
    /// regression tests await the real cleanup rather than guessing with
    /// yield loops.
    #[cfg(test)]
    cancel_cleanup_done: Notify,
}

/// Typed unsolicited event delivered to subscribers.
#[derive(Debug, Clone)]
pub enum HostEvent {
    /// Host requested a correlated native UI interaction.
    UiRequest(HostUiRequest),
    /// Fire-and-forget host notification.
    Notify(NotifyRequest),
    /// Host pushed a UI slot (generation-filtered).
    UiSlot(crate::protocol::UiSlot),
    /// Host disposed a slot.
    DisposeSlot(crate::protocol::DisposeSlot),
    /// Partial tool update.
    ToolUpdate(crate::protocol::ToolUpdate),
    /// Custom provider stream event.
    ProviderEvent(crate::protocol::ProviderEvent),
    /// Non-retryable extension failure.
    ExtensionError(crate::protocol::ExtensionErrorEvent),
    /// Extension `setTheme` application request.
    ThemeSet(crate::protocol::ThemeSet),
    /// Extension fire-and-forget session action (`pi.setSessionName`, …).
    SessionCommand(crate::protocol::SessionCommand),
    /// Correlated `pi.setModel` request awaiting [`HostClient::respond_set_model`].
    SetModelRequest {
        /// Original host correlation id.
        id: FrameId,
        /// Requested model payload.
        request: crate::protocol::SessionSetModelRequest,
    },
    /// Correlated `ctx.compact` request awaiting [`HostClient::respond_compact`].
    CompactRequest {
        /// Original host correlation id.
        id: FrameId,
        /// Compact request payload.
        request: crate::protocol::SessionCompactRequest,
    },
    /// Correlated `ctx.newSession` request awaiting [`HostClient::respond_new_session`].
    NewSessionRequest {
        /// Original host correlation id.
        id: FrameId,
        /// New-session request payload.
        request: crate::protocol::SessionNewSessionRequest,
    },
    /// Correlated `ctx.fork` request awaiting [`HostClient::respond_fork`].
    ForkRequest {
        /// Original host correlation id.
        id: FrameId,
        /// Fork request payload.
        request: crate::protocol::SessionForkRequest,
    },
    /// Correlated `ctx.navigateTree` request awaiting [`HostClient::respond_navigate_tree`].
    NavigateTreeRequest {
        /// Original host correlation id.
        id: FrameId,
        /// Navigate-tree request payload.
        request: crate::protocol::SessionNavigateTreeRequest,
    },
    /// Correlated `ctx.switchSession` request awaiting [`HostClient::respond_switch_session`].
    SwitchSessionRequest {
        /// Original host correlation id.
        id: FrameId,
        /// Switch-session request payload.
        request: crate::protocol::SessionSwitchSessionRequest,
    },
    /// Correlated `ctx.reload` request awaiting [`HostClient::respond_reload`].
    ReloadRequest {
        /// Original host correlation id.
        id: FrameId,
    },
    /// Host finished a ready-gated replacement (`session.replacementReady`).
    ReplacementReady {
        /// Token previously returned on a replacement response.
        token: String,
    },
    /// Extension fire-and-forget UI control (`ui.setStatus`, …).
    UiControl(crate::protocol::UiControl),
    /// Untyped / unrecognized frame.
    Raw(Frame),
    /// Host stdout closed.
    Eof,
    /// Host emitted a malformed frame.
    ProtocolError(String),
}

/// Host client error.
#[derive(Debug, Error, Clone)]
pub enum HostClientError {
    /// Handshake versions did not match.
    #[error("host handshake failed: {message}")]
    Handshake {
        /// Failure detail.
        message: String,
    },
    /// A request did not complete before its deadline.
    #[error("host request {id} timed out after {timeout:?}")]
    Timeout {
        /// Frame id.
        id: FrameId,
        /// Elapsed deadline.
        timeout: Duration,
    },
    /// A request was cancelled by the caller.
    #[error("host request {id} cancelled")]
    Cancelled {
        /// Frame id.
        id: FrameId,
    },
    /// Host stream closed (EOF or write failure).
    #[error("host closed: {message} (stderr: {stderr})")]
    Closed {
        /// Why the stream closed.
        message: String,
        /// Retained stderr tail.
        stderr: String,
    },
    /// Host emitted a malformed frame.
    #[error("host protocol error: {message} (stderr: {stderr})")]
    Protocol {
        /// Decode/validation failure.
        message: String,
        /// Retained stderr tail.
        stderr: String,
    },
    /// Host returned a structured error frame.
    #[error("host remote error {code}: {message}")]
    Remote {
        /// Stable error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// Spawning the host process failed.
    #[error("host spawn failed: {message}")]
    Spawn {
        /// OS or io detail.
        message: String,
    },
    /// The host is no longer running.
    #[error("host not running")]
    NotRunning,
    /// A payload failed to (de)serialize.
    #[error("host payload error: {0}")]
    Payload(String),
}

impl From<HostError> for HostClientError {
    fn from(value: HostError) -> Self {
        Self::Spawn {
            message: value.to_string(),
        }
    }
}

/// Multiplexed host client.
pub struct HostClient {
    cmd_tx: Mutex<Option<mpsc::Sender<Frame>>>,
    shared: Arc<Shared>,
    child: Mutex<Option<Child>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

impl HostClient {
    /// Spawn the host described by `spec` and connect a client to its pipes.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Spawn`] when the process cannot be started.
    pub fn spawn(spec: &HostSpec) -> HostResult<Self> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| HostClientError::Spawn {
            message: format!("{e}"),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| HostClientError::Spawn {
            message: "stdin pipe unavailable".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| HostClientError::Spawn {
            message: "stdout pipe unavailable".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| HostClientError::Spawn {
            message: "stderr pipe unavailable".to_owned(),
        })?;
        Ok(Self::connect(stdin, stdout, stderr, Some(child)))
    }

    /// Connect over child pipes.
    fn connect(
        stdin: ChildStdin,
        stdout: ChildStdout,
        stderr: ChildStderr,
        child: Option<Child>,
    ) -> Self {
        Self::connect_boxed(Box::new(stdin), Box::new(stdout), Box::new(stderr), child)
    }

    /// Boxed-stream constructor shared by [`HostClient::spawn`] and tests.
    #[must_use]
    pub fn connect_boxed(
        stdin: Box<dyn AsyncWrite + Unpin + Send>,
        stdout: Box<dyn AsyncRead + Unpin + Send>,
        stderr: Box<dyn AsyncRead + Unpin + Send>,
        child: Option<Child>,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Frame>(OUTBOUND_CAPACITY);
        let shared = Arc::new(Shared {
            pending: StdMutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
            slot_generations: StdMutex::new(HashMap::new()),
            events: events_tx,
            next_id: AtomicU64::new(1),
            next_pending_generation: AtomicU64::new(1),
            stderr: StdMutex::new(String::new()),
            running: AtomicBool::new(true),
            #[cfg(test)]
            cancel_cleanup_done: Notify::new(),
        });

        let writer_shared = Arc::clone(&shared);
        let reader_shared = Arc::clone(&shared);
        let stderr_shared = Arc::clone(&shared);

        tokio::spawn(async move {
            writer_task(cmd_rx, stdin, writer_shared).await;
        });
        let reader_handle = tokio::spawn(async move {
            reader_task(stdout, reader_shared).await;
        });
        tokio::spawn(async move {
            stderr_task(stderr, stderr_shared).await;
        });

        Self {
            cmd_tx: Mutex::new(Some(cmd_tx)),
            shared,
            child: Mutex::new(child),
            reader_handle: Mutex::new(Some(reader_handle)),
        }
    }

    /// Whether the host pipe is still believed alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    async fn stall_outbound_for_test(
        &self,
    ) -> (mpsc::Receiver<Frame>, Option<mpsc::Sender<Frame>>) {
        let (tx, rx) = mpsc::channel(1);
        let original = self.cmd_tx.lock().await.replace(tx);
        (rx, original)
    }

    /// Latest retained stderr tail.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        stderr_of(&self.shared)
    }

    /// Subscribe to the unsolicited event stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.shared.events.subscribe()
    }

    fn next_id(&self) -> FrameId {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }
    fn remove_pending(shared: &Shared, id: FrameId) {
        if let Ok(mut pending) = shared.pending.lock() {
            pending.remove(&id);
        }
    }

    async fn send_frame(&self, frame: Frame) -> HostResult<()> {
        let tx = self.cmd_tx.lock().await.clone();
        match tx {
            Some(tx) => tx.send(frame).await.map_err(|e| HostClientError::Closed {
                message: format!("outbound send failed: {e}"),
                stderr: self.stderr_tail(),
            }),
            None => Err(HostClientError::NotRunning),
        }
    }

    /// Serialize a typed value into a `FrameKind::Res` frame and send it.
    ///
    /// `encode_label` appears in the [`HostClientError::Payload`] message when
    /// serialization fails, preserving the per-responder error text.
    async fn send_typed_response<T: serde::Serialize>(
        &self,
        id: FrameId,
        method: &str,
        value: &T,
        encode_label: &str,
    ) -> HostResult<()> {
        let payload = serde_json::to_value(value)
            .map_err(|error| HostClientError::Payload(format!("encode {encode_label}: {error}")))?;
        self.send_frame(Frame {
            id,
            kind: FrameKind::Res,
            method: method.to_owned(),
            payload,
        })
        .await
    }

    /// Send a `FrameKind::Error` frame built from an [`ErrorPayload`].
    ///
    /// `encode_label` appears in the [`HostClientError::Payload`] message when
    /// serialization fails, preserving the per-responder error text.
    async fn send_error_frame(
        &self,
        id: FrameId,
        method: &str,
        payload: crate::protocol::ErrorPayload,
        encode_label: &str,
    ) -> HostResult<()> {
        let payload = serde_json::to_value(payload)
            .map_err(|error| HostClientError::Payload(format!("encode {encode_label}: {error}")))?;
        self.send_frame(Frame {
            id,
            kind: FrameKind::Error,
            method: method.to_owned(),
            payload,
        })
        .await
    }

    /// Send a request and await its terminal response.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Timeout`], [`HostClientError::Closed`],
    /// [`HostClientError::Remote`], or [`HostClientError::NotRunning`].
    pub async fn request(
        &self,
        method: Method,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> HostResult<Frame> {
        self.request_raw(method.as_str(), payload, timeout).await
    }

    /// Send a request using an open lifecycle method string.
    ///
    /// # Errors
    ///
    /// See [`HostClient::request`].
    pub async fn request_raw(
        &self,
        method: &str,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> HostResult<Frame> {
        if !self.is_running() {
            return Err(HostClientError::NotRunning);
        }
        let id = self.next_id();
        let (tx, rx) = oneshot::channel::<FrameResult>();
        let generation = self.insert_pending(
            id,
            PendingEntry {
                terminal: Some(tx),
                stream: None,
                cancelling: false,
                generation: 0,
            },
        );
        let frame = Frame {
            id,
            kind: FrameKind::Req,
            method: method.to_owned(),
            payload,
        };
        if let Err(e) = self.send_frame(frame).await {
            Self::remove_pending(&self.shared, id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(HostClientError::Closed {
                message: "response channel closed".to_owned(),
                stderr: self.stderr_tail(),
            }),
            Err(_) => {
                if let Some(control_method) = cancel_method_for(method) {
                    let _ = cancel_pending(
                        &self.shared,
                        id,
                        generation,
                        self.cmd_tx.lock().await.clone(),
                        Some(control_method),
                        None,
                    );
                } else {
                    Self::remove_pending(&self.shared, id);
                }
                Err(HostClientError::Timeout { id, timeout })
            }
        }
    }

    /// Answer a correlated host-initiated UI request.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be encoded or sent.
    pub async fn respond_ui(&self, response: HostUiResponse) -> HostResult<()> {
        let (id, method, payload) = match response {
            HostUiResponse::Select { id, value } => (
                id,
                Method::Select,
                serde_json::to_value(SelectResponse { value }),
            ),
            HostUiResponse::Confirm { id, confirmed } => (
                id,
                Method::Confirm,
                serde_json::to_value(ConfirmResponse { confirmed }),
            ),
            HostUiResponse::Input { id, value } => (
                id,
                Method::Input,
                serde_json::to_value(InputResponse { value }),
            ),
            HostUiResponse::Editor { id, value } => (
                id,
                Method::Editor,
                serde_json::to_value(EditorResponse { value }),
            ),
        };
        let payload = payload
            .map_err(|error| HostClientError::Payload(format!("encode UI response: {error}")))?;
        self.send_frame(Frame::response(id, method, payload)).await
    }

    /// Answer a correlated `session.setModel` request from the host.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_set_model(&self, id: FrameId, success: bool) -> HostResult<()> {
        self.send_typed_response(
            id,
            crate::protocol::SESSION_SET_MODEL_METHOD,
            &crate::protocol::SessionSetModelResponse { success },
            "setModel response",
        )
        .await
    }

    /// Answer a correlated `session.compact` request from the host.
    ///
    /// Success sends the serialized `CompactionResult`; failure sends an
    /// error frame the host surfaces to the extension's `onError` callback.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_compact(
        &self,
        id: FrameId,
        outcome: Result<serde_json::Value, String>,
    ) -> HostResult<()> {
        match outcome {
            Ok(result) => {
                self.send_typed_response(
                    id,
                    crate::protocol::SESSION_COMPACT_METHOD,
                    &crate::protocol::SessionCompactResponse { result },
                    "compact response",
                )
                .await
            }
            Err(message) => {
                self.send_error_frame(
                    id,
                    crate::protocol::SESSION_COMPACT_METHOD,
                    crate::protocol::ErrorPayload::new("extension_error", &message),
                    "compact error",
                )
                .await
            }
        }
    }

    /// Answer a correlated `session.newSession` request from the host.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_new_session(
        &self,
        id: FrameId,
        cancelled: bool,
        token: Option<&str>,
    ) -> HostResult<()> {
        self.send_typed_response(
            id,
            crate::protocol::SESSION_NEW_SESSION_METHOD,
            &crate::protocol::SessionNewSessionResponse {
                cancelled,
                replacement_token: token.map(str::to_owned),
            },
            "newSession response",
        )
        .await
    }

    /// Answer a correlated `session.fork` request from the host.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_fork(
        &self,
        id: FrameId,
        cancelled: bool,
        selected_text: Option<&str>,
        token: Option<&str>,
    ) -> HostResult<()> {
        self.send_typed_response(
            id,
            crate::protocol::SESSION_FORK_METHOD,
            &crate::protocol::SessionForkResponse {
                cancelled,
                selected_text: selected_text.map(str::to_owned),
                replacement_token: token.map(str::to_owned),
            },
            "fork response",
        )
        .await
    }

    /// Answer a correlated `session.switchSession` request from the host.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_switch_session(
        &self,
        id: FrameId,
        cancelled: bool,
        token: Option<&str>,
    ) -> HostResult<()> {
        self.send_typed_response(
            id,
            crate::protocol::SESSION_SWITCH_SESSION_METHOD,
            &crate::protocol::SessionSwitchSessionResponse {
                cancelled,
                replacement_token: token.map(str::to_owned),
            },
            "switchSession response",
        )
        .await
    }

    /// Answer a correlated `session.navigateTree` request from the host.
    ///
    /// Success sends the typed navigation result; failure sends an
    /// `extension_error` frame the host surfaces to the extension.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_navigate_tree(
        &self,
        id: FrameId,
        outcome: Result<crate::protocol::SessionNavigateTreeResponse, String>,
    ) -> HostResult<()> {
        match outcome {
            Ok(result) => {
                self.send_typed_response(
                    id,
                    crate::protocol::SESSION_NAVIGATE_TREE_METHOD,
                    &result,
                    "navigateTree response",
                )
                .await
            }
            Err(message) => {
                self.send_error_frame(
                    id,
                    crate::protocol::SESSION_NAVIGATE_TREE_METHOD,
                    crate::protocol::ErrorPayload::new("extension_error", &message),
                    "navigateTree error",
                )
                .await
            }
        }
    }

    /// Answer a correlated `session.reload` request from the host.
    ///
    /// Success sends an optional ready-gate token; failure sends an
    /// `extension_error` frame.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the response cannot be
    /// encoded or sent.
    pub async fn respond_reload(
        &self,
        id: FrameId,
        outcome: Result<Option<&str>, String>,
    ) -> HostResult<()> {
        match outcome {
            Ok(token) => {
                self.send_typed_response(
                    id,
                    crate::protocol::SESSION_RELOAD_METHOD,
                    &crate::protocol::SessionReloadResponse {
                        replacement_token: token.map(str::to_owned),
                    },
                    "reload response",
                )
                .await
            }
            Err(message) => {
                self.send_error_frame(
                    id,
                    crate::protocol::SESSION_RELOAD_METHOD,
                    crate::protocol::ErrorPayload::new("extension_error", &message),
                    "reload error",
                )
                .await
            }
        }
    }

    /// Reject a correlated replacement request because another is pending.
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the error frame cannot be
    /// encoded or sent.
    pub async fn respond_replacement_busy(&self, id: FrameId, method: &str) -> HostResult<()> {
        self.send_error_frame(
            id,
            method,
            crate::protocol::ErrorPayload {
                code: "replacement_busy".to_owned(),
                message: "session replacement in progress".to_owned(),
                retryable: true,
                data: None,
            },
            "replacement_busy error",
        )
        .await
    }

    /// Reject a correlated session request with a non-retryable `extension_error`.
    ///
    /// Used for unclaimed newSession/fork/switchSession (and any other session
    /// method that needs a correlated failure without a typed success helper).
    ///
    /// # Errors
    ///
    /// Returns a payload or transport error when the error frame cannot be
    /// encoded or sent.
    pub async fn respond_session_error(
        &self,
        id: FrameId,
        method: &str,
        message: &str,
    ) -> HostResult<()> {
        self.send_error_frame(
            id,
            method,
            crate::protocol::ErrorPayload::new("extension_error", message),
            "session error",
        )
        .await
    }

    /// Send a fire-and-forget event frame (id 0) with an open method string.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::NotRunning`] or [`HostClientError::Closed`].
    pub async fn send_event(&self, method: &str, payload: serde_json::Value) -> HostResult<()> {
        if !self.is_running() {
            return Err(HostClientError::NotRunning);
        }
        self.send_frame(Frame {
            id: 0,
            kind: FrameKind::Event,
            method: method.to_owned(),
            payload,
        })
        .await
    }

    /// Open a streaming call: intermediate `event` frames with the request id
    /// as parent are delivered through [`StreamHandle::next_event`], and the
    /// terminal `res`/`error` frame resolves [`StreamHandle::finish`].
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::NotRunning`] or [`HostClientError::Closed`].
    pub async fn open_stream(
        &self,
        method: Method,
        payload: serde_json::Value,
        event_bound: usize,
    ) -> HostResult<StreamHandle> {
        self.open_stream_raw(method.as_str(), payload, event_bound)
            .await
    }

    /// Streaming variant using an open lifecycle method string.
    ///
    /// # Errors
    ///
    /// See [`HostClient::open_stream`].
    pub async fn open_stream_raw(
        &self,
        method: &str,
        payload: serde_json::Value,
        event_bound: usize,
    ) -> HostResult<StreamHandle> {
        if !self.is_running() {
            return Err(HostClientError::NotRunning);
        }
        let id = self.next_id();
        let bound = event_bound.clamp(1, STREAM_EVENT_CAPACITY * 8);
        let (terminal_tx, terminal_rx) = oneshot::channel::<FrameResult>();
        let (stream_tx, stream_rx) = mpsc::channel::<Frame>(bound);
        let generation = self.insert_pending(
            id,
            PendingEntry {
                terminal: Some(terminal_tx),
                stream: Some(stream_tx),
                cancelling: false,
                generation: 0,
            },
        );
        let frame = Frame {
            id,
            kind: FrameKind::Req,
            method: method.to_owned(),
            payload,
        };
        if let Err(e) = self.send_frame(frame).await {
            Self::remove_pending(&self.shared, id);
            return Err(e);
        }
        Ok(StreamHandle {
            id,
            generation,
            events: stream_rx,
            terminal: Some(terminal_rx),
            shared: Arc::clone(&self.shared),
            cmd_tx: self.cmd_tx.lock().await.clone(),
            cancel_method: cancel_method_for(method),
            consumed: false,
        })
    }

    /// Perform the `hello` handshake and validate versions.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Handshake`], [`HostClientError::Timeout`],
    /// or transport errors.
    pub async fn handshake(&self) -> HostResult<()> {
        let hello = Hello::local();
        let payload = serde_json::to_value(&hello)
            .map_err(|e| HostClientError::Payload(format!("encode hello: {e}")))?;
        let frame = self
            .request(Method::Hello, payload, HANDSHAKE_TIMEOUT)
            .await?;
        let ack: HelloAck =
            from_payload(&frame.payload).map_err(|e| HostClientError::Handshake {
                message: format!("decode helloAck: {e}"),
            })?;
        if ack.protocol_version != PROTOCOL_VERSION
            || ack.compatibility_version != COMPATIBILITY_VERSION
        {
            return Err(HostClientError::Handshake {
                message: format!(
                    "version mismatch: remote protocol={} compat={} (expected {}/{})",
                    ack.protocol_version,
                    ack.compatibility_version,
                    PROTOCOL_VERSION,
                    COMPATIBILITY_VERSION
                ),
            });
        }
        Ok(())
    }

    /// Request a slot measure for `width` / `theme_generation`.
    ///
    /// # Errors
    ///
    /// Returns transport or remote errors.
    pub async fn measure(
        &self,
        key: &str,
        width: u16,
        theme_generation: u64,
        timeout: Duration,
    ) -> HostResult<u16> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Req<'a> {
            key: &'a str,
            width: u16,
            theme_generation: u64,
        }
        let payload = serde_json::to_value(Req {
            key,
            width,
            theme_generation,
        })
        .map_err(|e| HostClientError::Payload(format!("encode measure: {e}")))?;
        let frame = self.request(Method::Measure, payload, timeout).await?;
        let resp: MeasureResponse = from_payload(&frame.payload)
            .map_err(|e| HostClientError::Payload(format!("decode measure: {e}")))?;
        Ok(resp.height)
    }

    fn insert_pending(&self, id: FrameId, mut entry: PendingEntry) -> u64 {
        let generation = self
            .shared
            .next_pending_generation
            .fetch_add(1, Ordering::Relaxed);
        entry.generation = generation;
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.insert(id, entry);
        }
        generation
    }

    /// Graceful shutdown: close stdin, wait the grace period, then kill + reap.
    ///
    /// Closing the outbound channel lets the writer task flush stdin and exit,
    /// so the host observes EOF and can exit cleanly before the grace deadline.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Closed`] only if the child cannot be reaped.
    pub async fn shutdown(&self) -> HostResult<()> {
        self.shared.running.store(false, Ordering::Relaxed);
        // Drop the outbound sender so the writer EOFs stdin.
        drop(self.cmd_tx.lock().await.take());
        let mut child_guard = self.child.lock().await;
        let reap = if let Some(child) = child_guard.take() {
            let reap = reap_child(child).await;
            drop(self.reader_handle.lock().await.take());
            reap
        } else {
            // In-memory transport: no child to reap. `running` is already
            // cleared; pending calls are failed below.
            Ok(())
        };
        // Fail any pending calls that survived transport teardown. The reader
        // task may have already drained them on EOF; this is a no-op then.
        fail_all(
            &self.shared,
            &HostClientError::Closed {
                message: "host shut down".to_owned(),
                stderr: stderr_of(&self.shared),
            },
        );
        reap
    }
}

/// Handle for a streaming call.
pub struct StreamHandle {
    id: FrameId,
    generation: u64,
    events: mpsc::Receiver<Frame>,
    terminal: Option<oneshot::Receiver<FrameResult>>,
    shared: Arc<Shared>,
    cmd_tx: Option<mpsc::Sender<Frame>>,
    cancel_method: Option<&'static str>,
    consumed: bool,
}

impl StreamHandle {
    /// Request id of this call.
    #[must_use]
    pub fn id(&self) -> FrameId {
        self.id
    }

    /// Receive the next intermediate event frame, if any.
    ///
    /// Returns `None` when the stream closed (terminal resolved or host gone).
    pub async fn next_event(&mut self) -> Option<Frame> {
        self.events.recv().await
    }

    /// Send a cancel control frame for this call to the host.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Closed`] when the outbound pipe is broken.
    pub fn cancel(&mut self, control_method: &str) -> HostResult<()> {
        match cancel_pending(
            &self.shared,
            self.id,
            self.generation,
            self.cmd_tx.clone(),
            Some(control_method),
            None,
        ) {
            CancellationStart::Queued
            | CancellationStart::AlreadyCancelling
            | CancellationStart::QueuedInBackground => Ok(()),
            CancellationStart::Closed => Err(HostClientError::Closed {
                message: "cancel send failed: outbound pipe closed".to_owned(),
                stderr: stderr_of(&self.shared),
            }),
            CancellationStart::NotRunning => Err(HostClientError::NotRunning),
        }
    }

    /// Await the terminal response with a deadline.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Timeout`], [`HostClientError::Closed`], or
    /// the remote error payload.
    pub async fn finish(mut self, timeout: Duration) -> HostResult<Frame> {
        let terminal = self.terminal.take().ok_or(HostClientError::NotRunning)?;
        match tokio::time::timeout(timeout, terminal).await {
            Ok(Ok(r)) => {
                self.consumed = true;
                r
            }
            Ok(Err(_)) => {
                self.consumed = true;
                Err(HostClientError::Closed {
                    message: "stream terminal closed".to_owned(),
                    stderr: stderr_of(&self.shared),
                })
            }
            // Leave `consumed` false so `Drop` owns cancellation delivery.
            Err(_) => Err(HostClientError::Timeout {
                id: self.id,
                timeout,
            }),
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        let _ = cancel_pending(
            &self.shared,
            self.id,
            self.generation,
            self.cmd_tx.clone(),
            self.cancel_method,
            None,
        );
    }
}

async fn reap_child(mut child: Child) -> HostResult<()> {
    if let Ok(Ok(_status)) = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
        Ok(())
    } else {
        let _ = child.kill().await;
        match child.wait().await {
            Ok(_) => Ok(()),
            Err(e) => Err(HostClientError::Closed {
                message: format!("reap failed: {e}"),
                stderr: String::new(),
            }),
        }
    }
}

async fn writer_task(
    mut rx: mpsc::Receiver<Frame>,
    mut stdin: Box<dyn AsyncWrite + Unpin + Send>,
    shared: Arc<Shared>,
) {
    let mut failed = false;
    while let Some(frame) = rx.recv().await {
        match encode_frame(&frame) {
            Ok(bytes) => {
                let wrote = stdin.write_all(&bytes).await;
                let flushed = if wrote.is_ok() {
                    stdin.flush().await
                } else {
                    wrote
                };
                if flushed.is_err() {
                    failed = true;
                    break;
                }
            }
            Err(e) => {
                fail_one(&shared, frame.id, HostClientError::Payload(e.to_string()));
            }
        }
    }
    let _ = stdin.flush().await;
    if failed {
        shared.running.store(false, Ordering::Relaxed);
        fail_all(
            &shared,
            &HostClientError::Closed {
                message: "host stdin write failed".to_owned(),
                stderr: stderr_of(&shared),
            },
        );
    }
}

async fn reader_task(stdout: Box<dyn AsyncRead + Unpin + Send>, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stdout);
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                fail_all(
                    &shared,
                    &HostClientError::Closed {
                        message: "host stdout reached EOF".to_owned(),
                        stderr: stderr_of(&shared),
                    },
                );
                // Clear `running` BEFORE broadcasting: a subscriber that
                // misses this send must observe the flag when it probes.
                shared.running.store(false, Ordering::Relaxed);
                let _ = shared.events.send(HostEvent::Eof);
                break;
            }
            Ok(n) => match decoder.push(&buf[..n]) {
                Ok(frames) => {
                    for frame in frames {
                        dispatch(&shared, frame);
                    }
                }
                Err(e) => {
                    fail_all(
                        &shared,
                        &HostClientError::Protocol {
                            message: e.to_string(),
                            stderr: stderr_of(&shared),
                        },
                    );
                    shared.running.store(false, Ordering::Relaxed);
                    let _ = shared.events.send(HostEvent::ProtocolError(e.to_string()));
                    break;
                }
            },
            Err(e) => {
                fail_all(
                    &shared,
                    &HostClientError::Closed {
                        message: format!("stdout read error: {e}"),
                        stderr: stderr_of(&shared),
                    },
                );
                // Typed fatal event so the product pump tears the host down
                // exactly like EOF / protocol death.
                shared.running.store(false, Ordering::Relaxed);
                let _ = shared
                    .events
                    .send(HostEvent::ProtocolError(format!("stdout read error: {e}")));
                break;
            }
        }
    }
}

async fn stderr_task(stderr: Box<dyn AsyncRead + Unpin + Send>, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Ok(mut tail) = shared.stderr.lock() {
                    tail.push_str(&line);
                    let len = tail.len();
                    if len > STDERR_TAIL_BYTES {
                        let drop = len - STDERR_TAIL_BYTES;
                        tail.drain(..drop);
                    }
                }
            }
        }
    }
}
fn stderr_of(shared: &Shared) -> String {
    match shared.stderr.lock() {
        Ok(guard) => guard.clone(),
        Err(e) => e.into_inner().clone(),
    }
}

fn fail_one(shared: &Shared, id: FrameId, err: HostClientError) {
    let entry = take_pending(shared, id);
    if let Some(entry) = entry
        && let Some(tx) = entry.terminal
    {
        let _ = tx.send(Err(err));
    }
}

fn fail_all(shared: &Shared, err: &HostClientError) {
    let entries: Vec<PendingEntry> = if let Ok(mut pending) = shared.pending.lock() {
        pending.drain().map(|(_, v)| v).collect()
    } else {
        Vec::new()
    };
    for entry in entries {
        if let Some(tx) = entry.terminal {
            let _ = tx.send(Err(err.clone()));
        }
    }
}

fn take_pending(shared: &Shared, id: FrameId) -> Option<PendingEntry> {
    if let Ok(mut pending) = shared.pending.lock() {
        pending.remove(&id)
    } else {
        None
    }
}

/// Takes an active route while leaving a cancellation-owned route in place.
fn take_active_pending(shared: &Shared, id: FrameId) -> Option<PendingEntry> {
    if let Ok(mut pending) = shared.pending.lock()
        && pending.get(&id).is_some_and(|entry| !entry.cancelling)
    {
        pending.remove(&id)
    } else {
        None
    }
}

fn remove_cancelling_pending(shared: &Shared, id: FrameId, generation: u64) {
    if let Ok(mut pending) = shared.pending.lock()
        && pending
            .get(&id)
            .is_some_and(|entry| entry.cancelling && entry.generation == generation)
    {
        pending.remove(&id);
    }
}

/// Marks one pending route as cancelling and queues exactly one control frame.
/// On a full queue, the retained route is removed only after the spawned send
/// has queued that frame or observed channel closure.
fn cancel_pending(
    shared: &Arc<Shared>,
    id: FrameId,
    generation: u64,
    cmd_tx: Option<mpsc::Sender<Frame>>,
    control_method: Option<&str>,
    terminal_error: Option<HostClientError>,
) -> CancellationStart {
    let terminal = if let Ok(mut pending) = shared.pending.lock() {
        let Some(entry) = pending.get_mut(&id) else {
            return CancellationStart::AlreadyCancelling;
        };
        // A missing or generation-mismatched route is already gone or replaced
        // by a newer same-id entry; never cancel the wrong generation.
        if entry.generation != generation || entry.cancelling {
            return CancellationStart::AlreadyCancelling;
        }
        entry.cancelling = true;
        if terminal_error.is_some() {
            entry.terminal.take()
        } else {
            None
        }
    } else {
        return CancellationStart::AlreadyCancelling;
    };

    if let Some(err) = terminal_error
        && let Some(terminal) = terminal
    {
        let _ = terminal.send(Err(err));
    }

    let (Some(tx), Some(control_method)) = (cmd_tx, control_method) else {
        remove_cancelling_pending(shared, id, generation);
        return CancellationStart::NotRunning;
    };
    let cancel = cancel_frame(id, control_method);
    match tx.try_send(cancel) {
        Ok(()) => {
            remove_cancelling_pending(shared, id, generation);
            CancellationStart::Queued
        }
        Err(mpsc::error::TrySendError::Full(cancel)) => {
            let runtime = shared.runtime.clone();
            let shared = Arc::clone(shared);
            runtime.spawn(async move {
                let _ = tx.send(cancel).await;
                remove_cancelling_pending(&shared, id, generation);
                #[cfg(test)]
                shared.cancel_cleanup_done.notify_one();
            });
            CancellationStart::QueuedInBackground
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            remove_cancelling_pending(shared, id, generation);
            CancellationStart::Closed
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch(shared: &Shared, frame: Frame) {
    if let Err(e) = frame.validate(false) {
        let _ = shared.events.send(HostEvent::ProtocolError(e.to_string()));
        return;
    }
    let id = frame.id;
    match frame.kind {
        FrameKind::Res => {
            if let Some(entry) = take_active_pending(shared, id)
                && let Some(tx) = entry.terminal
            {
                let _ = tx.send(Ok(frame));
            }
        }
        FrameKind::Error => {
            if id == 0 {
                let _ = shared.events.send(HostEvent::Raw(frame));
            } else {
                let err = remote_error(&frame);
                if let Some(entry) = take_active_pending(shared, id)
                    && let Some(tx) = entry.terminal
                {
                    let _ = tx.send(Err(err));
                }
            }
        }
        FrameKind::Event => {
            if id == 0 {
                forward_event(shared, frame);
            } else {
                forward_stream_event(shared, frame);
            }
        }
        FrameKind::Req => {
            if frame.method == crate::protocol::SESSION_SET_MODEL_METHOD {
                match from_payload::<crate::protocol::SessionSetModelRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::SetModelRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_COMPACT_METHOD {
                match from_payload::<crate::protocol::SessionCompactRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::CompactRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_NEW_SESSION_METHOD {
                match from_payload::<crate::protocol::SessionNewSessionRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::NewSessionRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_FORK_METHOD {
                match from_payload::<crate::protocol::SessionForkRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::ForkRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_NAVIGATE_TREE_METHOD {
                match from_payload::<crate::protocol::SessionNavigateTreeRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::NavigateTreeRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_SWITCH_SESSION_METHOD {
                match from_payload::<crate::protocol::SessionSwitchSessionRequest>(&frame.payload) {
                    Ok(request) => {
                        let _ = shared.events.send(HostEvent::SwitchSessionRequest {
                            id: frame.id,
                            request,
                        });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if frame.method == crate::protocol::SESSION_RELOAD_METHOD {
                match from_payload::<crate::protocol::SessionReloadRequest>(&frame.payload) {
                    Ok(_request) => {
                        let _ = shared
                            .events
                            .send(HostEvent::ReloadRequest { id: frame.id });
                    }
                    Err(_) => {
                        let _ = shared.events.send(HostEvent::Raw(frame));
                    }
                }
            } else if let Some(request) = decode_ui_request(&frame) {
                let _ = shared.events.send(HostEvent::UiRequest(request));
            } else {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    }
}

fn forward_event(shared: &Shared, frame: Frame) {
    let method = frame.method.as_str();
    if method == Method::UiSlot.as_str() {
        match from_payload::<crate::protocol::UiSlot>(&frame.payload) {
            Ok(slot) => {
                if accept_generation(shared, &slot.key, slot.generation) {
                    let _ = shared.events.send(HostEvent::UiSlot(slot));
                }
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == Method::DisposeSlot.as_str() {
        match from_payload::<crate::protocol::DisposeSlot>(&frame.payload) {
            Ok(dispose) => {
                if accept_dispose(shared, &dispose.key, dispose.generation) {
                    let _ = shared.events.send(HostEvent::DisposeSlot(dispose));
                }
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == Method::ToolUpdate.as_str() {
        match from_payload::<crate::protocol::ToolUpdate>(&frame.payload) {
            Ok(t) => {
                let _ = shared.events.send(HostEvent::ToolUpdate(t));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == Method::ProviderEvent.as_str() {
        match from_payload::<crate::protocol::ProviderEvent>(&frame.payload) {
            Ok(p) => {
                let _ = shared.events.send(HostEvent::ProviderEvent(p));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == Method::ExtensionError.as_str() {
        match from_payload::<crate::protocol::ExtensionErrorEvent>(&frame.payload) {
            Ok(e) => {
                let _ = shared.events.send(HostEvent::ExtensionError(e));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == Method::Notify.as_str() {
        match from_payload::<NotifyRequest>(&frame.payload) {
            Ok(notification) => {
                let _ = shared.events.send(HostEvent::Notify(notification));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == crate::protocol::THEME_SET_METHOD {
        match from_payload::<crate::protocol::ThemeSet>(&frame.payload) {
            Ok(set) => {
                let _ = shared.events.send(HostEvent::ThemeSet(set));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == crate::protocol::SESSION_COMMAND_METHOD {
        match from_payload::<crate::protocol::SessionCommand>(&frame.payload) {
            Ok(command) => {
                let _ = shared.events.send(HostEvent::SessionCommand(command));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == crate::protocol::UI_CONTROL_METHOD {
        match from_payload::<crate::protocol::UiControl>(&frame.payload) {
            Ok(control) => {
                let _ = shared.events.send(HostEvent::UiControl(control));
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else if method == crate::protocol::SESSION_REPLACEMENT_READY_METHOD {
        match from_payload::<crate::protocol::SessionReplacementReadyEvent>(&frame.payload) {
            Ok(ready) => {
                let _ = shared
                    .events
                    .send(HostEvent::ReplacementReady { token: ready.token });
            }
            Err(_) => {
                let _ = shared.events.send(HostEvent::Raw(frame));
            }
        }
    } else {
        let _ = shared.events.send(HostEvent::Raw(frame));
    }
}

fn forward_stream_event(shared: &Shared, frame: Frame) {
    let id = frame.id;
    let stream = if let Ok(pending) = shared.pending.lock() {
        pending
            .get(&id)
            .filter(|entry| !entry.cancelling)
            .and_then(|entry| entry.stream.clone())
    } else {
        None
    };
    if let Some(stream) = stream {
        // Non-blocking: a full channel drops the stale event (backpressure).
        let _ = stream.try_send(frame);
    }
}

fn accept_generation(shared: &Shared, key: &str, generation: u64) -> bool {
    if let Ok(mut generations) = shared.slot_generations.lock() {
        let prev = generations.insert(key.to_owned(), generation);
        match prev {
            Some(prev) if prev > generation => {
                generations.insert(key.to_owned(), prev);
                false
            }
            _ => true,
        }
    } else {
        true
    }
}

/// Accept a dispose unless it names a generation older than the accepted
/// current one (a reordered stale dispose must not kill a live slot). An
/// absent generation is an unconditional dispose. Accepted disposes clear the
/// tracked generation so the key starts fresh.
fn accept_dispose(shared: &Shared, key: &str, generation: Option<u64>) -> bool {
    let Ok(mut generations) = shared.slot_generations.lock() else {
        return false;
    };
    if let (Some(generation), Some(current)) = (generation, generations.get(key).copied())
        && generation < current
    {
        return false;
    }
    generations.remove(key);
    true
}

fn remote_error(frame: &Frame) -> HostClientError {
    match from_payload::<crate::protocol::ErrorPayload>(&frame.payload) {
        Ok(ep) => HostClientError::Remote {
            code: ep.code,
            message: ep.message,
        },
        Err(_) => HostClientError::Remote {
            code: "unknown".to_owned(),
            message: format!("unparseable error frame for method {}", frame.method),
        },
    }
}

fn decode_ui_request(frame: &Frame) -> Option<HostUiRequest> {
    match Method::parse(&frame.method)? {
        Method::Select => from_payload(&frame.payload)
            .ok()
            .map(|request| HostUiRequest::Select {
                id: frame.id,
                request,
            }),
        Method::Confirm => {
            from_payload(&frame.payload)
                .ok()
                .map(|request| HostUiRequest::Confirm {
                    id: frame.id,
                    request,
                })
        }
        Method::Input => from_payload(&frame.payload)
            .ok()
            .map(|request| HostUiRequest::Input {
                id: frame.id,
                request,
            }),
        Method::Editor => from_payload(&frame.payload)
            .ok()
            .map(|request| HostUiRequest::Editor {
                id: frame.id,
                request,
            }),
        _ => None,
    }
}

fn cancel_method_for(method: &str) -> Option<&'static str> {
    match method {
        "tool.execute" => Some("tool.cancel"),
        "provider.stream" => Some("provider.cancel"),
        _ => None,
    }
}

fn cancel_frame(id: FrameId, control_method: &str) -> Frame {
    Frame {
        id: 0,
        kind: FrameKind::Event,
        method: control_method.to_owned(),
        payload: serde_json::json!({ "id": id }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostSource, HostSpec};
    use crate::protocol::{ErrorPayload, SlotPlacement, StyledRun, UiSlot, to_payload};
    use std::error::Error;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    type R = Result<(), Box<dyn Error>>;

    use crate::test_support::make_pair;

    #[tokio::test]
    async fn handshake_succeeds() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move { client.handshake().await });
        host.answer_hello().await?;
        assert!(client_task.await?.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn handshake_rejects_version_mismatch() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move { client.handshake().await });
        let req = host.read_frame().await.ok_or("no hello")?;
        let bad = Frame::response(
            req.id,
            Method::Hello,
            serde_json::json!({
                "protocolVersion": 99,
                "compatibilityVersion": COMPATIBILITY_VERSION,
            }),
        );
        host.write_frame(&bad).await?;
        let result = client_task.await?;
        assert!(
            matches!(result, Err(HostClientError::Handshake { .. })),
            "expected Handshake error, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn request_response_roundtrip() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({"m":"hi"}),
                    Duration::from_secs(2),
                )
                .await
        });
        let req = host.read_frame().await.ok_or("no req")?;
        assert_eq!(req.payload["m"], "hi");
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({"ok":true}));
        host.write_frame(&res).await?;
        let frame = client_task.await??;
        assert_eq!(frame.payload["ok"], true);
        Ok(())
    }
    #[tokio::test]
    async fn theme_set_event_is_typed_and_send_event_writes_id_zero() -> R {
        let (client, mut host) = make_pair().await;
        let mut events = client.subscribe();

        // Host → client: theme.set arrives typed, not as a fatal Raw frame.
        host.write_frame(&Frame {
            id: 0,
            kind: FrameKind::Event,
            method: crate::protocol::THEME_SET_METHOD.to_owned(),
            payload: serde_json::json!({"name": "m3-light", "persist": true}),
        })
        .await?;
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await??;
        let HostEvent::ThemeSet(set) = event else {
            return Err(format!("expected typed theme.set, got {event:?}").into());
        };
        assert_eq!(set.name.as_deref(), Some("m3-light"));
        assert!(set.persist);

        // Client → host: send_event emits a fire-and-forget event frame.
        client
            .send_event(
                crate::protocol::THEME_UPDATE_METHOD,
                serde_json::json!({"themeGeneration": 7}),
            )
            .await?;
        let frame = host.read_frame().await.ok_or("no theme.update event")?;
        assert_eq!(frame.kind, FrameKind::Event);
        assert_eq!(frame.id, 0);
        assert_eq!(frame.method, crate::protocol::THEME_UPDATE_METHOD);
        assert_eq!(frame.payload["themeGeneration"], 7);
        Ok(())
    }
    #[tokio::test]
    async fn host_ui_requests_and_notifications_are_typed_and_correlated() -> R {
        let (client, mut host) = make_pair().await;
        let mut events = client.subscribe();
        host.write_frame(&Frame {
            id: 77,
            kind: FrameKind::Req,
            method: Method::Select.as_str().to_owned(),
            payload: serde_json::json!({
                "title": "Pick",
                "options": ["a", "b"],
                "timeoutMs": 50
            }),
        })
        .await?;

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await??;
        let HostEvent::UiRequest(HostUiRequest::Select { id, request }) = event else {
            return Err("expected typed select request".into());
        };
        assert_eq!(id, 77);
        assert_eq!(request.options, ["a", "b"]);
        client
            .respond_ui(HostUiResponse::Select {
                id,
                value: Some("b".to_owned()),
            })
            .await?;
        let response = host.read_frame().await.ok_or("no UI response")?;
        assert_eq!(response.kind, FrameKind::Res);
        assert_eq!(response.id, 77);
        assert_eq!(response.payload["value"], "b");

        host.write_frame(&Frame::event(
            0,
            Method::Notify,
            serde_json::json!({"message":"hello","level":"warning"}),
        ))
        .await?;
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await??;
        let HostEvent::Notify(notification) = event else {
            return Err("expected typed notification".into());
        };
        assert_eq!(notification.message, "hello");
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_requests_all_resolve() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            let mut futs = Vec::new();
            for i in 1..=8u64 {
                futs.push(client.request(
                    Method::Notify,
                    serde_json::json!({"i": i}),
                    Duration::from_secs(3),
                ));
            }
            futures::future::join_all(futs).await
        });
        // Answer each incoming request, possibly out of order.
        for _ in 0..8usize {
            let req = host.read_frame().await.ok_or("no req")?;
            let i = req.payload["i"].as_u64().ok_or("no i")?;
            let res = Frame::response(req.id, Method::Notify, serde_json::json!({"i": i}));
            host.write_frame(&res).await?;
        }
        let results = client_task.await?;
        assert_eq!(results.len(), 8);
        for r in results {
            assert!(r.is_ok(), "request failed: {r:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn request_times_out_without_answer() {
        let (client, _host) = make_pair().await;
        let result = client
            .request(
                Method::Notify,
                serde_json::json!({}),
                Duration::from_millis(40),
            )
            .await;
        assert!(
            matches!(result, Err(HostClientError::Timeout { .. })),
            "expected Timeout, got {result:?}"
        );
    }

    #[tokio::test]
    async fn timed_out_tool_request_sends_cancel_before_returning() -> R {
        let (client, mut host) = make_pair().await;
        let task = tokio::spawn(async move {
            client
                .request_raw(
                    "tool.execute",
                    serde_json::json!({}),
                    Duration::from_millis(40),
                )
                .await
        });
        let request = host.read_frame().await.ok_or("no tool request")?;
        let cancel = host.read_frame().await.ok_or("no timeout cancel")?;
        assert_eq!(cancel.method, "tool.cancel");
        assert_eq!(cancel.payload["id"], request.id);
        assert!(matches!(task.await?, Err(HostClientError::Timeout { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn late_response_after_timeout_is_dropped() -> R {
        let (client, mut host) = make_pair().await;
        let id_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({}),
                    Duration::from_millis(40),
                )
                .await
        });
        let req = host.read_frame().await.ok_or("no req")?;
        // Wait for the client to time out.
        assert!(id_task.await?.is_err());
        // Now send a late response — must not panic or hang.
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({"late":true}));
        assert!(host.write_frame(&res).await.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn stream_delivers_events_then_terminal() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            let mut stream = client
                .open_stream_raw("tool.execute", serde_json::json!({}), 8)
                .await?;
            let mut events = Vec::new();
            while let Some(frame) = stream.next_event().await {
                events.push(frame.method.clone());
                if events.len() == 2 {
                    break;
                }
            }
            let terminal = stream.finish(Duration::from_secs(2)).await?;
            Ok::<_, HostClientError>((events, terminal))
        });
        let req = host.read_frame().await.ok_or("no req")?;
        let ev1 = Frame::event(req.id, Method::ToolUpdate, serde_json::json!({"n":1}));
        let ev2 = Frame::event(req.id, Method::ToolUpdate, serde_json::json!({"n":2}));
        host.write_frame(&ev1).await?;
        host.write_frame(&ev2).await?;
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({"done":true}));
        host.write_frame(&res).await?;
        let (events, terminal) = client_task.await??;
        assert_eq!(events.len(), 2);
        assert_eq!(terminal.payload["done"], true);
        Ok(())
    }

    #[tokio::test]
    async fn timed_out_provider_stream_sends_cancel_before_returning() -> R {
        let (client, mut host) = make_pair().await;
        let task = tokio::spawn(async move {
            let stream = client
                .open_stream_raw("provider.stream", serde_json::json!({}), 2)
                .await?;
            stream.finish(Duration::from_millis(40)).await
        });
        let request = host.read_frame().await.ok_or("no provider request")?;
        let cancel = host.read_frame().await.ok_or("no stream timeout cancel")?;
        assert_eq!(cancel.method, "provider.cancel");
        assert_eq!(cancel.payload["id"], request.id);
        assert!(matches!(task.await?, Err(HostClientError::Timeout { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn stream_backpressure_drops_excess_no_deadlock() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            let mut stream = client
                .open_stream_raw("tool.execute", serde_json::json!({}), 2)
                .await?;
            // Do not drain events immediately: a flood must not deadlock the host.
            tokio::time::sleep(Duration::from_millis(60)).await;
            let mut received = Vec::new();
            while let Some(frame) = stream.next_event().await {
                received.push(
                    frame.payload["n"]
                        .as_u64()
                        .ok_or_else(|| HostClientError::Payload("missing event index".into()))?,
                );
            }
            let terminal = stream.finish(Duration::from_secs(2)).await?;
            Ok::<_, HostClientError>((received, terminal))
        });
        let req = host.read_frame().await.ok_or("no req")?;
        // A bound-two channel retains a FIFO prefix (≤2 events) and drops the rest.
        for n in 0..20u64 {
            let ev = Frame::event(req.id, Method::ToolUpdate, serde_json::json!({"n": n}));
            host.write_frame(&ev).await?;
        }
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({"done": true}));
        host.write_frame(&res).await?;
        let (received, terminal) = client_task.await??;
        assert!(
            received.len() <= 2,
            "a bound-two channel retained {} events: {received:?}",
            received.len()
        );
        assert!(
            received.windows(2).all(|pair| pair[0] < pair[1]),
            "retained events must stay in FIFO order: {received:?}"
        );
        assert_eq!(
            received.first(),
            Some(&0),
            "the first event is never dropped"
        );
        assert_eq!(terminal.payload["done"], true);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_outbound_drop_queues_one_cancel_before_pending_removal() -> R {
        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let stream = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let id = stream.id();
        let shared = Arc::clone(&stream.shared);

        // The request already occupies the one-slot writer queue. Dropping the
        // stream must retain its correlation state while its cancel waits.
        drop(stream);
        assert!(
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .is_some_and(|entry| entry.cancelling),
            "the pending route remains cancellation-owned until the queued send completes"
        );

        let request = tokio::time::timeout(Duration::from_millis(100), stalled.recv())
            .await
            .map_err(|_| "missing queued stream request")?
            .ok_or("missing queued stream request")?;
        assert_eq!(request.id, id);

        let cancel = tokio::time::timeout(Duration::from_millis(100), stalled.recv())
            .await
            .map_err(|_| "queued cancellation did not reach the writer")?
            .ok_or("queued cancellation did not reach the writer")?;
        assert_eq!(cancel.method, "provider.cancel");
        assert_eq!(cancel.payload["id"], id);
        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                let pending = shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&id);
                if !pending {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "pending route was not released after cancellation queued")?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_cancel_does_not_remove_replaced_pending_under_same_id() -> R {
        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let stream = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let id = stream.id();
        let shared = Arc::clone(&stream.shared);
        let old_generation = stream.generation;

        // The request already occupies the one-slot writer queue. Dropping the
        // stream retains its correlation state while its cancel waits on the
        // saturated channel (background send path).
        drop(stream);
        assert!(
            shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .is_some_and(|entry| entry.cancelling && entry.generation == old_generation),
            "the pending route remains cancellation-owned by the old generation"
        );

        // Replace the pending route under the same frame id with a fresh,
        // non-cancelling entry carrying a newer generation.
        let new_generation = shared
            .next_pending_generation
            .fetch_add(1, Ordering::Relaxed);
        {
            let mut pending = shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.insert(
                id,
                PendingEntry {
                    terminal: None,
                    stream: None,
                    cancelling: false,
                    generation: new_generation,
                },
            );
        }

        // Drain the stalled channel: first the original request, then the
        // cancel the background send queued. The background cleanup runs
        // after its send completes.
        let request = tokio::time::timeout(Duration::from_millis(100), stalled.recv())
            .await
            .map_err(|_| "missing queued stream request")?
            .ok_or("missing queued stream request")?;
        assert_eq!(request.id, id);

        let cancel = tokio::time::timeout(Duration::from_millis(100), stalled.recv())
            .await
            .map_err(|_| "queued cancellation did not reach the writer")?
            .ok_or("queued cancellation did not reach the writer")?;
        assert_eq!(cancel.method, "provider.cancel");
        assert_eq!(cancel.payload["id"], id);

        // Await the background cleanup completion signal before asserting.
        // The cleanup task calls `notify_one` after `remove_cancelling_pending`;
        // awaiting it proves the cleanup actually ran, so the test cannot pass
        // merely because cancellation was requested while cleanup never ran.
        tokio::time::timeout(
            Duration::from_millis(500),
            shared.cancel_cleanup_done.notified(),
        )
        .await
        .map_err(|_| "background cancel cleanup did not complete")?;

        let (survives, surviving_gen, cancelling) = {
            let pending = shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = pending.get(&id);
            (
                entry.is_some(),
                entry.map(|e| e.generation),
                entry.map(|e| e.cancelling),
            )
        };
        assert!(
            survives,
            "newer same-id pending entry was removed by stale cleanup"
        );
        assert_eq!(
            surviving_gen,
            Some(new_generation),
            "surviving entry is the replacement, not the cancelled one"
        );
        assert_eq!(
            cancelling,
            Some(false),
            "replacement entry must not be marked cancelling"
        );
        Ok(())
    }

    #[tokio::test]
    async fn generation_filter_discards_stale_slot() -> R {
        let (client, mut host) = make_pair().await;
        let mut sub = client.subscribe();
        // Drive a trivial exchange so the reader task runs.
        let client_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await
        });
        let req = host.read_frame().await.ok_or("no req")?;
        // Push gen 2, then gen 1 (stale), then gen 3.
        for generation in [2u64, 1, 3] {
            let slot = UiSlot {
                key: "w".to_owned(),
                generation,
                placement: SlotPlacement::AboveEditor,
                height: 1,
                runs: vec![vec![StyledRun {
                    text: format!("g{generation}"),
                    style: crate::protocol::Style::default(),
                }]],
                focusable: false,
                cursor: None,
                overlay_options: None,
            };
            let frame = Frame::event(0, Method::UiSlot, to_payload(&slot)?);
            host.write_frame(&frame).await?;
        }
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({}));
        host.write_frame(&res).await?;
        let _ = client_task.await??;
        // Collect UiSlot events.
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, sub.recv()).await {
            if let HostEvent::UiSlot(s) = ev {
                seen.push(s.generation);
            }
        }
        assert!(seen.contains(&2), "gen 2 must arrive: {seen:?}");
        assert!(!seen.contains(&1), "stale gen 1 must be dropped: {seen:?}");
        assert!(seen.contains(&3), "gen 3 must arrive: {seen:?}");
        Ok(())
    }

    #[tokio::test]
    async fn reordered_stale_dispose_does_not_kill_live_slot() -> R {
        let (client, mut host) = make_pair().await;
        let mut sub = client.subscribe();
        let client_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await
        });
        let req = host.read_frame().await.ok_or("no req")?;
        let slot = |generation: u64| UiSlot {
            key: "w".to_owned(),
            generation,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![StyledRun {
                text: format!("g{generation}"),
                style: crate::protocol::Style::default(),
            }]],
            focusable: false,
            cursor: None,
            overlay_options: None,
        };
        // Live slot at gen 2, then a REORDERED stale dispose for gen 1.
        host.write_frame(&Frame::event(0, Method::UiSlot, to_payload(&slot(2))?))
            .await?;
        host.write_frame(&Frame::event(
            0,
            Method::DisposeSlot,
            to_payload(&crate::protocol::DisposeSlot {
                key: "w".to_owned(),
                generation: Some(1),
            })?,
        ))
        .await?;
        // Old generation stays rejected after the ignored dispose.
        host.write_frame(&Frame::event(0, Method::UiSlot, to_payload(&slot(1))?))
            .await?;
        // A current-generation dispose is honored.
        host.write_frame(&Frame::event(
            0,
            Method::DisposeSlot,
            to_payload(&crate::protocol::DisposeSlot {
                key: "w".to_owned(),
                generation: Some(2),
            })?,
        ))
        .await?;
        host.write_frame(&Frame::response(
            req.id,
            Method::Notify,
            serde_json::json!({}),
        ))
        .await?;
        let _ = client_task.await??;

        let mut slots = Vec::new();
        let mut disposes = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, sub.recv()).await {
            match event {
                HostEvent::UiSlot(slot) => slots.push(slot.generation),
                HostEvent::DisposeSlot(dispose) => disposes.push(dispose.generation),
                _ => {}
            }
        }
        assert_eq!(slots, vec![2], "live slot survives; stale push rejected");
        assert_eq!(
            disposes,
            vec![Some(2)],
            "stale dispose dropped, current dispose forwarded"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stdout_read_error_emits_fatal_protocol_event() -> R {
        struct FailingReader;
        impl tokio::io::AsyncRead for FailingReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("injected stdout failure")))
            }
        }

        let (client_to_host, _host_from_client) = tokio::io::duplex(4096);
        let (client_err, _host_err) = tokio::io::duplex(4096);
        let client = HostClient::connect_boxed(
            Box::new(client_to_host),
            Box::new(FailingReader),
            Box::new(client_err),
            None,
        );
        let mut events = client.subscribe();
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await??;
        let HostEvent::ProtocolError(message) = event else {
            return Err(format!("expected ProtocolError, got {event:?}").into());
        };
        assert!(
            message.contains("stdout read error"),
            "unexpected message: {message}"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while client.is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }
    #[tokio::test]
    async fn crash_eof_fails_pending_with_closed() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({}),
                    Duration::from_secs(3),
                )
                .await
        });
        let _req = host.read_frame().await.ok_or("no req")?;
        // Host dies: close the write half → client reader observes EOF.
        host.close().await;
        let result = client_task.await?;
        assert!(
            matches!(result, Err(HostClientError::Closed { .. })),
            "expected Closed, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_error_frame_maps_to_remote() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            client
                .request(
                    Method::Notify,
                    serde_json::json!({}),
                    Duration::from_secs(3),
                )
                .await
        });
        let req = host.read_frame().await.ok_or("no req")?;
        let err = ErrorPayload::new("E_FAIL", "nope");
        let frame = Frame::error_frame(req.id, Method::Notify, &err)?;
        host.write_frame(&frame).await?;
        match client_task.await? {
            Err(HostClientError::Remote { code, message }) => {
                assert_eq!(code, "E_FAIL");
                assert_eq!(message, "nope");
            }
            other => return Err(format!("expected Remote, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn stderr_tail_captured() -> R {
        let (client_to_host, host_from_client) = tokio::io::duplex(4096);
        let (host_to_client, client_from_host) = tokio::io::duplex(4096);
        let (client_err, host_err) = tokio::io::duplex(4096);
        let client = HostClient::connect_boxed(
            Box::new(client_to_host),
            Box::new(client_from_host),
            Box::new(client_err),
            None,
        );
        // Fake host writes stderr then closes.
        let stderr_task = tokio::spawn(async move {
            let mut host_err = host_err;
            let _ = host_err.write_all(b"boom trace\n").await;
            let _ = host_err.shutdown().await;
            let _ = host_from_client;
            let _ = host_to_client;
        });
        stderr_task.await?;
        // Give the stderr reader time to drain.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(client.stderr_tail().contains("boom trace"));
        Ok(())
    }

    fn write_exit_script(dir: &std::path::Path) -> R {
        let script = dir.join("fake-host");
        fs::write(&script, "#!/bin/sh\nexit 0\n")?;
        let mut perms = fs::metadata(&script)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms)?;
        Ok(())
    }

    #[tokio::test]
    async fn process_death_detected_and_reaped() -> R {
        let dir = tempdir()?;
        write_exit_script(dir.path())?;
        let spec = HostSpec {
            source: HostSource::Env(dir.path().join("fake-host")),
            program: dir.path().join("fake-host"),
            args: Vec::new(),
        };
        let client = HostClient::spawn(&spec)?;
        // The script exits immediately; a request must fail (Closed/Timeout/NotRunning).
        let result = client
            .request(Method::Hello, serde_json::json!({}), Duration::from_secs(2))
            .await;
        assert!(
            matches!(
                result,
                Err(HostClientError::Closed { .. }
                    | HostClientError::Timeout { .. }
                    | HostClientError::NotRunning)
            ),
            "expected failure from dead host, got {result:?}"
        );
        let reap = client.shutdown().await;
        assert!(reap.is_ok(), "shutdown should reap cleanly: {reap:?}");
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_marks_not_running() -> R {
        let (client, _host) = make_pair().await;
        assert!(client.is_running());
        client.shutdown().await?;
        assert!(!client.is_running());
        // Further requests are NotRunning.
        let result = client
            .request(
                Method::Notify,
                serde_json::json!({}),
                Duration::from_millis(10),
            )
            .await;
        assert!(matches!(result, Err(HostClientError::NotRunning)));
        Ok(())
    }

    #[tokio::test]
    async fn responder_success_frames_preserve_shape() -> R {
        let (client, mut host) = make_pair().await;

        client.respond_set_model(1, true).await?;
        let f = host.require_frame("setModel res").await?;
        assert_eq!(f.id, 1);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_SET_MODEL_METHOD);
        assert_eq!(f.payload["success"], true);

        client
            .respond_compact(2, Ok(serde_json::json!({"summary": "ok"})))
            .await?;
        let f = host.require_frame("compact res").await?;
        assert_eq!(f.id, 2);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_COMPACT_METHOD);
        assert_eq!(f.payload["result"]["summary"], "ok");

        client.respond_new_session(3, false, Some("tok")).await?;
        let f = host.require_frame("newSession res").await?;
        assert_eq!(f.id, 3);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_NEW_SESSION_METHOD);
        assert_eq!(f.payload["cancelled"], false);
        assert_eq!(f.payload["replacementToken"], "tok");

        client
            .respond_fork(4, false, Some("sel"), Some("tok2"))
            .await?;
        let f = host.require_frame("fork res").await?;
        assert_eq!(f.id, 4);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_FORK_METHOD);
        assert_eq!(f.payload["selectedText"], "sel");
        assert_eq!(f.payload["replacementToken"], "tok2");

        client.respond_switch_session(5, true, None).await?;
        let f = host.require_frame("switchSession res").await?;
        assert_eq!(f.id, 5);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_SWITCH_SESSION_METHOD);
        assert_eq!(f.payload["cancelled"], true);
        assert!(f.payload.get("replacementToken").is_none());

        let nav = crate::protocol::SessionNavigateTreeResponse {
            cancelled: false,
            editor_text: Some("draft".to_owned()),
            aborted: None,
            summary_entry: None,
        };
        client.respond_navigate_tree(6, Ok(nav)).await?;
        let f = host.require_frame("navigateTree res").await?;
        assert_eq!(f.id, 6);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_NAVIGATE_TREE_METHOD);
        assert_eq!(f.payload["editorText"], "draft");

        client.respond_reload(7, Ok(Some("tok3"))).await?;
        let f = host.require_frame("reload res").await?;
        assert_eq!(f.id, 7);
        assert_eq!(f.kind, FrameKind::Res);
        assert_eq!(f.method, crate::protocol::SESSION_RELOAD_METHOD);
        assert_eq!(f.payload["replacementToken"], "tok3");

        Ok(())
    }

    #[tokio::test]
    async fn responder_error_frames_preserve_shape() -> R {
        let (client, mut host) = make_pair().await;

        client
            .respond_compact(10, Err("compaction failed".to_owned()))
            .await?;
        let f = host.require_frame("compact err").await?;
        assert_eq!(f.id, 10);
        assert_eq!(f.kind, FrameKind::Error);
        assert_eq!(f.method, crate::protocol::SESSION_COMPACT_METHOD);
        assert_eq!(f.payload["code"], "extension_error");
        assert_eq!(f.payload["message"], "compaction failed");
        assert_eq!(f.payload["retryable"], false);

        client
            .respond_navigate_tree(11, Err("entry not found".to_owned()))
            .await?;
        let f = host.require_frame("navigateTree err").await?;
        assert_eq!(f.id, 11);
        assert_eq!(f.kind, FrameKind::Error);
        assert_eq!(f.method, crate::protocol::SESSION_NAVIGATE_TREE_METHOD);
        assert_eq!(f.payload["code"], "extension_error");
        assert_eq!(f.payload["message"], "entry not found");
        assert_eq!(f.payload["retryable"], false);

        client
            .respond_reload(12, Err("reload failed".to_owned()))
            .await?;
        let f = host.require_frame("reload err").await?;
        assert_eq!(f.id, 12);
        assert_eq!(f.kind, FrameKind::Error);
        assert_eq!(f.method, crate::protocol::SESSION_RELOAD_METHOD);
        assert_eq!(f.payload["code"], "extension_error");
        assert_eq!(f.payload["message"], "reload failed");
        assert_eq!(f.payload["retryable"], false);

        client
            .respond_replacement_busy(13, crate::protocol::SESSION_NEW_SESSION_METHOD)
            .await?;
        let f = host.require_frame("replacement_busy err").await?;
        assert_eq!(f.id, 13);
        assert_eq!(f.kind, FrameKind::Error);
        assert_eq!(f.method, crate::protocol::SESSION_NEW_SESSION_METHOD);
        assert_eq!(f.payload["code"], "replacement_busy");
        assert_eq!(f.payload["message"], "session replacement in progress");
        assert_eq!(f.payload["retryable"], true);

        client
            .respond_session_error(14, crate::protocol::SESSION_FORK_METHOD, "unclaimed")
            .await?;
        let f = host.require_frame("session_error err").await?;
        assert_eq!(f.id, 14);
        assert_eq!(f.kind, FrameKind::Error);
        assert_eq!(f.method, crate::protocol::SESSION_FORK_METHOD);
        assert_eq!(f.payload["code"], "extension_error");
        assert_eq!(f.payload["message"], "unclaimed");
        assert_eq!(f.payload["retryable"], false);

        Ok(())
    }

    #[tokio::test]
    async fn shutdown_fails_pending_calls() -> R {
        let (client, _host) = make_pair().await;
        // Start a request that will never be answered by the host.
        let request_fut = client.request(
            Method::Notify,
            serde_json::json!({}),
            Duration::from_secs(30),
        );
        tokio::pin!(request_fut);
        // Let the request register in the pending map (synchronous prefix).
        tokio::select! {
            biased;
            _ = &mut request_fut => {
                return Err("request completed before shutdown — test setup issue".into());
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        // Shut down while the request is pending.
        client.shutdown().await?;
        // The request must resolve with an error, not hang.
        let result = tokio::time::timeout(Duration::from_secs(2), &mut request_fut)
            .await
            .map_err(|_| "request hung after shutdown")?;
        assert!(
            matches!(
                result,
                Err(HostClientError::Closed { .. } | HostClientError::NotRunning)
            ),
            "expected Closed or NotRunning after shutdown, got {result:?}"
        );
        // The pending map must be empty.
        let pending_len = client.shared.pending.lock().map_or(0, |p| p.len());
        assert_eq!(pending_len, 0, "pending map should be empty after shutdown");
        Ok(())
    }
}
