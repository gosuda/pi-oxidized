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
//! - **Cancel / timeout.** Every call has a deadline; cancellation removes the
//!   pending entry and (optionally) sends a control frame.
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
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::host::{HostError, HostSpec};
use crate::protocol::{
    COMPATIBILITY_VERSION, Frame, FrameDecoder, FrameId, FrameKind, Hello, HelloAck,
    MeasureResponse, Method, PROTOCOL_VERSION, encode_frame, from_payload,
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
}

/// Cross-task shared state.
struct Shared {
    /// id → pending call. `std::sync::Mutex` because critical sections never await.
    pending: StdMutex<HashMap<FrameId, PendingEntry>>,
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
                Self::remove_pending(&self.shared, id);
                Err(HostClientError::Timeout { id, timeout })
            }
        }
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
        self.insert_pending(
            id,
            PendingEntry {
                terminal: Some(terminal_tx),
                stream: Some(stream_tx),
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
            events: stream_rx,
            terminal: Some(terminal_rx),
            shared: Arc::clone(&self.shared),
            cmd_tx: self.cmd_tx.lock().await.clone(),
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
    events: mpsc::Receiver<Frame>,
    terminal: Option<oneshot::Receiver<FrameResult>>,
    shared: Arc<Shared>,
    cmd_tx: Option<mpsc::Sender<Frame>>,
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
    pub async fn cancel(&mut self, control_method: &str) -> HostResult<()> {
        let payload = serde_json::json!({ "id": self.id });
        let frame = Frame {
            id: 0,
            kind: FrameKind::Event,
            method: control_method.to_owned(),
            payload,
        };
        match &self.cmd_tx {
            Some(tx) => tx.send(frame).await.map_err(|e| HostClientError::Closed {
                message: format!("cancel send failed: {e}"),
                stderr: stderr_of(&self.shared),
            }),
            None => Err(HostClientError::NotRunning),
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
        let result = match tokio::time::timeout(timeout, terminal).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(HostClientError::Closed {
                message: "stream terminal closed".to_owned(),
                stderr: stderr_of(&self.shared),
            }),
            Err(_) => {
                Self::remove_pending(&self.shared, self.id);
                Err(HostClientError::Timeout {
                    id: self.id,
                    timeout,
                })
            }
        };
        self.consumed = true;
        result
    }

    fn remove_pending(shared: &Shared, id: FrameId) {
        if let Ok(mut pending) = shared.pending.lock() {
            pending.remove(&id);
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if !self.consumed {
            Self::remove_pending(&self.shared, self.id);
        }
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
                let _ = shared.events.send(HostEvent::Eof);
                shared.running.store(false, Ordering::Relaxed);
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
                    let _ = shared.events.send(HostEvent::ProtocolError(e.to_string()));
                    shared.running.store(false, Ordering::Relaxed);
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
                shared.running.store(false, Ordering::Relaxed);
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

fn dispatch(shared: &Shared, frame: Frame) {
    if let Err(e) = frame.validate(false) {
        let _ = shared.events.send(HostEvent::ProtocolError(e.to_string()));
        return;
    }
    let id = frame.id;
    match frame.kind {
        FrameKind::Res => {
            if let Some(entry) = take_pending(shared, id)
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
                if let Some(entry) = take_pending(shared, id)
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
            let _ = shared.events.send(HostEvent::Raw(frame));
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
            Ok(d) => {
                clear_generation(shared, &d.key);
                let _ = shared.events.send(HostEvent::DisposeSlot(d));
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
    } else {
        let _ = shared.events.send(HostEvent::Raw(frame));
    }
}

fn forward_stream_event(shared: &Shared, frame: Frame) {
    let id = frame.id;
    let stream = if let Ok(pending) = shared.pending.lock() {
        pending.get(&id).and_then(|entry| entry.stream.clone())
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

fn clear_generation(shared: &Shared, key: &str) {
    if let Ok(mut generations) = shared.slot_generations.lock() {
        generations.remove(key);
    }
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
    async fn stream_backpressure_drops_excess_no_deadlock() -> R {
        let (client, mut host) = make_pair().await;
        let client_task = tokio::spawn(async move {
            let mut stream = client
                .open_stream_raw("tool.execute", serde_json::json!({}), 2)
                .await?;
            // Do not drain events immediately: a flood must not deadlock the host.
            tokio::time::sleep(Duration::from_millis(60)).await;
            let mut got = 0u32;
            while stream.next_event().await.is_some() {
                got = got.saturating_add(1);
            }
            let terminal = stream.finish(Duration::from_secs(2)).await?;
            Ok::<_, HostClientError>((got, terminal))
        });
        let req = host.read_frame().await.ok_or("no req")?;
        // Flood 20 events into a bound-2 channel (excess dropped via try_send).
        for n in 0..20u64 {
            let ev = Frame::event(req.id, Method::ToolUpdate, serde_json::json!({"n":n}));
            let _ = host.write_frame(&ev).await;
        }
        let res = Frame::response(req.id, Method::Notify, serde_json::json!({}));
        host.write_frame(&res).await?;
        let (got, _terminal) = client_task.await??;
        // No deadlock: some events were observed, terminal resolved.
        assert!(got > 0, "should have observed some events");
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
}
