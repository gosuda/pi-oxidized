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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::host::{HostError, HostSpec};
use crate::protocol::{
    COMPATIBILITY_VERSION, ConfirmRequest, ConfirmResponse, EditorRequest, EditorResponse, Frame,
    FrameDecoder, FrameId, FrameKind, Hello, HelloAck, InputRequest, InputResponse, Method,
    NotifyRequest, PROTOCOL_VERSION, SelectRequest, SelectResponse, encode_frame, from_payload,
};

/// Default bounded capacity for the outbound (client → host) frame channel.
pub const OUTBOUND_CAPACITY: usize = 128;
/// Default bounded capacity for the unsolicited event broadcast.
pub const EVENT_CAPACITY: usize = 256;
/// Default bounded capacity for per-call streaming event channels.
pub const STREAM_EVENT_CAPACITY: usize = 64;
/// Provider ingress queues provider-event frames. The frame-count capacity
/// bounds queue depth; [`PROVIDER_FORWARD_BYTES`] independently bounds the
/// retained wire bytes so a slow consumer cannot accumulate gigabytes of
/// near-`MAX_FRAME_BYTES` frames before the count bound trips.
const PROVIDER_FORWARD_CAPACITY: usize = 16 * STREAM_EVENT_CAPACITY;
/// Maximum retained wire bytes across queued provider-event frames. One
/// frame may legally reach [`crate::protocol::MAX_FRAME_BYTES`], so this
/// budget — not the frame count — is the hard memory bound on ingress.
const PROVIDER_FORWARD_BYTES: usize = 32 * 1024 * 1024;
/// Maximum number of correlation ids retained by the cancellation drain.
/// Overflow remains represented by cancellation state on the existing pending route.
const CANCELLATION_BACKLOG_CAPACITY: usize = 4;
/// Grace period before killing the host on shutdown.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Default handshake timeout.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum retained stderr tail in bytes.
pub const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Result type for host client operations.
pub type HostResult<T> = Result<T, HostClientError>;

/// A terminal frame result delivered to a pending caller.
type FrameResult = HostResult<Frame>;

/// One in-flight call.
struct PendingEntry {
    /// Terminal response/error sender (taken on Res/Error).
    terminal: Option<oneshot::Sender<FrameResult>>,
    /// Optional streaming event sink for intermediate events.
    stream: Option<PendingStream>,
    /// Sender owned by this route for cancellation delivery.
    cancellation_tx: Option<mpsc::Sender<Frame>>,
    /// Control method owned by this route for cancellation delivery.
    cancellation_method: Option<String>,
    /// Cancellation delivery state. Pending-route ownership lets the shared
    /// drain bound its separate scheduling backlog.
    cancellation: CancellationDelivery,
    /// A terminal frame arrived while cancellation delivery was still pending.
    terminal_seen: bool,
}

#[derive(Default)]
enum CancellationDelivery {
    #[default]
    Idle,
    Preparing(CancellationRetention),
    Waiting(CancellationRetention),
    Sending(CancellationRetention),
    SentUntilTerminal,
}

/// Outcome of asking the outbound writer to cancel a pending route.
enum CancellationStart {
    Queued,
    QueuedInBackground { capacity: usize },
    Closed,
    NotRunning,
    AlreadyCancelling,
}

struct QueuedCancellation {
    id: FrameId,
    tx: mpsc::Sender<Frame>,
    frame: Frame,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum CancellationRetention {
    UntilQueued,
    UntilTerminal,
}

#[derive(Default)]
struct CancellationDrain {
    queued: VecDeque<FrameId>,
    overflowed: bool,
    active: bool,
    #[cfg(test)]
    high_watermark: usize,
}

/// Removes a just-registered call if opening its outbound stream is cancelled.
struct PendingRegistration {
    shared: Arc<Shared>,
    id: FrameId,
    cmd_tx: Option<mpsc::Sender<Frame>>,
    cancel_method: Option<&'static str>,
    armed: bool,
}

impl PendingRegistration {
    fn new(
        shared: Arc<Shared>,
        id: FrameId,
        cmd_tx: Option<mpsc::Sender<Frame>>,
        cancel_method: Option<&'static str>,
    ) -> Self {
        Self {
            shared,
            id,
            cmd_tx,
            cancel_method,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = cancel_pending(
            &self.shared,
            self.id,
            self.cmd_tx.clone(),
            self.cancel_method,
            None,
            CancellationRetention::UntilQueued,
        );
    }
}

struct RetainedBytes {
    counter: Arc<std::sync::atomic::AtomicUsize>,
    cost: usize,
}

impl Drop for RetainedBytes {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.cost, Ordering::Relaxed);
    }
}

struct QueuedFrame {
    frame: Frame,
    retained: Option<RetainedBytes>,
}

impl QueuedFrame {
    fn plain(frame: Frame) -> Self {
        Self {
            frame,
            retained: None,
        }
    }

    fn retained(frame: Frame, counter: Arc<std::sync::atomic::AtomicUsize>, cost: usize) -> Self {
        Self {
            frame,
            retained: Some(RetainedBytes { counter, cost }),
        }
    }

    fn into_frame(self) -> Frame {
        let Self { frame, retained } = self;
        drop(retained);
        frame
    }
}

#[derive(Clone)]
enum PendingStream {
    /// Tool progress is explicitly lossy: stale updates may be discarded.
    Lossy(mpsc::Sender<QueuedFrame>),
    /// Provider events enter a bounded per-call forwarding task. Saturation
    /// (frame count or retained wire bytes) fails and cancels only this
    /// call rather than blocking the shared reader.
    Lossless {
        ingress: mpsc::Sender<QueuedFrame>,
        cancel_tx: Option<mpsc::Sender<Frame>>,
        cancel_method: &'static str,
        /// Retained wire-byte budget shared with the ingress sender.
        bytes: Arc<std::sync::atomic::AtomicUsize>,
    },
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
    /// Saturated cancellations share one FIFO drain task instead of spawning
    /// one blocked task per pending route.
    cancellation_drain: StdMutex<CancellationDrain>,
    /// Notifies observers after the cancellation drain reaches an idle state.
    cancellation_drain_idle: Notify,
    /// slot key → latest accepted generation. Stale pushes are discarded.
    slot_generations: StdMutex<HashMap<String, u64>>,
    /// Unsolicited event fan-out.
    events: broadcast::Sender<HostEvent>,
    /// Monotonic request id allocator.
    next_id: AtomicU64,
    /// Retained stderr tail (most recent `STDERR_TAIL_BYTES` bytes).
    stderr: StdMutex<String>,
    /// Cleared once the reader or writer observes end-of-stream.
    running: AtomicBool,
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
    /// Extension fire-and-forget UI control (`ui.setStatus`, …).
    UiControl(crate::protocol::UiControl),
    /// Untyped / unrecognized frame.
    Raw(Frame),
    /// Host stdout closed.
    Eof,
    /// Host emitted a malformed frame.
    ProtocolError(String),
}

/// Handshake validation policy for a host connection.
///
/// The wire `hello` payload is identical under every policy; only the
/// acknowledgment validation differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HandshakePolicy {
    /// Validate protocol and compatibility versions (Mode 1 compiled host).
    /// This is the historical [`HostClient::handshake`] behavior.
    #[default]
    Compat,
    /// Validate only the protocol version (Mode 2 lean and Mode 3 native
    /// endpoints, which carry no upstream TypeScript compatibility surface).
    ProtocolOnly,
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
    /// A lossless stream's bounded ingress queue was exhausted (frame count
    /// or retained payload bytes).
    #[error(
        "host stream {id} exceeded its forwarding capacity ({capacity} frames / {bytes} bytes)"
    )]
    StreamOverflow {
        /// Frame id of the overflowing call.
        id: FrameId,
        /// Maximum queued provider events.
        capacity: usize,
        /// Maximum retained provider wire bytes.
        bytes: usize,
    },
    /// An explicit stream cancel could not be queued because outbound capacity was full.
    #[error(
        "host stream {id} cancel could not enqueue: outbound channel is saturated at {capacity} frames; cancellation is queued for retry"
    )]
    OutboundCancelFull {
        /// Frame id of the stream being cancelled.
        id: FrameId,
        /// Maximum queued outbound frames.
        capacity: usize,
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
            cancellation_drain: StdMutex::new(CancellationDrain::default()),
            cancellation_drain_idle: Notify::new(),
            slot_generations: StdMutex::new(HashMap::new()),
            events: events_tx,
            next_id: AtomicU64::new(1),
            stderr: StdMutex::new(String::new()),
            running: AtomicBool::new(true),
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
    pub(crate) fn pending_count(&self) -> usize {
        self.shared.pending.lock().map_or(0, |pending| {
            pending
                .values()
                .filter(|entry| {
                    matches!(
                        entry.cancellation,
                        CancellationDelivery::Idle | CancellationDelivery::SentUntilTerminal
                    )
                })
                .count()
        })
    }

    #[cfg(test)]
    fn cancellation_delivery_count(&self) -> usize {
        self.shared.pending.lock().map_or(0, |pending| {
            pending
                .values()
                .filter(|entry| !matches!(entry.cancellation, CancellationDelivery::Idle))
                .count()
        })
    }

    #[cfg(test)]
    async fn wait_for_cancellation_drain_idle(&self) {
        loop {
            let notified = self.shared.cancellation_drain_idle.notified();
            let idle = !self
                .shared
                .cancellation_drain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active;
            if idle {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn stall_outbound_for_test(
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
        self.insert_pending(
            id,
            PendingEntry {
                terminal: Some(tx),
                stream: None,
                cancellation_tx: None,
                cancellation_method: None,
                cancellation: CancellationDelivery::Idle,
                terminal_seen: false,
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
                        self.cmd_tx.lock().await.clone(),
                        Some(control_method),
                        None,
                        CancellationRetention::UntilQueued,
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
        let payload = serde_json::to_value(crate::protocol::SessionSetModelResponse { success })
            .map_err(|error| {
                HostClientError::Payload(format!("encode setModel response: {error}"))
            })?;
        self.send_frame(Frame {
            id,
            kind: FrameKind::Res,
            method: crate::protocol::SESSION_SET_MODEL_METHOD.to_owned(),
            payload,
        })
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
        let frame = match outcome {
            Ok(result) => Frame {
                id,
                kind: FrameKind::Res,
                method: crate::protocol::SESSION_COMPACT_METHOD.to_owned(),
                payload: serde_json::to_value(crate::protocol::SessionCompactResponse { result })
                    .map_err(|error| {
                    HostClientError::Payload(format!("encode compact response: {error}"))
                })?,
            },
            Err(message) => Frame {
                id,
                kind: FrameKind::Error,
                method: crate::protocol::SESSION_COMPACT_METHOD.to_owned(),
                payload: serde_json::to_value(crate::protocol::ErrorPayload::new(
                    "extension_error",
                    &message,
                ))
                .map_err(|error| {
                    HostClientError::Payload(format!("encode compact error: {error}"))
                })?,
            },
        };
        self.send_frame(frame).await
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
        let (stream_tx, stream_rx) = mpsc::channel::<QueuedFrame>(bound);
        let cmd_tx = self.cmd_tx.lock().await.clone();
        let stream = if method == "provider.stream" {
            let (forward_tx, mut forward_rx) =
                mpsc::channel::<QueuedFrame>(PROVIDER_FORWARD_CAPACITY);
            tokio::spawn(async move {
                while let Some(frame) = forward_rx.recv().await {
                    if stream_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            });
            let retained_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            PendingStream::Lossless {
                ingress: forward_tx,
                cancel_tx: cmd_tx.clone(),
                cancel_method: "provider.cancel",
                bytes: retained_bytes,
            }
        } else {
            PendingStream::Lossy(stream_tx)
        };
        self.insert_pending(
            id,
            PendingEntry {
                terminal: Some(terminal_tx),
                stream: Some(stream),
                cancellation_tx: None,
                cancellation_method: cancel_method_for(method).map(str::to_owned),
                cancellation: CancellationDelivery::Idle,
                terminal_seen: false,
            },
        );
        let mut registration = PendingRegistration::new(
            Arc::clone(&self.shared),
            id,
            cmd_tx.clone(),
            cancel_method_for(method),
        );
        let frame = Frame {
            id,
            kind: FrameKind::Req,
            method: method.to_owned(),
            payload,
        };
        self.send_frame(frame).await?;
        let handle = StreamHandle {
            id,
            events: stream_rx,
            terminal: Some(terminal_rx),
            shared: Arc::clone(&self.shared),
            cmd_tx,
            cancel_method: cancel_method_for(method),
            consumed: false,
        };
        registration.disarm();
        Ok(handle)
    }

    /// Perform the `hello` handshake and validate versions.
    ///
    /// Equivalent to [`HostClient::handshake_with_policy`] with
    /// [`HandshakePolicy::Compat`]: both the protocol and compatibility
    /// versions must match the compiled constants (Mode 1 hosts).
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Handshake`], [`HostClientError::Timeout`],
    /// or transport errors.
    pub async fn handshake(&self) -> HostResult<()> {
        self.handshake_with_policy(HandshakePolicy::Compat).await
    }

    /// Perform the `hello` handshake under an explicit validation policy.
    ///
    /// The request payload is identical for every policy; only the
    /// acknowledgment validation differs. [`HandshakePolicy::ProtocolOnly`]
    /// is used for Mode 2 lean and Mode 3 native endpoints, which carry no
    /// upstream TypeScript compatibility surface.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::Handshake`], [`HostClientError::Timeout`],
    /// or transport errors.
    pub async fn handshake_with_policy(&self, policy: HandshakePolicy) -> HostResult<()> {
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
        match policy {
            HandshakePolicy::Compat => {
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
            }
            HandshakePolicy::ProtocolOnly => {
                if ack.protocol_version != PROTOCOL_VERSION {
                    return Err(HostClientError::Handshake {
                        message: format!(
                            "protocol mismatch: remote protocol={} (expected {PROTOCOL_VERSION})",
                            ack.protocol_version,
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn insert_pending(&self, id: FrameId, entry: PendingEntry) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.insert(id, entry);
        }
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
        if let Some(child) = child_guard.take() {
            let reap = reap_child(child).await;
            drop(self.reader_handle.lock().await.take());
            reap
        } else {
            // In-memory transport: no child to reap. Pending calls remain the
            // reader's responsibility; `running` is already cleared.
            Ok(())
        }
    }
}

/// Handle for a streaming call.
pub struct StreamHandle {
    id: FrameId,
    events: mpsc::Receiver<QueuedFrame>,
    terminal: Option<oneshot::Receiver<FrameResult>>,
    shared: Arc<Shared>,
    cmd_tx: Option<mpsc::Sender<Frame>>,
    cancel_method: Option<&'static str>,
    consumed: bool,
}

impl StreamHandle {
    /// Correlation id for this streaming call.
    #[must_use]
    pub fn id(&self) -> FrameId {
        self.id
    }

    /// Receive the next intermediate event frame, if any.
    ///
    /// Returns `None` when the stream closed (terminal resolved or host gone).
    pub async fn next_event(&mut self) -> Option<Frame> {
        self.events.recv().await.map(QueuedFrame::into_frame)
    }

    /// Send a cancel control frame for this call to the host.
    ///
    /// # Errors
    ///
    /// Returns [`HostClientError::OutboundCancelFull`] when the outbound queue
    /// is full after scheduling one background retry, or
    /// [`HostClientError::Closed`] when the pipe is broken.
    pub fn cancel(&mut self, control_method: &str) -> HostResult<()> {
        match cancel_pending(
            &self.shared,
            self.id,
            self.cmd_tx.clone(),
            Some(control_method),
            None,
            CancellationRetention::UntilTerminal,
        ) {
            CancellationStart::Queued | CancellationStart::AlreadyCancelling => Ok(()),
            CancellationStart::QueuedInBackground { capacity } => {
                Err(HostClientError::OutboundCancelFull {
                    id: self.id,
                    capacity,
                })
            }
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
            Ok(Ok(result)) => {
                self.consumed = true;
                result
            }
            Ok(Err(_)) => {
                self.consumed = true;
                Err(HostClientError::Closed {
                    message: "stream terminal closed".to_owned(),
                    stderr: stderr_of(&self.shared),
                })
            }
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
            self.cmd_tx.clone(),
            self.cancel_method,
            None,
            CancellationRetention::UntilQueued,
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
            Ok(n) => match decoder.push_with_wire_bytes(&buf[..n]) {
                Ok(frames) => {
                    for frame in frames {
                        dispatch_decoded(&shared, frame);
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

fn remove_cancelling_pending(shared: &Shared, id: FrameId) {
    if let Ok(mut pending) = shared.pending.lock()
        && pending
            .get(&id)
            .is_some_and(|entry| !matches!(entry.cancellation, CancellationDelivery::Idle))
    {
        pending.remove(&id);
    }
}

fn finish_cancellation(shared: &Shared, id: FrameId, queued: bool) {
    let Ok(mut pending) = shared.pending.lock() else {
        return;
    };
    let Some(entry) = pending.get_mut(&id) else {
        return;
    };
    let (CancellationDelivery::Preparing(retention) | CancellationDelivery::Sending(retention)) =
        entry.cancellation
    else {
        return;
    };
    if !queued || retention == CancellationRetention::UntilQueued || entry.terminal_seen {
        pending.remove(&id);
    } else {
        entry.cancellation = CancellationDelivery::SentUntilTerminal;
    }
}

fn claim_cancellation(shared: &Shared, id: Option<FrameId>) -> Option<QueuedCancellation> {
    let mut pending = shared
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (id, entry) = match id {
        Some(id) => (id, pending.get_mut(&id)?),
        None => pending
            .iter_mut()
            .find(|(_, entry)| matches!(entry.cancellation, CancellationDelivery::Waiting(_)))
            .map(|(id, entry)| (*id, entry))?,
    };
    let CancellationDelivery::Waiting(retention) = entry.cancellation else {
        return None;
    };
    entry.cancellation = CancellationDelivery::Sending(retention);
    let tx = entry.cancellation_tx.take()?;
    let method = entry.cancellation_method.take()?;
    Some(QueuedCancellation {
        id,
        tx,
        frame: cancel_frame(id, &method),
    })
}

/// Marks one pending route as cancelling and queues exactly one control frame.
/// The scheduling queue is bounded; overflow ownership remains on the pending
/// route and is discovered by the single shared drain.
fn enqueue_cancellation(shared: &Arc<Shared>, id: FrameId) {
    let start_drain = {
        let mut drain = shared
            .cancellation_drain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if drain.queued.len() < CANCELLATION_BACKLOG_CAPACITY {
            drain.queued.push_back(id);
            #[cfg(test)]
            {
                drain.high_watermark = drain.high_watermark.max(drain.queued.len());
            }
        } else {
            drain.overflowed = true;
        }
        if drain.active {
            false
        } else {
            drain.active = true;
            true
        }
    };
    if !start_drain {
        return;
    }

    let runtime = shared.runtime.clone();
    let shared = Arc::clone(shared);
    runtime.spawn(async move {
        loop {
            let (id, scanning_overflow) = {
                let mut drain = shared
                    .cancellation_drain
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(id) = drain.queued.pop_front() {
                    (Some(id), false)
                } else if drain.overflowed {
                    drain.overflowed = false;
                    (None, true)
                } else {
                    drain.active = false;
                    shared.cancellation_drain_idle.notify_waiters();
                    return;
                }
            };
            let Some(cancellation) = claim_cancellation(&shared, id) else {
                continue;
            };
            if scanning_overflow {
                shared
                    .cancellation_drain
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .overflowed = true;
            }
            let queued = cancellation.tx.send(cancellation.frame).await.is_ok();
            finish_cancellation(&shared, cancellation.id, queued);
        }
    });
}

fn cancel_pending(
    shared: &Arc<Shared>,
    id: FrameId,
    cmd_tx: Option<mpsc::Sender<Frame>>,
    control_method: Option<&str>,
    terminal_error: Option<HostClientError>,
    retention: CancellationRetention,
) -> CancellationStart {
    let terminal = if let Ok(mut pending) = shared.pending.lock() {
        let Some(entry) = pending.get_mut(&id) else {
            return CancellationStart::AlreadyCancelling;
        };
        match entry.cancellation {
            CancellationDelivery::Idle => {
                entry.cancellation = CancellationDelivery::Preparing(retention);
            }
            CancellationDelivery::Preparing(current) => {
                entry.cancellation = CancellationDelivery::Preparing(current.min(retention));
                return CancellationStart::AlreadyCancelling;
            }
            CancellationDelivery::Waiting(current) => {
                entry.cancellation = CancellationDelivery::Waiting(current.min(retention));
                return CancellationStart::AlreadyCancelling;
            }
            CancellationDelivery::Sending(current) => {
                entry.cancellation = CancellationDelivery::Sending(current.min(retention));
                return CancellationStart::AlreadyCancelling;
            }
            CancellationDelivery::SentUntilTerminal => {
                if retention == CancellationRetention::UntilQueued {
                    pending.remove(&id);
                }
                return CancellationStart::AlreadyCancelling;
            }
        }
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
        remove_cancelling_pending(shared, id);
        return CancellationStart::NotRunning;
    };
    let cancel = cancel_frame(id, control_method);
    let capacity = tx.max_capacity();
    match tx.try_send(cancel) {
        Ok(()) => {
            finish_cancellation(shared, id, true);
            CancellationStart::Queued
        }
        Err(mpsc::error::TrySendError::Full(frame)) => {
            let stored = if let Ok(mut pending) = shared.pending.lock()
                && let Some(entry) = pending.get_mut(&id)
                && let CancellationDelivery::Preparing(retention) = entry.cancellation
            {
                if entry.cancellation_tx.is_none() {
                    entry.cancellation_tx = Some(tx);
                }
                if entry.cancellation_method.as_deref() != Some(frame.method.as_str()) {
                    entry.cancellation_method = Some(frame.method);
                }
                entry.cancellation = CancellationDelivery::Waiting(retention);
                true
            } else {
                false
            };
            if stored {
                enqueue_cancellation(shared, id);
            }
            CancellationStart::QueuedInBackground { capacity }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            remove_cancelling_pending(shared, id);
            CancellationStart::Closed
        }
    }
}

#[cfg(test)]
fn dispatch(shared: &Arc<Shared>, frame: Frame) {
    dispatch_with_wire_bytes(shared, frame, 0);
}

fn dispatch_decoded(shared: &Arc<Shared>, decoded: crate::protocol::DecodedFrame) {
    dispatch_with_wire_bytes(shared, decoded.frame, decoded.wire_bytes);
}

fn dispatch_with_wire_bytes(shared: &Arc<Shared>, frame: Frame, wire_bytes: usize) {
    if let Err(e) = frame.validate(false) {
        let _ = shared.events.send(HostEvent::ProtocolError(e.to_string()));
        return;
    }
    let id = frame.id;
    match frame.kind {
        FrameKind::Res => {
            if let Some(tx) = take_terminal_pending(shared, id) {
                let _ = tx.send(Ok(frame));
            }
        }
        FrameKind::Error => {
            if id == 0 {
                let _ = shared.events.send(HostEvent::Raw(frame));
            } else {
                let err = remote_error(&frame);
                if let Some(tx) = take_terminal_pending(shared, id) {
                    let _ = tx.send(Err(err));
                }
            }
        }
        FrameKind::Event => {
            if id == 0 {
                forward_event(shared, frame);
            } else {
                forward_stream_event(shared, frame, wire_bytes);
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
    } else {
        let _ = shared.events.send(HostEvent::Raw(frame));
    }
}

fn forward_stream_event(shared: &Arc<Shared>, frame: Frame, wire_bytes: usize) {
    let id = frame.id;
    let stream = if let Ok(pending) = shared.pending.lock() {
        pending
            .get(&id)
            .filter(|entry| matches!(entry.cancellation, CancellationDelivery::Idle))
            .and_then(|entry| entry.stream.clone())
    } else {
        None
    };
    if let Some(stream) = stream {
        match stream {
            PendingStream::Lossy(stream) => {
                let _ = stream.try_send(QueuedFrame::plain(frame));
            }
            PendingStream::Lossless {
                ingress,
                cancel_tx,
                cancel_method,
                bytes,
            } => {
                let cost = wire_bytes;
                let prev = bytes.fetch_add(cost, Ordering::Relaxed);
                let queued = QueuedFrame::retained(frame, Arc::clone(&bytes), cost);
                let send = if prev.saturating_add(cost) > PROVIDER_FORWARD_BYTES {
                    Err(mpsc::error::TrySendError::Full(queued))
                } else {
                    ingress.try_send(queued)
                };
                match send {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        let _ = cancel_pending(
                            shared,
                            id,
                            cancel_tx,
                            Some(cancel_method),
                            Some(HostClientError::StreamOverflow {
                                id,
                                capacity: PROVIDER_FORWARD_CAPACITY,
                                bytes: PROVIDER_FORWARD_BYTES,
                            }),
                            CancellationRetention::UntilQueued,
                        );
                    }
                }
            }
        }
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

fn take_terminal_pending(shared: &Shared, id: FrameId) -> Option<oneshot::Sender<FrameResult>> {
    let mut pending = shared.pending.lock().ok()?;
    let entry = pending.get_mut(&id)?;
    let terminal = entry.terminal.take();
    entry.stream = None;
    if matches!(
        entry.cancellation,
        CancellationDelivery::Idle | CancellationDelivery::SentUntilTerminal
    ) {
        pending.remove(&id);
    } else {
        entry.terminal_seen = true;
    }
    terminal
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

    fn provider_retained_bytes(
        client: &HostClient,
        id: FrameId,
    ) -> Result<Arc<std::sync::atomic::AtomicUsize>, Box<dyn Error>> {
        let pending = client
            .shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .get(&id)
            .and_then(|entry| match &entry.stream {
                Some(PendingStream::Lossless { bytes, .. }) => Some(Arc::clone(bytes)),
                _ => None,
            })
            .ok_or_else(|| format!("missing provider wire-byte budget for stream {id}").into())
    }

    async fn wait_for_retained_bytes(bytes: &std::sync::atomic::AtomicUsize, expected: usize) -> R {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if bytes.load(Ordering::Relaxed) == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        Ok(())
    }

    use crate::test_support::make_pair;

    #[tokio::test]
    async fn decoded_non_provider_dispatch_is_unchanged() -> R {
        let (client, mut host) = make_pair().await;
        let client = Arc::new(client);
        let request_client = Arc::clone(&client);
        let request = tokio::spawn(async move {
            request_client
                .request_raw(
                    "ordinary.call",
                    serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await
        });
        let outbound = host.read_frame().await.ok_or("no ordinary request")?;
        let response = Frame::response(
            outbound.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        );
        let wire_bytes = crate::protocol::encode_frame(&response)?.len();
        dispatch_decoded(
            &client.shared,
            crate::protocol::DecodedFrame {
                frame: response,
                wire_bytes,
            },
        );

        assert_eq!(request.await??.payload["done"], true);
        Ok(())
    }

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
    async fn compat_handshake_rejects_compatibility_mismatch() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move { client.handshake().await });
        let req = host.read_frame().await.ok_or("no hello")?;
        let bad = Frame::response(
            req.id,
            Method::Hello,
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "compatibilityVersion": "0.0.0-bogus",
            }),
        );
        host.write_frame(&bad).await?;
        let result = client_task.await?;
        assert!(
            matches!(result, Err(HostClientError::Handshake { .. })),
            "compat policy must reject a compatibility mismatch, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn protocol_only_handshake_accepts_compatibility_mismatch() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            client
                .handshake_with_policy(HandshakePolicy::ProtocolOnly)
                .await
        });
        let req = host.read_frame().await.ok_or("no hello")?;
        // Lean/native endpoints carry no upstream TypeScript compatibility
        // surface, so ProtocolOnly ignores the acknowledgment string.
        let ack = Frame::response(
            req.id,
            Method::Hello,
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "compatibilityVersion": "0.0.0-bogus",
            }),
        );
        host.write_frame(&ack).await?;
        assert!(client_task.await?.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn protocol_only_handshake_rejects_protocol_mismatch() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            client
                .handshake_with_policy(HandshakePolicy::ProtocolOnly)
                .await
        });
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
            "protocol-only policy must still reject a protocol mismatch, got {result:?}"
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
                received.push(frame.payload["n"].as_u64().ok_or_else(|| {
                    HostClientError::Payload(
                        "stream event must contain an integer index".to_owned(),
                    )
                })?);
            }
            let terminal = stream.finish(Duration::from_secs(2)).await?;
            Ok::<_, HostClientError>((received, terminal))
        });
        let req = host.read_frame().await.ok_or("no req")?;
        // A bound-two channel retains exactly the FIFO prefix and drops 18 updates.
        for n in 0..20u64 {
            let ev = Frame::event(req.id, Method::ToolUpdate, serde_json::json!({"n": n}));
            host.write_frame(&ev).await?;
        }
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({"done": true}));
        host.write_frame(&res).await?;
        let (received, terminal) = client_task.await??;
        assert_eq!(
            received,
            vec![0, 1],
            "only the bound-two FIFO prefix is retained"
        );
        assert_eq!(terminal.payload["done"], true);
        Ok(())
    }

    #[tokio::test]
    async fn provider_stream_backpressure_is_lossless_and_call_local() -> R {
        const EVENT_COUNT: u64 = 128;

        let (client, mut host) = make_pair().await;
        let client = Arc::new(client);
        let mut provider = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let provider_request = host.read_frame().await.ok_or("no provider request")?;

        let concurrent_client = Arc::clone(&client);
        let concurrent = tokio::spawn(async move {
            concurrent_client
                .request_raw(
                    "concurrent.call",
                    serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await
        });
        let concurrent_request = host.read_frame().await.ok_or("no concurrent request")?;

        for n in 0..EVENT_COUNT {
            host.write_frame(&Frame::event(
                provider_request.id,
                Method::ProviderEvent,
                serde_json::json!({"n": n}),
            ))
            .await?;
        }
        host.write_frame(&Frame::response(
            concurrent_request.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        ))
        .await?;

        let concurrent_terminal =
            tokio::time::timeout(Duration::from_millis(500), concurrent).await??;
        assert!(
            concurrent_terminal?.payload["done"]
                .as_bool()
                .unwrap_or(false)
        );

        host.write_frame(&Frame::response(
            provider_request.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        ))
        .await?;
        let mut seen = Vec::new();
        while let Some(frame) = provider.next_event().await {
            seen.push(frame.payload["n"].as_u64().ok_or("missing event index")?);
        }
        assert_eq!(seen, (0..EVENT_COUNT).collect::<Vec<_>>());
        assert!(
            provider.finish(Duration::from_secs(2)).await?.payload["done"]
                .as_bool()
                .unwrap_or(false)
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_explicitly_cancelled_stream_releases_directly_queued_correlation() -> R {
        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let mut stream = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let request = stalled.recv().await.ok_or("no stream request")?;

        stream.cancel("provider.cancel")?;
        drop(stream);

        let cancel = stalled.recv().await.ok_or("no cancellation")?;
        assert_eq!(cancel.payload["id"], request.id);
        assert!(matches!(
            stalled.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "drop must release terminal correlation after cancellation is queued"
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_cancel_keeps_terminal_correlation_while_stream_is_alive() -> R {
        let (client, mut host) = make_pair().await;
        let mut stream = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let request = host.read_frame().await.ok_or("no stream request")?;

        stream.cancel("provider.cancel")?;
        let cancel = host.read_frame().await.ok_or("no cancellation")?;
        assert_eq!(cancel.payload["id"], request.id);
        assert_eq!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "live handle must retain terminal correlation after explicit cancel"
        );

        host.write_frame(&Frame::response(
            request.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        ))
        .await?;
        assert!(
            stream.finish(Duration::from_secs(1)).await?.payload["done"]
                .as_bool()
                .unwrap_or(false)
        );
        assert!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_explicitly_cancelled_streams_waiting_behind_saturation_cleans_up() -> R {
        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let mut first = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let first_id = stalled.recv().await.ok_or("no first stream request")?.id;
        let mut second = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let second_id = stalled.recv().await.ok_or("no second stream request")?.id;
        let blocker = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;

        assert!(matches!(
            first.cancel("provider.cancel"),
            Err(HostClientError::OutboundCancelFull { .. })
        ));
        assert!(matches!(
            second.cancel("provider.cancel"),
            Err(HostClientError::OutboundCancelFull { .. })
        ));
        assert!(matches!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&second_id)
                .map(|entry| &entry.cancellation),
            Some(CancellationDelivery::Waiting(
                CancellationRetention::UntilTerminal
            ))
        ));
        drop(first);
        drop(second);

        let blocked_request = stalled.recv().await.ok_or("no blocker request")?;
        dispatch(
            &client.shared,
            Frame::response(
                blocked_request.id,
                Method::Notify,
                serde_json::json!({"done": true}),
            ),
        );
        blocker.finish(Duration::from_secs(1)).await?;

        let mut cancelled_ids = vec![
            stalled.recv().await.ok_or("no first cancellation")?.payload["id"]
                .as_u64()
                .ok_or("invalid first cancellation id")?,
            stalled
                .recv()
                .await
                .ok_or("no second cancellation")?
                .payload["id"]
                .as_u64()
                .ok_or("invalid second cancellation id")?,
        ];
        cancelled_ids.sort_unstable();
        let mut expected = vec![first_id, second_id];
        expected.sort_unstable();
        assert_eq!(cancelled_ids, expected);
        client.wait_for_cancellation_drain_idle().await;
        assert!(matches!(
            stalled.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "dropped waiting cancellations must release all raw pending entries"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_explicitly_cancelled_stream_while_sending_cleans_up() -> R {
        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let mut stream = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        let stream_id = stalled.recv().await.ok_or("no stream request")?.id;
        let blocker = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;

        assert!(matches!(
            stream.cancel("provider.cancel"),
            Err(HostClientError::OutboundCancelFull { .. })
        ));
        tokio::task::yield_now().await;
        assert!(matches!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&stream_id)
                .map(|entry| &entry.cancellation),
            Some(CancellationDelivery::Sending(
                CancellationRetention::UntilTerminal
            ))
        ));
        drop(stream);

        let blocked_request = stalled.recv().await.ok_or("no blocker request")?;
        dispatch(
            &client.shared,
            Frame::response(
                blocked_request.id,
                Method::Notify,
                serde_json::json!({"done": true}),
            ),
        );
        blocker.finish(Duration::from_secs(1)).await?;

        let cancel = stalled.recv().await.ok_or("no cancellation")?;
        assert_eq!(cancel.payload["id"], stream_id);
        client.wait_for_cancellation_drain_idle().await;
        assert!(matches!(
            stalled.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(
            client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "dropped sending cancellation must release all raw pending entries"
        );
        Ok(())
    }

    async fn open_drained_streams(
        client: &HostClient,
        outbound: &mut mpsc::Receiver<Frame>,
        count: usize,
    ) -> Result<(Vec<StreamHandle>, Vec<FrameId>), Box<dyn Error>> {
        let mut streams = Vec::with_capacity(count + 1);
        let mut ids = Vec::with_capacity(count + 1);
        for _ in 0..count {
            let stream = client
                .open_stream_raw("provider.stream", serde_json::json!({}), 2)
                .await?;
            ids.push(stream.id());
            streams.push(stream);
            outbound
                .recv()
                .await
                .ok_or("queued stream request must remain readable")?;
        }
        Ok((streams, ids))
    }

    async fn receive_cancellation_ids(
        outbound: &mut mpsc::Receiver<Frame>,
        count: usize,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            let cancel = outbound
                .recv()
                .await
                .ok_or("shared drain must eventually queue every cancellation")?;
            assert_eq!(cancel.method, "provider.cancel");
            ids.push(
                cancel.payload["id"]
                    .as_u64()
                    .ok_or("cancel frame must contain an integer correlation id")?,
            );
        }
        Ok(ids)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_outbound_cancellations_share_one_drain_and_cleanup_exactly_once() -> R {
        const STREAMS: usize = CANCELLATION_BACKLOG_CAPACITY * 3;

        let (client, _host) = make_pair().await;
        let (mut stalled, _original) = client.stall_outbound_for_test().await;
        let (mut streams, mut ids) = open_drained_streams(&client, &mut stalled, STREAMS).await?;

        let blocker = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 2)
            .await?;
        ids.push(blocker.id());
        streams.push(blocker);

        let mut explicit = streams.pop().ok_or("explicit cancellation stream")?;
        let explicit_id = explicit.id();
        assert!(matches!(
            explicit.cancel("provider.cancel"),
            Err(HostClientError::OutboundCancelFull { .. })
        ));
        dispatch(
            &client.shared,
            Frame::response(
                explicit_id,
                Method::Notify,
                serde_json::json!({"done": true}),
            ),
        );
        drop(explicit);
        for stream in streams {
            drop(stream);
        }

        {
            let drain = client
                .shared
                .cancellation_drain
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                drain.active,
                "saturated cancellations must have one drain owner"
            );
            assert_eq!(drain.queued.len(), CANCELLATION_BACKLOG_CAPACITY);
            assert!(
                drain.overflowed,
                "overflow must be coalesced into drain state"
            );
            assert_eq!(drain.high_watermark, CANCELLATION_BACKLOG_CAPACITY);
        }
        assert_eq!(
            client.cancellation_delivery_count(),
            STREAMS + 1,
            "pending routes own cancellation delivery beyond the bounded backlog"
        );

        let blocked_request = stalled
            .recv()
            .await
            .ok_or("the request saturating the outbound queue must remain readable")?;
        assert_eq!(blocked_request.id, *ids.last().ok_or("blocker id")?);

        let mut cancelled_ids = receive_cancellation_ids(&mut stalled, STREAMS + 1).await?;
        cancelled_ids.sort_unstable();
        ids.sort_unstable();
        assert_eq!(
            cancelled_ids, ids,
            "each dropped stream must queue exactly one cancellation"
        );

        client.wait_for_cancellation_drain_idle().await;
        let drain = client
            .shared
            .cancellation_drain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!drain.active);
        assert!(drain.queued.is_empty());
        assert!(drain.high_watermark <= CANCELLATION_BACKLOG_CAPACITY);
        drop(drain);
        assert_eq!(
            client.pending_count(),
            0,
            "logical active pending count must be zero"
        );
        {
            let pending = client
                .shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(pending.is_empty(), "raw pending map must not leak routes");
        }
        assert_eq!(
            client.cancellation_delivery_count(),
            0,
            "no cancellation delivery state must remain on an idle drain"
        );
        assert!(
            matches!(stalled.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "an idle drain cannot later queue a duplicate cancellation"
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
    async fn provider_stream_overflow_is_explicit_cancelled_and_call_local() -> R {
        // Flood past the production bound so a capacity change cannot desync the
        // regression (too-small flood would stop overflowing; stale expected
        // capacity would fail the assertion below).
        const FLOOD: usize = PROVIDER_FORWARD_CAPACITY + 3;

        let (client, mut host) = make_pair().await;
        let client = Arc::new(client);
        let provider = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 1)
            .await?;
        let provider_request = host.read_frame().await.ok_or("no provider request")?;

        for n in 0..FLOOD {
            host.write_frame(&Frame::event(
                provider_request.id,
                Method::ProviderEvent,
                serde_json::json!({"n": n}),
            ))
            .await?;
        }

        let concurrent_client = Arc::clone(&client);
        let concurrent = tokio::spawn(async move {
            concurrent_client
                .request_raw(
                    "concurrent.call",
                    serde_json::json!({}),
                    Duration::from_secs(2),
                )
                .await
        });

        let mut cancel = None;
        let mut concurrent_request = None;
        while cancel.is_none() || concurrent_request.is_none() {
            let frame = tokio::time::timeout(Duration::from_secs(2), host.read_frame())
                .await?
                .ok_or("host pipe closed before overflow handling")?;
            if frame.method == "provider.cancel" {
                cancel = Some(frame);
            } else if frame.method == "concurrent.call" {
                concurrent_request = Some(frame);
            }
        }
        let cancel = cancel.ok_or("missing provider cancel")?;
        assert_eq!(cancel.payload["id"], provider_request.id);

        let concurrent_request = concurrent_request.ok_or("missing concurrent request")?;
        host.write_frame(&Frame::response(
            concurrent_request.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        ))
        .await?;
        let concurrent_terminal =
            tokio::time::timeout(Duration::from_millis(500), concurrent).await???;
        assert_eq!(concurrent_terminal.payload["done"], true);

        assert!(matches!(
            provider.finish(Duration::from_secs(2)).await,
            Err(HostClientError::StreamOverflow {
                id,
                capacity: PROVIDER_FORWARD_CAPACITY,
                ..
            }) if id == provider_request.id
        ));
        Ok(())
    }
    #[tokio::test]
    async fn provider_stream_wire_bytes_are_bounded_before_frame_capacity() -> R {
        let (client, mut host) = make_pair().await;
        let provider = client
            .open_stream_raw(
                "provider.stream",
                serde_json::json!({}),
                STREAM_EVENT_CAPACITY * 8,
            )
            .await?;
        let request = host.read_frame().await.ok_or("no provider request")?;
        let chunk = "x".repeat(PROVIDER_FORWARD_BYTES / 8 - 512);

        for n in 0..9 {
            let frame = Frame::event(
                request.id,
                Method::ProviderEvent,
                serde_json::json!({"n": n, "chunk": chunk.clone()}),
            );
            let wire_bytes = crate::protocol::encode_frame(&frame)?.len();
            dispatch_decoded(
                &client.shared,
                crate::protocol::DecodedFrame { frame, wire_bytes },
            );
            tokio::task::yield_now().await;
        }

        let cancel = tokio::time::timeout(Duration::from_secs(2), host.read_frame())
            .await?
            .ok_or("missing provider cancel")?;
        assert_eq!(cancel.method, "provider.cancel");
        assert_eq!(cancel.payload["id"], request.id);
        assert!(matches!(
            provider.finish(Duration::from_secs(2)).await,
            Err(HostClientError::StreamOverflow {
                id,
                capacity: PROVIDER_FORWARD_CAPACITY,
                bytes: PROVIDER_FORWARD_BYTES,
            }) if id == request.id
        ));
        Ok(())
    }
    #[tokio::test]
    async fn provider_stream_wire_budget_is_released_when_consumed() -> R {
        let (client, mut host) = make_pair().await;
        let mut provider = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 1)
            .await?;
        let request = host.read_frame().await.ok_or("no provider request")?;
        let bytes = provider_retained_bytes(&client, request.id)?;
        let chunk = "x".repeat(PROVIDER_FORWARD_BYTES / 8 - 512);

        for n in 0..8 {
            let frame = Frame::event(
                request.id,
                Method::ProviderEvent,
                serde_json::json!({"n": n, "chunk": chunk.clone()}),
            );
            let wire_bytes = crate::protocol::encode_frame(&frame)?.len();
            dispatch_decoded(
                &client.shared,
                crate::protocol::DecodedFrame { frame, wire_bytes },
            );
            wait_for_retained_bytes(&bytes, wire_bytes).await?;
            let event = tokio::time::timeout(Duration::from_secs(2), provider.next_event())
                .await?
                .ok_or("provider event queue closed")?;
            assert_eq!(event.payload["n"], n);
            wait_for_retained_bytes(&bytes, 0).await?;
        }

        host.write_frame(&Frame::response(
            request.id,
            Method::Notify,
            serde_json::json!({"done": true}),
        ))
        .await?;
        assert_eq!(
            provider.finish(Duration::from_secs(2)).await?.payload["done"],
            true
        );
        Ok(())
    }
    #[tokio::test]
    async fn provider_stream_wire_budget_is_released_when_dropped() -> R {
        let (client, mut host) = make_pair().await;
        let provider = client
            .open_stream_raw("provider.stream", serde_json::json!({}), 1)
            .await?;
        let request = host.read_frame().await.ok_or("no provider request")?;
        let bytes = provider_retained_bytes(&client, request.id)?;
        let frame = Frame::event(
            request.id,
            Method::ProviderEvent,
            serde_json::json!({"chunk": "retained"}),
        );
        let wire_bytes = crate::protocol::encode_frame(&frame)?.len();
        dispatch_decoded(
            &client.shared,
            crate::protocol::DecodedFrame { frame, wire_bytes },
        );
        wait_for_retained_bytes(&bytes, wire_bytes).await?;

        drop(provider);
        wait_for_retained_bytes(&bytes, 0).await?;
        Ok(())
    }
}
