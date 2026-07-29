//! Mode 3 native extension server.
//!
//! A native endpoint is a separate executable that speaks the same JSONL
//! frame protocol as the TypeScript extension host, with a protocol-only
//! handshake: the received `protocolVersion` is validated against the
//! compiled [`PROTOCOL_VERSION`] and `compatibilityVersion` is ignored
//! (there is no TypeScript compatibility surface to match).
//!
//! The server answers `hello`, `extensions.load`, `tool.prepare`,
//! `tool.validate`, `tool.execute` (streaming `toolUpdate` events),
//! `command.execute`, `provider.stream` (streaming correlated
//! `providerEvent` frames), `flags.set`, `shortcut.execute`,
//! `terminalInput`, `tool.renderHtml`, and advertised lifecycle hooks, and
//! honors `tool.cancel` / `provider.cancel` control events. The registry
//! snapshot is captured once at startup; the lifecycle allowlist is derived
//! from its `handlers` (requests to `message_update_delta` map onto the
//! advertised `message_update` key). Request handling is concurrent: the
//! read loop keeps consuming frames while tool executions and provider
//! streams run, so cancel frames are observed mid-execution. In-flight
//! work and per-call update/event channels are bounded; unknown or
//! unadvertised methods fail closed with a correlated error frame without
//! affecting other requests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use pi_ai::types::AssistantMessageEvent;

use crate::adapters::methods;
use crate::protocol::{
    ErrorPayload, FLAGS_SET_METHOD, FlagValueWire, FlagsSetRequest, Frame, FrameDecoder, FrameId,
    FrameKind, Hello, HelloAck, Method, PROTOCOL_VERSION, RegistrySnapshot,
    SHORTCUT_EXECUTE_METHOD, TerminalInputResult, ToolUpdate, encode_frame, from_payload,
    to_payload,
};

/// Wire method for the registry snapshot request.
pub const EXTENSIONS_LOAD_METHOD: &str = "extensions.load";
/// Wire method for slash-command execution.
pub const COMMAND_EXECUTE_METHOD: &str = "command.execute";
/// Wire method for tool HTML rendering (`renderCall` / `renderResult`).
pub const TOOL_RENDER_HTML_METHOD: &str = "tool.renderHtml";
/// Wire method carrying compact assistant-message deltas; maps onto the
/// advertised `message_update` lifecycle handler key.
pub const MESSAGE_UPDATE_DELTA_METHOD: &str = "message_update_delta";
/// Advertised lifecycle handler key for [`MESSAGE_UPDATE_DELTA_METHOD`].
const MESSAGE_UPDATE_HANDLER: &str = "message_update";
/// Dedicated keypress-to-paint budget for native terminal-input callbacks.
///
/// This latency-sensitive path intentionally does not inherit a general hook
/// timeout. Dropping the callback at the deadline stops its work and releases
/// the request's server permit.
pub const NATIVE_TERMINAL_INPUT_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);
/// Server-side deadline for native callbacks without explicit cancellation.
///
/// A callback whose future never resolves would retain its semaphore permit
/// forever, eventually exhausting `max_in_flight`. Dropping the future at
/// the deadline releases the permit and answers a correlated `timeout` error.
pub const NATIVE_LIFECYCLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Default bound on concurrently handled requests.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 64;
/// Default bound on queued per-call streaming updates.
pub const DEFAULT_UPDATE_CAPACITY: usize = 64;
/// Default bound on the outbound (server → client) frame channel.
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 128;

/// Boxed extension future returned by [`NativeExtension`] methods.
pub type NativeFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Server configuration bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    /// Maximum number of requests handled concurrently. Overload errors use a
    /// bounded deferred queue so the read loop never stalls and cancel frames
    /// stay observable. If both outbound queues fill, the endpoint terminates
    /// instead of dropping a correlated rejection or growing memory unbounded.
    pub max_in_flight: usize,
    /// Maximum queued streaming updates per `tool.execute` call. When full,
    /// the stale update is dropped (matching the client's stream
    /// backpressure policy).
    pub update_capacity: usize,
    /// Bound on the shared outbound frame channel.
    pub outbound_capacity: usize,
    /// Server-side deadline for native callbacks without explicit cancellation.
    /// A callback that never resolves is dropped so its permit is released.
    pub lifecycle_deadline: std::time::Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            update_capacity: DEFAULT_UPDATE_CAPACITY,
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
            lifecycle_deadline: NATIVE_LIFECYCLE_DEADLINE,
        }
    }
}

/// Fatal server error (transport, protocol, handshake, or bounded overload).
#[derive(Debug, Error)]
pub enum ServerError {
    /// The first frame was not an acceptable `hello`.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// A malformed inbound frame was received.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The bounded deferred-rejection queue was exhausted.
    #[error("outbound overload rejection queue saturated")]
    OutboundOverflow,
    /// Transport failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Extension-reported failure, mapped onto a correlated error frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFault {
    /// Stable machine-readable error code (e.g. `not_found`).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl ExtensionFault {
    /// Build a fault with an explicit code.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// `not_found` fault for an unknown tool/command name.
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    /// `extension_error` fault for a generic extension failure.
    #[must_use]
    pub fn extension_error(message: impl Into<String>) -> Self {
        Self::new("extension_error", message)
    }
}

impl fmt::Display for ExtensionFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ExtensionFault {}

// ---------------------------------------------------------------------------
// Tool execution surface
// ---------------------------------------------------------------------------

/// Immutable session context published by a valid `extensions.load`.
///
/// The server passes an owned [`Arc`] snapshot to every asynchronous
/// [`NativeExtension`] callback. A later successful load replaces the
/// runtime snapshot without changing contexts already held by in-flight
/// callbacks. `project_trusted` is true only when the load payload contains
/// the JSON boolean `projectTrusted: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeExtensionContext {
    /// Session working directory supplied by `extensions.load`.
    pub cwd: String,
    /// Whether the host marked this project trusted for the session.
    pub project_trusted: bool,
}

/// A single `tool.execute` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Tool name.
    pub name: String,
    /// Correlating tool call id.
    pub tool_call_id: String,
    /// Arguments (prepared when `prepared` is set).
    pub args: Value,
    /// Whether `args` already went through prepare/validate.
    pub prepared: bool,
}

/// Bounded, non-blocking sink for streaming partial results.
///
/// When the per-call update channel is full, the stale update is dropped and
/// `send` returns `false` — matching the client's stream backpressure
/// policy. Not `Clone`: the server's `done_tx` signal — not sink drop —
/// bounds the forwarder's lifetime. Updates queued before `execute_tool`
/// returns are flushed before the terminal response; post-return sends
/// from a detached sink are ignored.
pub struct ToolUpdateSink {
    tx: mpsc::Sender<Value>,
}

impl ToolUpdateSink {
    /// Queue a partial result for streaming as a `toolUpdate` event.
    ///
    /// Returns `false` when the bounded channel is full (update dropped).
    #[must_use]
    pub fn send(&self, partial_result: Value) -> bool {
        self.tx.try_send(partial_result).is_ok()
    }
}

/// A single `provider.stream` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderStreamCall {
    /// Provider registration id (`providerId`; `name` accepted as fallback).
    pub provider_id: String,
    /// Model descriptor (open JSON).
    pub model: Value,
    /// Conversation context (open JSON).
    pub context: Value,
    /// Prepared stream options (open JSON).
    pub options: Value,
}

/// One item in the shared outbound queue. Event sinks retain their validated
/// wire bytes; other responses stay structured for fallback encoding.
#[derive(Debug)]
enum OutboundFrame {
    Structured(Frame),
    Encoded(Vec<u8>),
}

impl From<Frame> for OutboundFrame {
    fn from(frame: Frame) -> Self {
        Self::Structured(frame)
    }
}

/// Bounded, backpressured sink for `providerEvent` stream events.
///
/// Unlike [`ToolUpdateSink`], `send` awaits channel capacity instead of
/// dropping events: a provider stream must never silently lose an event
/// and still report success. Each event is pre-validated against the frame
/// encoder; an oversize or unencodable event poisons the call, `send`
/// returns `false`, and the server terminates with a correlated
/// `invalid_payload` error (never a success after loss). `send` is
/// cancellation-aware: a `provider.cancel` observed while the bounded
/// queue is full unblocks the wait so the execution winds down and
/// releases its in-flight slot. Not `Clone`: the server's `done_tx`
/// signal — not sink drop — bounds the forwarder's lifetime. Events
/// queued before `stream_provider` returns are flushed before the
/// terminal frame; post-return sends from a detached sink are ignored.
pub struct ProviderEventSink {
    id: FrameId,
    tx: mpsc::Sender<OutboundFrame>,
    cancel: CancellationToken,
    invalid: Arc<AtomicBool>,
}

impl ProviderEventSink {
    /// Queue one assistant-stream event for delivery as a correlated
    /// `providerEvent` frame, awaiting bounded capacity.
    ///
    /// Events are typed [`AssistantMessageEvent`]s, so semantically invalid
    /// events are unrepresentable and cannot be silently dropped by the
    /// client's decoder. A serialization or encode failure (oversize)
    /// poisons the call: `send` returns `false` and the terminal becomes a
    /// correlated `invalid_payload` error. Also returns `false` when the
    /// call was cancelled or the outbound channel closed; the producer
    /// must stop sending and wind down.
    pub async fn send(&self, event: AssistantMessageEvent) -> bool {
        if self.cancel.is_cancelled() {
            return false;
        }
        let Ok(payload) = serde_json::to_value(&event) else {
            self.invalid.store(true, Ordering::SeqCst);
            return false;
        };
        let frame = Frame {
            id: self.id,
            kind: FrameKind::Event,
            method: Method::ProviderEvent.as_str().to_owned(),
            payload,
        };
        // Encode once so validation and wire output share the same bytes.
        let Ok(encoded) = encode_frame(&frame) else {
            self.invalid.store(true, Ordering::SeqCst);
            return false;
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => false,
            result = self.tx.send(OutboundFrame::Encoded(encoded)) => result.is_ok(),
        }
    }
}

/// Bounded sink for unsolicited (id `0`) extension event frames.
///
/// Passed to lifecycle callbacks so native hooks can emit `uiSlot`,
/// `notify`, and other fire-and-forget events. Sends await capacity on the
/// shared outbound channel — there is no separate unbounded queue. Each
/// event is pre-validated against the frame encoder, so an oversize or
/// invalid event fails the `send` itself instead of being silently dropped
/// by the writer after a successful queue.
#[derive(Clone)]
pub struct NativeEventSink {
    tx: mpsc::Sender<OutboundFrame>,
}

impl NativeEventSink {
    /// Emit one unsolicited extension event frame (id `0`), awaiting
    /// bounded capacity on the shared outbound channel.
    ///
    /// Returns `false` when the event fails encode pre-validation or the
    /// outbound channel is closed (writer gone).
    pub async fn send(&self, method: &str, payload: Value) -> bool {
        let frame = Frame {
            id: 0,
            kind: FrameKind::Event,
            method: method.to_owned(),
            payload,
        };
        let Ok(encoded) = encode_frame(&frame) else {
            return false;
        };
        self.tx.send(OutboundFrame::Encoded(encoded)).await.is_ok()
    }
}

/// Unstable Mode 3 native extension contract.
///
/// This trait is pre-1.0: methods may change between releases. Implementors
/// are compiled into a native endpoint executable served by [`serve`].
///
/// Every asynchronous callback receives an owned [`Arc`] of the immutable
/// [`NativeExtensionContext`] captured for that request. `snapshot()` remains
/// startup-only and deliberately has no session context.
pub trait NativeExtension: Send + Sync + 'static {
    /// Registry snapshot mirror returned for `extensions.load`.
    fn snapshot(&self) -> RegistrySnapshot;

    /// Prepare raw tool arguments (`tool.prepare`).
    fn prepare_tool(
        &self,
        context: Arc<NativeExtensionContext>,
        name: String,
        args: Value,
    ) -> NativeFuture<Result<Value, ExtensionFault>>;

    /// Validate prepared tool arguments (`tool.validate`).
    fn validate_tool(
        &self,
        context: Arc<NativeExtensionContext>,
        name: String,
        args: Value,
        tool_call_id: Option<String>,
    ) -> NativeFuture<Result<Value, ExtensionFault>>;

    /// Execute a tool (`tool.execute`).
    ///
    /// Partial results are streamed through `updates`; `cancel` is triggered
    /// by a `tool.cancel` control event. Cancellation is cooperative (the
    /// future is never aborted mid-poll), matching the TypeScript host's
    /// `AbortSignal` semantics. When the token is cancelled, the server
    /// answers with a `cancelled` error frame regardless of the returned
    /// value.
    fn execute_tool(
        &self,
        context: Arc<NativeExtensionContext>,
        call: ToolCall,
        updates: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> NativeFuture<Result<Value, ExtensionFault>>;

    /// Run a slash command (`command.execute`).
    ///
    /// The default answers `not_found`, matching an endpoint with no
    /// registered commands.
    fn execute_command(
        &self,
        _context: Arc<NativeExtensionContext>,
        command: String,
        args: String,
    ) -> NativeFuture<Result<(), ExtensionFault>> {
        let _ = args;
        Box::pin(async move {
            Err(ExtensionFault::not_found(format!(
                "command not found: {command}"
            )))
        })
    }

    /// Stream one custom-provider call (`provider.stream`).
    ///
    /// Assistant-stream events go through `events` (bounded and
    /// cancellation-aware); `cancel` is triggered by a `provider.cancel`
    /// control event. The server always answers with a terminal frame:
    /// `invalid_payload` when an event failed encode pre-validation, else
    /// `cancelled` when the token fired, else the returned value/fault.
    /// The default answers `not_found`, matching an endpoint with no
    /// streaming providers.
    fn stream_provider(
        &self,
        _context: Arc<NativeExtensionContext>,
        call: ProviderStreamCall,
        events: ProviderEventSink,
        cancel: CancellationToken,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        let _ = (events, cancel);
        Box::pin(async move {
            Err(ExtensionFault::not_found(format!(
                "provider not found: {}",
                call.provider_id
            )))
        })
    }

    /// Apply validated CLI-flag values (`flags.set`).
    ///
    /// The response carries the returned boolean verbatim. The default
    /// returns `Ok(false)` — the honest answer for an endpoint that did
    /// not store anything.
    fn set_flags(
        &self,
        _context: Arc<NativeExtensionContext>,
        values: BTreeMap<String, FlagValueWire>,
    ) -> NativeFuture<Result<bool, ExtensionFault>> {
        let _ = values;
        Box::pin(async { Ok(false) })
    }

    /// Execute one registered keyboard shortcut (`shortcut.execute`).
    ///
    /// Returns whether the key was owned and dispatched. The default
    /// returns `Ok(false)` (`{"handled": false}`), matching the
    /// TypeScript hosts' no-owner reply.
    fn execute_shortcut(
        &self,
        _context: Arc<NativeExtensionContext>,
        key: String,
    ) -> NativeFuture<Result<bool, ExtensionFault>> {
        let _ = key;
        Box::pin(async { Ok(false) })
    }

    /// Rewrite or consume terminal input (`terminalInput`).
    ///
    /// The default returns an empty result (`{}`): no rewrite, no consume.
    fn handle_terminal_input(
        &self,
        _context: Arc<NativeExtensionContext>,
        data: String,
    ) -> NativeFuture<Result<TerminalInputResult, ExtensionFault>> {
        let _ = data;
        Box::pin(async { Ok(TerminalInputResult::default()) })
    }

    /// Render tool HTML for one phase (`tool.renderHtml`).
    ///
    /// `phase` is `call` or `result`; `payload` is the open render body.
    /// Return an object such as `{"html": "..."}`; the default returns
    /// `{}` (no renderer), mirroring both TypeScript hosts.
    fn render_tool_html(
        &self,
        _context: Arc<NativeExtensionContext>,
        tool_name: String,
        phase: String,
        payload: Value,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        let _ = (tool_name, phase, payload);
        Box::pin(async { Ok(json!({})) })
    }

    /// Run one lifecycle hook (open method strings such as
    /// `session_start`).
    ///
    /// Only advertised event types route here: the allowlist derives from
    /// the cached snapshot's `handlers`, and requests to
    /// `message_update_delta` invoke the hook under the advertised
    /// `message_update` key; `event_type` is always the advertised key
    /// while `payload` is the request payload verbatim. `events` emits
    /// unsolicited id-`0` frames (`uiSlot`, `notify`, …). The returned
    /// payload is answered verbatim; the default answers `{}` (hook
    /// observed, no mutation).
    fn on_lifecycle(
        &self,
        _context: Arc<NativeExtensionContext>,
        event_type: String,
        payload: Value,
        events: NativeEventSink,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        let _ = (event_type, payload, events);
        Box::pin(async { Ok(json!({})) })
    }

    /// Observe a `theme.update` broadcast from the host.
    ///
    /// Called when the host pushes the active theme (initial or on change).
    /// The default is a no-op; extensions emitting styled `uiSlot` content
    /// override this to track palette, polarity, and generation.
    fn on_theme_update(&self, _update: crate::protocol::ThemeUpdate) {}
}

// ---------------------------------------------------------------------------
// Server driver
// ---------------------------------------------------------------------------

/// Serve a native extension over stdin/stdout with default bounds.
///
/// Runs until EOF on stdin or a transport/protocol failure.
///
/// # Errors
///
/// Returns [`ServerError`] on handshake, protocol, bounded overload, or I/O failure.
pub async fn serve<E: NativeExtension>(extension: E) -> Result<(), ServerError> {
    // Bridge OS stdio through in-memory duplex streams. Do not use Tokio
    // `io-std` stdin: docs note an uncancellable blocking read that can hang
    // runtime shutdown. The stdin pump must be joinable: a detached
    // `spawn_blocking` read keeps a current-thread runtime alive when the
    // parent holds the pipe open after an early `serve_io` exit (e.g. bad hello).
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;

        // Own private fds so the pump does not contend on the global Stdin mutex
        // or its userspace buffer (StdinLock is also !Send).
        let stdin = std::fs::File::from(
            std::io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .map_err(ServerError::Io)?,
        );
        let stdout = std::fs::File::from(
            std::io::stdout()
                .as_fd()
                .try_clone_to_owned()
                .map_err(ServerError::Io)?,
        );
        serve_stdio(stdin, stdout, extension, ServerConfig::default()).await
    }
    #[cfg(not(unix))]
    {
        serve_stdio_detached(extension).await
    }
}

/// Serve over caller-provided blocking stdio, joining pumps before return.
///
/// On Unix the stdin pump `poll`s the reader alongside a self-pipe; dropping
/// the wake write-end unblocks a stuck read so the thread cannot outlive
/// [`serve_io`] on the normal return path (wake then join). This is not
/// outer-future cancellation/Drop-safe: an aborted `serve_stdio` future still
/// drops the wake writer (unblocking `poll`) but does not join the thread.
/// Tokio `io-std` stdin is not an alternative — it uses an uncancellable
/// blocking read that can hang runtime shutdown. Used by [`serve`] and the
/// stdin-lifecycle regression test.
#[cfg(unix)]
async fn serve_stdio<R, W, E>(
    stdin: R,
    stdout: W,
    extension: E,
    config: ServerConfig,
) -> Result<(), ServerError>
where
    R: std::io::Read + std::os::fd::AsFd + Send + 'static,
    W: std::io::Write + Send + 'static,
    E: NativeExtension,
{
    let (stdin_reader, stdin_sink) = tokio::io::duplex(64 * 1024);
    let (stdout_source, stdout_writer) = tokio::io::duplex(64 * 1024);
    let (wake_reader, wake_writer) = std::io::pipe()?;

    let stdin_pump = std::thread::Builder::new()
        .name("pi-ext-stdin".into())
        .spawn(move || {
            let mut stdin = stdin;
            let mut stdin_sink = stdin_sink;
            pump_stdin_cancellable(&mut stdin, &mut stdin_sink, &wake_reader);
            drop(wake_reader);
        })
        .map_err(ServerError::Io)?;

    let stdout_pump = match std::thread::Builder::new()
        .name("pi-ext-stdout".into())
        .spawn(move || pump_stdout(stdout_source, stdout))
    {
        Ok(handle) => handle,
        Err(error) => {
            drop(wake_writer);
            let _ = stdin_pump.join();
            return Err(ServerError::Io(error));
        }
    };

    let result = serve_io(stdin_reader, stdout_writer, extension, config).await;
    // Unblock poll, then join — do not detach. `spawn_blocking` abort cannot
    // stop a running stdin `read` / `poll`.
    drop(wake_writer);
    let _ = stdin_pump.join();
    let _ = stdout_pump.join();
    result
}

/// Non-Unix fallback: dedicated threads are not owned by Tokio, so a stuck
/// stdin read cannot wedge current-thread runtime shutdown. The pump may
/// still outlive `serve_io` until process exit (no portable interrupt).
#[cfg(not(unix))]
async fn serve_stdio_detached<E: NativeExtension>(extension: E) -> Result<(), ServerError> {
    let (stdin_reader, mut stdin_sink) = tokio::io::duplex(64 * 1024);
    let (stdout_source, stdout_writer) = tokio::io::duplex(64 * 1024);
    let _stdin_pump = std::thread::Builder::new()
        .name("pi-ext-stdin".into())
        .spawn(move || {
            use std::io::Read as _;
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 8192];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if block_on(stdin_sink.write_all(&buf[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(ServerError::Io)?;
    let stdout_pump = std::thread::Builder::new()
        .name("pi-ext-stdout".into())
        .spawn(move || pump_stdout(stdout_source, std::io::stdout()))
        .map_err(ServerError::Io)?;
    let result = serve_io(
        stdin_reader,
        stdout_writer,
        extension,
        ServerConfig::default(),
    )
    .await;
    let _ = stdout_pump.join();
    result
}

/// Pump OS/test stdin into the async duplex until EOF or wake-pipe shutdown.
#[cfg(unix)]
fn pump_stdin_cancellable<R>(
    reader: &mut R,
    sink: &mut tokio::io::DuplexStream,
    wake: &std::io::PipeReader,
) where
    R: std::io::Read + std::os::fd::AsFd,
{
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::AsFd;

    // Closing the wake write-end surfaces POLLHUP (not necessarily POLLIN).
    let stop_mask =
        PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL;
    let mut buf = [0u8; 8192];
    loop {
        let mut fds = [
            // Wake first: shutdown must win over stdin readability.
            PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            PollFd::new(reader.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return,
            Ok(_) => {}
        }
        let wake_revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        let stdin_revents = fds[1].revents().unwrap_or_else(PollFlags::empty);
        if wake_revents.intersects(stop_mask) {
            return;
        }
        let stdin_readable = stdin_revents.contains(PollFlags::POLLIN);
        let stdin_hup =
            stdin_revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL);
        if !stdin_readable {
            if stdin_hup {
                return;
            }
            continue;
        }
        match reader.read(&mut buf) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if block_on(sink.write_all(&buf[..n])).is_err() {
                    return;
                }
            }
        }
    }
}

fn pump_stdout(mut source: tokio::io::DuplexStream, mut stdout: impl std::io::Write) {
    let mut buf = [0u8; 8192];
    loop {
        match block_on(source.read(&mut buf)) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                if stdout.flush().is_err() {
                    break;
                }
            }
        }
    }
}

/// Serve a native extension over an arbitrary async byte stream pair.
///
/// The first inbound frame must be a `hello` request; the server validates
/// the received `protocolVersion` against the compiled [`PROTOCOL_VERSION`]
/// (protocol-only handshake) and answers with the compiled [`HelloAck`]
/// constants. Returns `Ok(())` on clean EOF.
///
/// # Errors
///
/// Returns [`ServerError`] on handshake, protocol, bounded overload, or I/O
/// failure.
pub async fn serve_io<R, W, E>(
    reader: R,
    writer: W,
    extension: E,
    config: ServerConfig,
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    E: NativeExtension,
{
    let (runtime, out_rx) = ServerRuntime::new(extension, config);
    serve_io_inner(reader, writer, Arc::new(runtime), out_rx).await
}

/// Shared driver behind [`serve_io`]. Tests construct the runtime directly
/// so they can observe in-flight bookkeeping.
async fn serve_io_inner<R, W, E>(
    reader: R,
    writer: W,
    runtime: Arc<ServerRuntime<E>>,
    mut out_rx: mpsc::Receiver<OutboundFrame>,
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    E: NativeExtension,
{
    let writer_dead = CancellationToken::new();
    let mut tasks: JoinSet<()> = JoinSet::new();
    let (rejection_tx, mut rejection_rx) = mpsc::channel::<Frame>(runtime.out_tx.max_capacity());

    let rejection_flusher = {
        let out_tx = runtime.out_tx.clone();
        let writer_dead = writer_dead.clone();
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    () = writer_dead.cancelled() => return,
                    frame = rejection_rx.recv() => frame,
                };
                let Some(frame) = frame else {
                    return;
                };
                let sent = tokio::select! {
                    biased;
                    () = writer_dead.cancelled() => return,
                    sent = out_tx.send(frame.into()) => sent,
                };
                if sent.is_err() {
                    return;
                }
            }
        })
    };

    let writer_shutdown = CancellationToken::new();
    let writer_task = {
        let writer_dead = writer_dead.clone();
        let shutdown = writer_shutdown.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            let result: Result<(), ServerError> = async {
                loop {
                    let frame = tokio::select! {
                        frame = out_rx.recv() => frame,
                        () = shutdown.cancelled() => {
                            // Explicit shutdown: close the receiver so the loop
                            // drains buffered frames then stops, regardless of
                            // retained event-sink sender clones.
                            out_rx.close();
                            out_rx.recv().await
                        }
                    };
                    let Some(frame) = frame else { break };
                    let bytes = match frame {
                        OutboundFrame::Encoded(bytes) => bytes,
                        OutboundFrame::Structured(frame) => {
                            // Contain per-frame encode failures: one bad payload
                            // must not kill the endpoint or sibling requests.
                            let Some(bytes) = encode_frame(&frame)
                                .ok()
                                .or_else(|| encode_fallback(&frame))
                            else {
                                continue;
                            };
                            bytes
                        }
                    };
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
                writer.shutdown().await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                writer_dead.cancel();
            }
            result
        })
    };

    let run_result = drive(reader, &runtime, &writer_dead, &rejection_tx, &mut tasks).await;

    if run_result.is_err() {
        // Fatal read/dispatch errors must not wait on a peer that already
        // stopped draining the writer or deferred-rejection path.
        rejection_flusher.abort();
        writer_task.abort();
    }

    // Teardown: cancel cooperative executions, stop request tasks, drop the
    // runtime, then signal the writer. The explicit shutdown signal lets the
    // writer drain buffered frames and stop regardless of retained
    // event-sink sender clones held by detached background work.
    runtime
        .in_flight
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .for_each(|entry| entry.token.cancel());
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        let _ = joined;
    }
    drop(rejection_tx);
    drop(runtime);
    // Explicit shutdown signal: the writer drains buffered frames and stops
    // regardless of retained event-sink sender clones (detached work).
    writer_shutdown.cancel();
    let _ = rejection_flusher.await;
    let write_result = writer_task.await;

    match (run_result, write_result) {
        (Ok(()), Ok(Ok(()))) => Ok(()),
        (Err(e), _) | (Ok(()), Ok(Err(e))) => Err(e),
        (Ok(()), Err(join)) => Err(ServerError::Io(std::io::Error::other(join))),
    }
}

/// Minimal `block_on` for the stdio pump threads (the `futures` executor
/// feature is not enabled for this crate). Parks the current thread until
/// the pumped duplex I/O future resolves.
fn block_on<F: Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};

    struct Parker(std::thread::Thread);
    impl Wake for Parker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Parker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Inbound state: the first frame must complete the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    /// Waiting for the `hello` request.
    WaitingHello,
    /// Handshake complete; serving requests.
    Ready,
}

/// Operation class for an in-flight cancellable request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlightKind {
    Tool,
    Provider,
}

/// Cancellation state retained until a streaming request terminates.
struct InFlightEntry {
    token: CancellationToken,
    kind: InFlightKind,
}

/// Shared server state: one per `serve_io`, shared by the read loop and
/// every spawned request task. Dropping the last `Arc` clone closes the
/// outbound channel, so the writer drains only after all tasks finish.
struct ServerRuntime<E: NativeExtension> {
    /// Extension implementation served by this endpoint.
    extension: E,
    /// Immutable registry snapshot, captured once at startup.
    snapshot: RegistrySnapshot,
    /// Lifecycle allowlist derived from the cached snapshot's `handlers`.
    handlers: HashSet<String>,
    /// Bound on concurrently handled requests.
    semaphore: Arc<Semaphore>,
    /// Cancellation state for in-flight `tool.execute` and `provider.stream`
    /// calls, keyed by frame id.
    in_flight: Mutex<HashMap<FrameId, InFlightEntry>>,
    /// Last valid session context. `None` until the first accepted
    /// `extensions.load`; cloned snapshots are never held under this lock
    /// across callback awaits.
    context: Mutex<Option<Arc<NativeExtensionContext>>>,
    /// Shared outbound (server → client) frame channel.
    out_tx: mpsc::Sender<OutboundFrame>,
    /// Bound on queued per-call streaming updates/events.
    update_capacity: usize,
    /// Server-side deadline for native callbacks without explicit cancellation.
    lifecycle_deadline: std::time::Duration,
}

impl<E: NativeExtension> ServerRuntime<E> {
    /// Capture the immutable snapshot once and derive the lifecycle
    /// allowlist from it. Returns the runtime plus the outbound receiver.
    fn new(extension: E, config: ServerConfig) -> (Self, mpsc::Receiver<OutboundFrame>) {
        let (out_tx, out_rx) = mpsc::channel(config.outbound_capacity.max(1));
        let snapshot = extension.snapshot();
        let handlers = snapshot.handlers.iter().cloned().collect();
        (
            Self {
                extension,
                snapshot,
                handlers,
                semaphore: Arc::new(Semaphore::new(config.max_in_flight.max(1))),
                in_flight: Mutex::new(HashMap::new()),
                context: Mutex::new(None),
                out_tx,
                update_capacity: config.update_capacity,
                lifecycle_deadline: config.lifecycle_deadline,
            },
            out_rx,
        )
    }

    /// Clone the current immutable session context without retaining the
    /// runtime lock into asynchronous extension work.
    fn capture_context(&self) -> Option<Arc<NativeExtensionContext>> {
        self.context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Read/dispatch loop: runs until EOF, writer death, or a fatal error.
async fn drive<R, E>(
    reader: R,
    runtime: &Arc<ServerRuntime<E>>,
    writer_dead: &CancellationToken,
    rejection_tx: &mpsc::Sender<Frame>,
    tasks: &mut JoinSet<()>,
) -> Result<(), ServerError>
where
    R: AsyncRead + Unpin + Send,
    E: NativeExtension,
{
    let mut reader = BufReader::new(reader);
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 8192];
    let mut state = ServerState::WaitingHello;
    loop {
        let read = tokio::select! {
            biased;
            () = writer_dead.cancelled() => break,
            read = reader.read(&mut buf) => read,
        };
        let n = match read {
            Ok(0) => {
                decoder
                    .finish()
                    .map_err(|e| ServerError::Protocol(e.to_string()))?;
                break;
            }
            Ok(n) => n,
            Err(e) => return Err(ServerError::Io(e)),
        };
        let frames = decoder
            .push(&buf[..n])
            .map_err(|e| ServerError::Protocol(e.to_string()))?;
        for frame in frames {
            match state {
                ServerState::WaitingHello => {
                    validate_hello(&frame)?;
                    let ack = Frame {
                        id: frame.id,
                        kind: FrameKind::Res,
                        method: Method::Hello.as_str().to_owned(),
                        payload: to_payload(&HelloAck::local())
                            .map_err(|e| ServerError::Protocol(format!("encode helloAck: {e}")))?,
                    };
                    runtime.out_tx.send(ack.into()).await.map_err(|_| {
                        ServerError::Io(std::io::Error::other("outbound channel closed"))
                    })?;
                    state = ServerState::Ready;
                }
                ServerState::Ready => {
                    dispatch_ready(frame, runtime, rejection_tx, tasks)?;
                }
            }
        }
    }
    Ok(())
}

/// Validate the first frame as a protocol-only `hello`.
fn validate_hello(frame: &Frame) -> Result<(), ServerError> {
    if frame.kind != FrameKind::Req || frame.method != Method::Hello.as_str() {
        return Err(ServerError::Handshake(format!(
            "expected hello as first frame, got: {}",
            frame.method
        )));
    }
    let hello: Hello = from_payload(&frame.payload)
        .map_err(|e| ServerError::Handshake(format!("decode hello: {e}")))?;
    // Protocol-only: compatibilityVersion is deliberately ignored.
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(ServerError::Handshake(format!(
            "protocol version mismatch: remote={} local={PROTOCOL_VERSION}",
            hello.protocol_version
        )));
    }
    Ok(())
}

fn handle_theme_update<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    rejection_tx: &mpsc::Sender<Frame>,
    payload: &Value,
) -> Result<(), ServerError> {
    let Ok(update) = from_payload(payload) else {
        return Ok(());
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.extension.on_theme_update(update);
    }))
    .is_ok()
    {
        return Ok(());
    }

    let failure = Frame::event(
        0,
        Method::ExtensionError,
        json!({
            "code": "internal",
            "message": "native extension theme callback panicked",
            "retryable": false,
        }),
    );
    match rejection_tx.try_send(failure) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(ServerError::OutboundOverflow),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(ServerError::Io(
            std::io::Error::other("deferred extension error channel closed"),
        )),
    }
}

/// Dispatch one post-handshake frame.
fn dispatch_ready<E: NativeExtension>(
    frame: Frame,
    runtime: &Arc<ServerRuntime<E>>,
    rejection_tx: &mpsc::Sender<Frame>,
    tasks: &mut JoinSet<()>,
) -> Result<(), ServerError> {
    match frame.kind {
        FrameKind::Req => {
            if let Ok(permit) = runtime.semaphore.clone().try_acquire_owned() {
                // Register the cancel token BEFORE spawning: a cancel event
                // read right after this request must find its target.
                let kind = match frame.method.as_str() {
                    methods::TOOL_EXECUTE => Some(InFlightKind::Tool),
                    methods::PROVIDER_STREAM => Some(InFlightKind::Provider),
                    _ => None,
                };
                let token = kind.map(|kind| {
                    let token = CancellationToken::new();
                    runtime
                        .in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            frame.id,
                            InFlightEntry {
                                token: token.clone(),
                                kind,
                            },
                        );
                    token
                });
                let runtime = Arc::clone(runtime);
                tasks.spawn(async move {
                    let _permit = permit;
                    handle_request(runtime, frame, token).await;
                });
                // Reap finished tasks so the set does not grow per request.
                while tasks.try_join_next().is_some() {}
            } else {
                // Defer the correlated rejection without stalling reads. If
                // this bounded queue also fills, the peer is not draining;
                // fail the transport instead of orphaning another request.
                let overloaded = error_frame(
                    frame.id,
                    &frame.method,
                    "overloaded",
                    "too many in-flight requests",
                    true,
                );
                match rejection_tx.try_send(overloaded) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        return Err(ServerError::OutboundOverflow);
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        return Err(ServerError::Io(std::io::Error::other(
                            "deferred rejection channel closed",
                        )));
                    }
                }
            }
        }
        FrameKind::Event => {
            match frame.method.as_str() {
                methods::TOOL_CANCEL | methods::PROVIDER_CANCEL => {
                    let kind = if frame.method == methods::TOOL_CANCEL {
                        InFlightKind::Tool
                    } else {
                        InFlightKind::Provider
                    };
                    if let Some(id) = frame.payload.get("id").and_then(Value::as_u64)
                        && let Some(entry) = runtime
                            .in_flight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&id)
                        && entry.kind == kind
                    {
                        entry.token.cancel();
                    }
                }
                crate::protocol::THEME_UPDATE_METHOD => {
                    handle_theme_update(runtime, rejection_tx, &frame.payload)?;
                }
                // Unknown events are fire-and-forget: ignored by design.
                _ => {}
            }
        }
        // The native endpoint initiates no requests; stray res/error frames
        // carry no correlation state and are ignored.
        FrameKind::Res | FrameKind::Error => {}
    }
    Ok(())
}

fn remove_in_flight<E: NativeExtension>(runtime: &ServerRuntime<E>, id: FrameId) {
    runtime
        .in_flight
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
}

/// Handle one request to a terminal frame and send it.
///
/// The dispatch runs in a child task so a panicking extension callback is
/// caught by the `JoinHandle` instead of silently unwinding the request task.
/// On panic the server cleans up the in-flight entry and answers with a
/// correlated `internal` error frame.
async fn handle_request<E: NativeExtension>(
    runtime: Arc<ServerRuntime<E>>,
    frame: Frame,
    token: Option<CancellationToken>,
) {
    let id = frame.id;
    let method = frame.method.clone();
    let result = tokio::spawn(handle_request_dispatch(Arc::clone(&runtime), frame, token)).await;
    if result.is_err() {
        remove_in_flight(&runtime, id);
        let _ = runtime
            .out_tx
            .send(
                error_frame(
                    id,
                    &method,
                    "internal",
                    "extension callback panicked",
                    false,
                )
                .into(),
            )
            .await;
    }
}

/// Inner dispatch: route one request to its handler and send the terminal frame.
async fn handle_request_dispatch<E: NativeExtension>(
    runtime: Arc<ServerRuntime<E>>,
    frame: Frame,
    token: Option<CancellationToken>,
) {
    let id = frame.id;
    let method = frame.method.clone();
    let terminal = match method.as_str() {
        EXTENSIONS_LOAD_METHOD => handle_load(&runtime, id, &method, &frame.payload),
        methods::TOOL_PREPARE => handle_prepare(&runtime, id, &method, &frame.payload).await,
        methods::TOOL_VALIDATE => handle_validate(&runtime, id, &method, &frame.payload).await,
        methods::TOOL_EXECUTE => {
            execute_tool_request(&runtime, id, &method, frame.payload, token).await;
            return;
        }
        methods::PROVIDER_STREAM => {
            execute_provider_request(&runtime, id, &method, frame.payload, token).await;
            return;
        }
        COMMAND_EXECUTE_METHOD => handle_command(&runtime, id, &method, &frame.payload).await,
        FLAGS_SET_METHOD => handle_flags_set(&runtime, id, &method, &frame.payload).await,
        SHORTCUT_EXECUTE_METHOD => handle_shortcut(&runtime, id, &method, &frame.payload).await,
        TOOL_RENDER_HTML_METHOD => handle_render_html(&runtime, id, &method, &frame.payload).await,
        m if m == Method::TerminalInput.as_str() => {
            handle_terminal_input(&runtime, id, &method, &frame.payload).await
        }
        m if m == Method::Hello.as_str() => error_frame(
            id,
            &method,
            "invalid_request",
            "hello already completed",
            false,
        ),
        m => {
            // Lifecycle: only advertised event types route (allowlist from
            // the cached snapshot); everything else fails closed. Compact
            // `message_update_delta` requests map onto the advertised
            // `message_update` handler key.
            let advertised = if m == MESSAGE_UPDATE_DELTA_METHOD {
                MESSAGE_UPDATE_HANDLER
            } else {
                m
            };
            if runtime.handlers.contains(advertised) {
                handle_lifecycle(&runtime, id, &method, advertised, frame.payload).await
            } else {
                error_frame(
                    id,
                    &method,
                    "unknown_method",
                    &format!("unknown method: {method}"),
                    false,
                )
            }
        }
    };
    let _ = runtime.out_tx.send(terminal.into()).await;
}

/// `extensions.load`: publish its session context and encode the cached registry snapshot mirror.
fn handle_load<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
        return error_frame(
            id,
            method,
            "invalid_request",
            "extensions.load requires a string cwd",
            false,
        );
    };

    let context = Arc::new(NativeExtensionContext {
        cwd: cwd.to_owned(),
        project_trusted: payload.get("projectTrusted") == Some(&Value::Bool(true)),
    });
    *runtime
        .context
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context);
    match serde_json::to_value(&runtime.snapshot) {
        Ok(payload) => res_frame(id, method, payload),
        Err(e) => error_frame(
            id,
            method,
            "extension_error",
            &format!("encode registry snapshot: {e}"),
            false,
        ),
    }
}

/// Capture the request's immutable context or fail closed before an extension
/// callback starts.
fn callback_context<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
) -> Result<Arc<NativeExtensionContext>, Frame> {
    runtime.capture_context().ok_or_else(|| {
        error_frame(
            id,
            method,
            "extension_not_loaded",
            "extensions.load must succeed before invoking native callbacks",
            false,
        )
    })
}

/// Await a non-cancellable extension callback under the shared server deadline.
async fn await_callback<T>(
    deadline: std::time::Duration,
    callback: NativeFuture<Result<T, ExtensionFault>>,
) -> Result<Result<T, ExtensionFault>, tokio::time::error::Elapsed> {
    tokio::time::timeout(deadline, callback).await
}

/// Convert an expired callback deadline into the normal correlated error frame.
fn callback_timeout_frame(id: FrameId, method: &str) -> Frame {
    error_frame(
        id,
        method,
        "timeout",
        "extension callback exceeded its server-side deadline",
        false,
    )
}

/// `tool.prepare`: run the extension's argument preparation.
async fn handle_prepare<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return error_frame(
            id,
            method,
            "invalid_request",
            "tool.prepare requires a string name",
            false,
        );
    };
    let args = payload.get("args").cloned().unwrap_or(Value::Null);
    match await_callback(
        runtime.lifecycle_deadline,
        runtime
            .extension
            .prepare_tool(context, name.to_owned(), args),
    )
    .await
    {
        Ok(Ok(args)) => res_frame(id, method, json!({ "args": args })),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// `tool.validate`: run the extension's argument validation.
async fn handle_validate<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return error_frame(
            id,
            method,
            "invalid_request",
            "tool.validate requires a string name",
            false,
        );
    };
    let args = payload.get("args").cloned().unwrap_or(Value::Null);
    let tool_call_id = payload
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match await_callback(
        runtime.lifecycle_deadline,
        runtime
            .extension
            .validate_tool(context, name.to_owned(), args, tool_call_id),
    )
    .await
    {
        Ok(Ok(args)) => res_frame(id, method, json!({ "args": args })),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// `command.execute`: run a slash command.
async fn handle_command<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let command = payload
        .get("command")
        .or_else(|| payload.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = payload
        .get("args")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match await_callback(
        runtime.lifecycle_deadline,
        runtime.extension.execute_command(context, command, args),
    )
    .await
    {
        Ok(Ok(())) => res_frame(id, method, json!({ "ok": true })),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// `flags.set`: apply the validated flag overlay.
async fn handle_flags_set<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let request = match from_payload::<FlagsSetRequest>(payload) {
        Ok(request) => request,
        Err(e) => {
            return error_frame(
                id,
                method,
                "invalid_request",
                &format!("flags.set requires a values object of boolean/string: {e}"),
                false,
            );
        }
    };
    match await_callback(
        runtime.lifecycle_deadline,
        runtime.extension.set_flags(context, request.values),
    )
    .await
    {
        Ok(Ok(ok)) => res_frame(id, method, json!({ "ok": ok })),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// `shortcut.execute`: dispatch one key; unowned keys answer
/// `handled: false` rather than an error, matching the TypeScript hosts.
async fn handle_shortcut<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let Some(key) = payload.get("key").and_then(Value::as_str) else {
        // Missing/null/number/bool/array/object key: answer `handled: false`
        // without invoking extension code, matching both TypeScript hosts
        // (`typeof key !== "string"` short-circuit). Valid strings, including
        // the empty string, still dispatch below.
        return res_frame(id, method, json!({ "handled": false }));
    };
    let key = key.to_owned();
    match await_callback(
        runtime.lifecycle_deadline,
        runtime.extension.execute_shortcut(context, key),
    )
    .await
    {
        Ok(Ok(handled)) => res_frame(id, method, json!({ "handled": handled })),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// `terminalInput`: rewrite or consume one input chunk.
async fn handle_terminal_input<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let data = payload
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match tokio::time::timeout(
        NATIVE_TERMINAL_INPUT_BUDGET,
        runtime.extension.handle_terminal_input(context, data),
    )
    .await
    {
        Ok(Ok(result)) => res_frame(
            id,
            method,
            to_payload(&result).unwrap_or_else(|_| json!({})),
        ),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => error_frame(
            id,
            method,
            "timeout",
            "terminal input callback exceeded its 4ms budget",
            false,
        ),
    }
}

/// `tool.renderHtml`: render one tool phase; endpoints without a renderer
/// answer `{}`.
async fn handle_render_html<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let phase = payload
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_name = payload
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let body = payload.get("payload").cloned().unwrap_or(Value::Null);
    match await_callback(
        runtime.lifecycle_deadline,
        runtime
            .extension
            .render_tool_html(context, tool_name, phase, body),
    )
    .await
    {
        Ok(Ok(value)) => res_frame(id, method, value),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

/// Lifecycle hook (open method strings): run the advertised handler with a
/// bounded sink for unsolicited id-`0` events, under a finite server-side
/// deadline. A hook that never resolves is dropped at the deadline so its
/// in-flight permit is released and the peer receives a correlated
/// `timeout` error instead of the request running forever.
async fn handle_lifecycle<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    event_type: &str,
    payload: Value,
) -> Frame {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let events = NativeEventSink {
        tx: runtime.out_tx.clone(),
    };
    match await_callback(
        runtime.lifecycle_deadline,
        runtime
            .extension
            .on_lifecycle(context, event_type.to_owned(), payload, events),
    )
    .await
    {
        Ok(Ok(value)) => res_frame(id, method, value),
        Ok(Err(fault)) => fault_frame(id, method, &fault),
        Err(_) => callback_timeout_frame(id, method),
    }
}

async fn send_forwarded_frame<T: Into<OutboundFrame>>(
    out: &mpsc::Sender<OutboundFrame>,
    cancel: &CancellationToken,
    frame: T,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = out.send(frame.into()) => sent.is_ok(),
    }
}

/// Run one `tool.execute` call: stream `toolUpdate` events while the
/// execution future is active, honor `tool.cancel`, then send the terminal
/// frame. Cancellation maps to a `cancelled` error regardless of the value
/// the extension returned, matching the TypeScript host.
async fn execute_tool_request<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: Value,
    token: Option<CancellationToken>,
) {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(terminal) => {
            remove_in_flight(runtime, id);
            let _ = runtime.out_tx.send(terminal.into()).await;
            return;
        }
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        remove_in_flight(runtime, id);
        let _ = runtime
            .out_tx
            .send(
                error_frame(
                    id,
                    method,
                    "invalid_request",
                    "tool.execute requires a string name",
                    false,
                )
                .into(),
            )
            .await;
        return;
    };
    let tool_call_id = payload
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let call = ToolCall {
        name: name.to_owned(),
        tool_call_id: tool_call_id.clone(),
        args: payload.get("args").cloned().unwrap_or(Value::Null),
        prepared: payload.get("prepared") == Some(&Value::Bool(true)),
    };

    // The token was registered by the dispatcher before this task spawned,
    // so an early `tool.cancel` is never lost.
    let token = token.unwrap_or_default();
    let (update_tx, mut update_rx) = mpsc::channel::<Value>(runtime.update_capacity.max(1));
    let (done_tx, mut done_rx) = oneshot::channel::<()>();

    // The execution-done signal bounds the forwarder's lifetime even when an
    // extension moves its sink into detached work. Updates queued before the
    // extension returns are drained; detached post-return updates are ignored.
    let forwarder = {
        let out = runtime.out_tx.clone();
        let cancel = token.clone();
        let tool_call_id = tool_call_id.clone();
        let name = call.name.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = &mut done_rx => break,
                    partial = update_rx.recv() => {
                        let Some(partial) = partial else {
                            return;
                        };
                        let frame = update_frame(id, &tool_call_id, &name, partial);
                        if !send_forwarded_frame(&out, &cancel, frame).await {
                            return;
                        }
                    }
                }
            }
            update_rx.close();
            while let Some(partial) = update_rx.recv().await {
                let frame = update_frame(id, &tool_call_id, &name, partial);
                if !send_forwarded_frame(&out, &cancel, frame).await {
                    return;
                }
            }
        })
    };

    let result = runtime
        .extension
        .execute_tool(
            context,
            call,
            ToolUpdateSink { tx: update_tx },
            token.clone(),
        )
        .await;
    let _ = done_tx.send(());

    // The done signal closes the receiver and flushes all updates accepted
    // before completion. Await the flush before publishing the terminal.
    let _ = forwarder.await;
    remove_in_flight(runtime, id);

    let terminal = if token.is_cancelled() {
        error_frame(id, method, "cancelled", "extension tool cancelled", false)
    } else {
        match result {
            Ok(value) => res_frame(id, method, value),
            Err(fault) => fault_frame(id, method, &fault),
        }
    };
    let _ = runtime.out_tx.send(terminal.into()).await;
}

/// Run one `provider.stream` call: forward correlated `providerEvent`
/// frames while the stream is active, honor `provider.cancel`, then send
/// the terminal frame. Terminal precedence is `invalid_payload` (an event
/// failed encode pre-validation — never report success after loss) over
/// `cancelled` over the returned value/fault.
async fn execute_provider_request<E: NativeExtension>(
    runtime: &ServerRuntime<E>,
    id: FrameId,
    method: &str,
    payload: Value,
    token: Option<CancellationToken>,
) {
    let context = match callback_context(runtime, id, method) {
        Ok(context) => context,
        Err(terminal) => {
            remove_in_flight(runtime, id);
            let _ = runtime.out_tx.send(terminal.into()).await;
            return;
        }
    };
    let call = ProviderStreamCall {
        provider_id: payload
            .get("providerId")
            .or_else(|| payload.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        model: payload.get("model").cloned().unwrap_or(Value::Null),
        context: payload.get("context").cloned().unwrap_or(Value::Null),
        options: payload.get("options").cloned().unwrap_or(Value::Null),
    };

    // The token was registered by the dispatcher before this task spawned,
    // so an early `provider.cancel` is never lost.
    let token = token.unwrap_or_default();
    let (event_tx, mut event_rx) = mpsc::channel::<OutboundFrame>(runtime.update_capacity.max(1));
    let (done_tx, mut done_rx) = oneshot::channel::<()>();
    let invalid = Arc::new(AtomicBool::new(false));
    let sink = ProviderEventSink {
        id,
        tx: event_tx,
        cancel: token.clone(),
        invalid: Arc::clone(&invalid),
    };

    // Match the tool forwarder: completion, not sender ownership, defines the
    // event stream lifetime. Drain only events queued before completion.
    let forwarder = {
        let out = runtime.out_tx.clone();
        let cancel = token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    _ = &mut done_rx => break,
                    frame = event_rx.recv() => {
                        let Some(frame) = frame else {
                            return;
                        };
                        if !send_forwarded_frame(&out, &cancel, frame).await {
                            return;
                        }
                    }
                }
            }
            event_rx.close();
            while let Some(frame) = event_rx.recv().await {
                if !send_forwarded_frame(&out, &cancel, frame).await {
                    return;
                }
            }
        })
    };

    let result = runtime
        .extension
        .stream_provider(context, call, sink, token.clone())
        .await;
    let _ = done_tx.send(());

    // The done signal closes the receiver and flushes all events accepted
    // before completion. Await the flush before publishing the terminal.
    let _ = forwarder.await;
    remove_in_flight(runtime, id);

    let terminal = if invalid.load(Ordering::SeqCst) {
        error_frame(
            id,
            method,
            "invalid_payload",
            "provider event could not be encoded",
            false,
        )
    } else if token.is_cancelled() {
        error_frame(id, method, "cancelled", "provider stream cancelled", false)
    } else {
        match result {
            Ok(value) => res_frame(id, method, value),
            Err(fault) => fault_frame(id, method, &fault),
        }
    };
    let _ = runtime.out_tx.send(terminal.into()).await;
}

/// Build a correlated success response for an open method string.
fn res_frame(id: FrameId, method: &str, payload: Value) -> Frame {
    Frame {
        id,
        kind: FrameKind::Res,
        method: method.to_owned(),
        payload,
    }
}

/// Build a correlated error response for an open method string.
fn error_frame(id: FrameId, method: &str, code: &str, message: &str, retryable: bool) -> Frame {
    let payload = serde_json::to_value(ErrorPayload {
        code: code.to_owned(),
        message: message.to_owned(),
        retryable,
        data: None,
    })
    .unwrap_or_else(
        |_| json!({ "code": "internal", "message": "error encoding failed", "retryable": false }),
    );
    Frame {
        id,
        kind: FrameKind::Error,
        method: method.to_owned(),
        payload,
    }
}

/// Build a correlated error response from an extension fault.
fn fault_frame(id: FrameId, method: &str, fault: &ExtensionFault) -> Frame {
    error_frame(id, method, &fault.code, &fault.message, false)
}

/// Build a streaming `toolUpdate` event frame.
fn update_frame(id: FrameId, tool_call_id: &str, tool_name: &str, partial: Value) -> Frame {
    let payload = to_payload(&ToolUpdate {
        tool_call_id: tool_call_id.to_owned(),
        tool_name: tool_name.to_owned(),
        partial_result: partial,
    })
    .unwrap_or(Value::Null);
    Frame {
        id,
        kind: FrameKind::Event,
        method: Method::ToolUpdate.as_str().to_owned(),
        payload,
    }
}

/// Replace an unencodable correlated terminal frame with a small
/// `invalid_payload` error carrying the same id/method. Uncorrelated
/// frames (events) return `None` and are dropped by the writer.
fn encode_fallback(frame: &Frame) -> Option<Vec<u8>> {
    if frame.id == 0 || !matches!(frame.kind, FrameKind::Res | FrameKind::Error) {
        return None;
    }
    let fallback = error_frame(
        frame.id,
        &frame.method,
        "invalid_payload",
        "response payload could not be encoded",
        false,
    );
    encode_frame(&fallback).ok()
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;
    use crate::client::{HandshakePolicy, HostClient, HostClientError};
    use crate::protocol::{
        COMPATIBILITY_VERSION, CommandSnapshotEntry, FlagSnapshotEntry, ProviderSnapshotEntry,
        ShortcutSnapshotEntry, ToolSnapshotEntry, decode_frame_str,
    };
    use pi_ai::types::AssistantMessage;
    use std::error::Error;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::Notify;

    type R = Result<(), Box<dyn Error>>;

    const TIMEOUT: Duration = Duration::from_secs(5);

    // -----------------------------------------------------------------------
    // Harness
    // -----------------------------------------------------------------------

    /// Raw frame-level peer for driving the server without the full client.
    struct RawClient {
        write: tokio::io::DuplexStream,
        read: BufReader<tokio::io::DuplexStream>,
    }

    impl RawClient {
        async fn send(&mut self, frame: &Frame) -> R {
            let bytes = encode_frame(frame)?;
            self.write.write_all(&bytes).await?;
            self.write.flush().await?;
            Ok(())
        }

        async fn recv(&mut self) -> Result<Frame, Box<dyn Error>> {
            let mut line = String::new();
            let n = tokio::time::timeout(TIMEOUT, self.read.read_line(&mut line)).await??;
            assert!(n > 0, "server closed the stream before answering");
            Ok(decode_frame_str(line.trim_end())?)
        }

        async fn request(
            &mut self,
            id: FrameId,
            method: &str,
            payload: Value,
        ) -> Result<Frame, Box<dyn Error>> {
            self.send(&Frame {
                id,
                kind: FrameKind::Req,
                method: method.to_owned(),
                payload,
            })
            .await?;
            self.recv().await
        }

        async fn hello(&mut self, protocol_version: u32, compat: &str) -> R {
            self.send(&Frame {
                id: 1,
                kind: FrameKind::Req,
                method: Method::Hello.as_str().to_owned(),
                payload: json!({
                    "protocolVersion": protocol_version,
                    "compatibilityVersion": compat,
                }),
            })
            .await
        }

        async fn load_context(&mut self) -> R {
            self.send(&Frame {
                id: u64::MAX,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({
                    "extensionPaths": [],
                    "cwd": "/raw-test-context",
                    "projectTrusted": false,
                }),
            })
            .await?;
            let response = self.recv().await?;
            assert_eq!(response.id, u64::MAX);
            assert_eq!(response.kind, FrameKind::Res);
            Ok(())
        }
    }

    struct BlockingWriter {
        block: Arc<AtomicBool>,
        writes: Arc<AtomicUsize>,
    }

    impl tokio::io::AsyncWrite for BlockingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if !self.block.load(AtomicOrdering::SeqCst) {
                self.writes.fetch_add(1, AtomicOrdering::SeqCst);
                return std::task::Poll::Ready(Ok(buf.len()));
            }
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    type ServerHandle = tokio::task::JoinHandle<Result<(), ServerError>>;

    fn spawn_raw<E: NativeExtension>(ext: E, config: ServerConfig) -> (RawClient, ServerHandle) {
        let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move { serve_io(server_rx, server_tx, ext, config).await });
        (
            RawClient {
                write: client_tx,
                read: BufReader::new(client_rx),
            },
            handle,
        )
    }

    /// Shared observation handles for the scripted extension.
    struct DemoHandles {
        started: Arc<Notify>,
        release: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
        tool_cancel_token: Arc<Mutex<Option<CancellationToken>>>,
        saturated: Arc<AtomicBool>,
        commands: Arc<Mutex<Vec<(String, String)>>>,
        snapshot_calls: Arc<AtomicUsize>,
        lifecycle: Arc<Mutex<Vec<(String, Value)>>>,
        flags: Arc<Mutex<BTreeMap<String, FlagValueWire>>>,
        shortcuts: Arc<Mutex<Vec<String>>>,
        terminal_inputs: Arc<Mutex<Vec<String>>>,
        terminal_started: Arc<Notify>,
        terminal_stopped: Arc<AtomicBool>,
        renders: Arc<Mutex<Vec<(String, String)>>>,
        tool_contexts: Arc<Mutex<Vec<Arc<NativeExtensionContext>>>>,
        provider_started: Arc<Notify>,
        provider_cancelled: Arc<AtomicBool>,
        provider_cancel_token: Arc<Mutex<Option<CancellationToken>>>,
        provider_send_failed: Arc<AtomicBool>,
        provider_ticks: Arc<AtomicUsize>,
        theme_updates: Arc<Mutex<Vec<crate::protocol::ThemeUpdate>>>,
    }

    /// Scripted extension used across the server tests.
    struct DemoExtension {
        handles: DemoHandles,
    }

    impl DemoHandles {
        fn new() -> Self {
            Self {
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                cancelled: Arc::new(AtomicBool::new(false)),
                tool_cancel_token: Arc::new(Mutex::new(None)),
                saturated: Arc::new(AtomicBool::new(false)),
                commands: Arc::new(Mutex::new(Vec::new())),
                snapshot_calls: Arc::new(AtomicUsize::new(0)),
                lifecycle: Arc::new(Mutex::new(Vec::new())),
                flags: Arc::new(Mutex::new(BTreeMap::new())),
                shortcuts: Arc::new(Mutex::new(Vec::new())),
                terminal_inputs: Arc::new(Mutex::new(Vec::new())),
                terminal_started: Arc::new(Notify::new()),
                terminal_stopped: Arc::new(AtomicBool::new(false)),
                renders: Arc::new(Mutex::new(Vec::new())),
                tool_contexts: Arc::new(Mutex::new(Vec::new())),
                provider_started: Arc::new(Notify::new()),
                provider_cancelled: Arc::new(AtomicBool::new(false)),
                provider_cancel_token: Arc::new(Mutex::new(None)),
                provider_send_failed: Arc::new(AtomicBool::new(false)),
                provider_ticks: Arc::new(AtomicUsize::new(0)),
                theme_updates: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn cloned(&self) -> Self {
            Self {
                started: Arc::clone(&self.started),
                release: Arc::clone(&self.release),
                cancelled: Arc::clone(&self.cancelled),
                tool_cancel_token: Arc::clone(&self.tool_cancel_token),
                saturated: Arc::clone(&self.saturated),
                commands: Arc::clone(&self.commands),
                snapshot_calls: Arc::clone(&self.snapshot_calls),
                lifecycle: Arc::clone(&self.lifecycle),
                flags: Arc::clone(&self.flags),
                shortcuts: Arc::clone(&self.shortcuts),
                terminal_inputs: Arc::clone(&self.terminal_inputs),
                terminal_started: Arc::clone(&self.terminal_started),
                terminal_stopped: Arc::clone(&self.terminal_stopped),
                renders: Arc::clone(&self.renders),
                tool_contexts: Arc::clone(&self.tool_contexts),
                provider_started: Arc::clone(&self.provider_started),
                provider_cancelled: Arc::clone(&self.provider_cancelled),
                provider_cancel_token: Arc::clone(&self.provider_cancel_token),
                provider_send_failed: Arc::clone(&self.provider_send_failed),
                provider_ticks: Arc::clone(&self.provider_ticks),
                theme_updates: Arc::clone(&self.theme_updates),
            }
        }
    }

    fn record<T>(cell: &Mutex<Vec<T>>, value: T) {
        cell.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(value);
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, AtomicOrdering::SeqCst);
        }
    }

    /// Typed provider event carrying `delta` (valid `text_delta` wire).
    fn demo_event(delta: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: delta.to_owned(),
            partial: AssistantMessage::new("test-api", "test-provider", "m", 0),
        }
    }

    impl DemoExtension {
        fn new() -> (Self, DemoHandles) {
            let handles = DemoHandles::new();
            let ext = Self {
                handles: handles.cloned(),
            };
            (ext, handles)
        }
    }

    fn tool_entry(name: &str) -> ToolSnapshotEntry {
        ToolSnapshotEntry {
            name: name.to_owned(),
            label: format!("{name} label"),
            description: format!("{name} description"),
            parameters: json!({ "type": "object" }),
            execution_mode: None,
        }
    }

    impl NativeExtension for DemoExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            self.handles
                .snapshot_calls
                .fetch_add(1, AtomicOrdering::SeqCst);
            RegistrySnapshot {
                tools: vec![
                    tool_entry("echo"),
                    tool_entry("slow"),
                    tool_entry("block"),
                    tool_entry("scalar"),
                    tool_entry("huge"),
                    tool_entry("flood"),
                ],
                commands: vec![CommandSnapshotEntry {
                    name: "demo".to_owned(),
                    description: "record a command".to_owned(),
                    source: "native://demo".to_owned(),
                }],
                shortcuts: vec![ShortcutSnapshotEntry {
                    key: "ctrl+alt+d".to_owned(),
                    description: "demo shortcut".to_owned(),
                    extension_path: "native://demo".to_owned(),
                }],
                flags: vec![FlagSnapshotEntry {
                    name: "verbose".to_owned(),
                    description: "demo flag".to_owned(),
                    kind: "boolean".to_owned(),
                    extension_path: "native://demo".to_owned(),
                    default: Some(FlagValueWire::Boolean(false)),
                    value: None,
                }],
                renderers: vec![],
                providers: vec![
                    ProviderSnapshotEntry {
                        name: "demoProv".to_owned(),
                        stream_simple: true,
                        extension_path: Some("native://demo".to_owned()),
                        ..ProviderSnapshotEntry::default()
                    },
                    ProviderSnapshotEntry {
                        name: "slowProv".to_owned(),
                        stream_simple: true,
                        ..ProviderSnapshotEntry::default()
                    },
                    ProviderSnapshotEntry {
                        name: "hugeProv".to_owned(),
                        stream_simple: true,
                        ..ProviderSnapshotEntry::default()
                    },
                    ProviderSnapshotEntry {
                        name: "floodProv".to_owned(),
                        stream_simple: true,
                        ..ProviderSnapshotEntry::default()
                    },
                ],
                handlers: vec![
                    "session_start".to_owned(),
                    "message_update".to_owned(),
                    "stall".to_owned(),
                ],
                terminal_input: true,
                extensions: 1,
                errors: vec![],
            }
        }

        fn prepare_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move {
                match name.as_str() {
                    "echo" | "slow" | "block" | "scalar" | "huge" | "flood" => {
                        let mut args = args;
                        if let Some(map) = args.as_object_mut() {
                            map.insert("prepared".to_owned(), Value::Bool(true));
                        }
                        Ok(args)
                    }
                    other => Err(ExtensionFault::not_found(format!(
                        "Tool not found: {other}"
                    ))),
                }
            })
        }

        fn validate_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move {
                match name.as_str() {
                    "echo" | "slow" | "block" | "scalar" | "huge" | "flood" => {
                        let mut args = args;
                        if let Some(map) = args.as_object_mut() {
                            map.insert("validated".to_owned(), Value::Bool(true));
                        }
                        Ok(args)
                    }
                    other => Err(ExtensionFault::not_found(format!(
                        "Tool not found: {other}"
                    ))),
                }
            })
        }

        fn execute_tool(
            &self,
            context: Arc<NativeExtensionContext>,
            call: ToolCall,
            updates: ToolUpdateSink,
            cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let tool_cancel_token = Arc::clone(&self.handles.tool_cancel_token);
            *tool_cancel_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
            let started = Arc::clone(&self.handles.started);
            let release = Arc::clone(&self.handles.release);
            let cancelled = Arc::clone(&self.handles.cancelled);
            let saturated = Arc::clone(&self.handles.saturated);
            let tool_contexts = Arc::clone(&self.handles.tool_contexts);
            Box::pin(async move {
                record(&tool_contexts, context);
                match call.name.as_str() {
                    "echo" => {
                        let _ = updates.send(json!({ "stage": "half" }));
                        let _ = updates.send(json!({ "stage": "almost" }));
                        Ok(json!({
                            "content": [{ "type": "text", "text": "done" }],
                            "isError": false,
                        }))
                    }
                    // Waits for cancellation; returns Ok on purpose so the
                    // test proves the server maps a cancelled token onto a
                    // `cancelled` error regardless of the returned value.
                    "slow" => {
                        started.notify_one();
                        cancel.cancelled().await;
                        cancelled.store(true, AtomicOrdering::SeqCst);
                        Ok(json!({ "content": [] }))
                    }
                    // Blocks until released (bounded-overload test).
                    "block" => {
                        started.notify_one();
                        release.notified().await;
                        Ok(json!({ "done": true }))
                    }
                    // Scalar result: fails frame validation at encode time.
                    "scalar" => Ok(json!(42)),
                    // Oversize result: exceeds MAX_FRAME_BYTES at encode time.
                    "huge" => Ok(json!({ "blob": "x".repeat(9 * 1024 * 1024) })),
                    // Floods updates until the bounded pipeline saturates,
                    // then waits for cancellation (overload/cancel test).
                    "flood" => {
                        started.notify_one();
                        loop {
                            if cancel.is_cancelled() {
                                cancelled.store(true, AtomicOrdering::SeqCst);
                                return Ok(json!({ "content": [] }));
                            }
                            if !updates.send(json!({ "tick": 1 })) {
                                saturated.store(true, AtomicOrdering::SeqCst);
                            }
                            tokio::task::yield_now().await;
                        }
                    }
                    other => Err(ExtensionFault::not_found(format!(
                        "Tool not found: {other}"
                    ))),
                }
            })
        }

        fn execute_command(
            &self,
            _context: Arc<NativeExtensionContext>,
            command: String,
            args: String,
        ) -> NativeFuture<Result<(), ExtensionFault>> {
            let commands = Arc::clone(&self.handles.commands);
            Box::pin(async move {
                if command == "stall" {
                    std::future::pending::<()>().await;
                }
                if command == "demo" {
                    commands
                        .lock()
                        .map_err(|e| ExtensionFault::extension_error(e.to_string()))?
                        .push((command, args));
                    Ok(())
                } else {
                    Err(ExtensionFault::not_found(format!(
                        "Command not found: {command}"
                    )))
                }
            })
        }

        fn stream_provider(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ProviderStreamCall,
            events: ProviderEventSink,
            cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let provider_cancel_token = Arc::clone(&self.handles.provider_cancel_token);
            *provider_cancel_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cancel.clone());
            let started = Arc::clone(&self.handles.provider_started);
            let cancelled = Arc::clone(&self.handles.provider_cancelled);
            let send_failed = Arc::clone(&self.handles.provider_send_failed);
            let ticks = Arc::clone(&self.handles.provider_ticks);
            Box::pin(async move {
                match call.provider_id.as_str() {
                    // Two events, then a clean terminal.
                    "demoProv" => {
                        if !events.send(demo_event("one")).await
                            || !events.send(demo_event("two")).await
                        {
                            send_failed.store(true, AtomicOrdering::SeqCst);
                        }
                        Ok(json!({}))
                    }
                    // Waits for cancellation; returns Ok on purpose so the
                    // test proves the server maps a cancelled token onto a
                    // `cancelled` error regardless of the returned value.
                    "slowProv" => {
                        started.notify_one();
                        cancel.cancelled().await;
                        cancelled.store(true, AtomicOrdering::SeqCst);
                        Ok(json!({}))
                    }
                    // One oversize event: fails encode pre-validation, so
                    // `send` returns false and the terminal must be
                    // `invalid_payload` even though the handler returns Ok.
                    "hugeProv" => {
                        if events.send(demo_event(&"x".repeat(9 * 1024 * 1024))).await {
                            return Err(ExtensionFault::extension_error(
                                "oversize event must not send",
                            ));
                        }
                        send_failed.store(true, AtomicOrdering::SeqCst);
                        Ok(json!({}))
                    }
                    // Floods events until the bounded pipeline saturates,
                    // then waits for cancellation (saturation/cancel test).
                    "floodProv" => {
                        started.notify_one();
                        while events.send(demo_event("tick")).await {
                            ticks.fetch_add(1, AtomicOrdering::SeqCst);
                            if cancel.is_cancelled() {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        cancelled.store(true, AtomicOrdering::SeqCst);
                        Ok(json!({}))
                    }
                    other => Err(ExtensionFault::not_found(format!(
                        "provider not found: {other}"
                    ))),
                }
            })
        }

        fn set_flags(
            &self,
            _context: Arc<NativeExtensionContext>,
            values: BTreeMap<String, FlagValueWire>,
        ) -> NativeFuture<Result<bool, ExtensionFault>> {
            let flags = Arc::clone(&self.handles.flags);
            Box::pin(async move {
                flags
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(values);
                Ok(true)
            })
        }

        fn execute_shortcut(
            &self,
            _context: Arc<NativeExtensionContext>,
            key: String,
        ) -> NativeFuture<Result<bool, ExtensionFault>> {
            let shortcuts = Arc::clone(&self.handles.shortcuts);
            Box::pin(async move {
                let owned = key == "ctrl+alt+d";
                record(&shortcuts, key);
                Ok(owned)
            })
        }

        fn handle_terminal_input(
            &self,
            _context: Arc<NativeExtensionContext>,
            data: String,
        ) -> NativeFuture<Result<TerminalInputResult, ExtensionFault>> {
            let seen = Arc::clone(&self.handles.terminal_inputs);
            let started = Arc::clone(&self.handles.terminal_started);
            let stopped = Arc::clone(&self.handles.terminal_stopped);
            Box::pin(async move {
                if data == "stall" {
                    let _drop_flag = DropFlag(stopped);
                    started.notify_one();
                    std::future::pending::<()>().await;
                }
                record(&seen, data.clone());
                Ok(TerminalInputResult {
                    consume: false,
                    data: Some(format!("{data}|rewritten")),
                })
            })
        }

        fn render_tool_html(
            &self,
            _context: Arc<NativeExtensionContext>,
            tool_name: String,
            phase: String,
            payload: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let renders = Arc::clone(&self.handles.renders);
            Box::pin(async move {
                let _ = payload;
                if tool_name == "stall" {
                    std::future::pending::<()>().await;
                }
                record(&renders, (tool_name.clone(), phase.clone()));
                Ok(json!({ "html": format!("<b>{tool_name}:{phase}</b>") }))
            })
        }

        fn on_lifecycle(
            &self,
            _context: Arc<NativeExtensionContext>,
            event_type: String,
            payload: Value,
            events: NativeEventSink,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let seen = Arc::clone(&self.handles.lifecycle);
            Box::pin(async move {
                // A hook that never resolves: exercises the server-side deadline.
                if event_type == "stall" {
                    std::future::pending::<()>().await;
                }
                record(&seen, (event_type.clone(), payload));
                let _ = events
                    .send(
                        "uiSlot",
                        json!({
                            "key": "demo",
                            "generation": 1,
                            "placement": "aboveEditor",
                            "height": 1,
                            "runs": [[{ "text": event_type, "style": {} }]],
                        }),
                    )
                    .await;
                Ok(json!({ "seen": event_type }))
            })
        }

        fn on_theme_update(&self, update: crate::protocol::ThemeUpdate) {
            record(&self.handles.theme_updates, update);
        }
    }

    struct DetachedSinkExtension {
        release: CancellationToken,
    }

    impl NativeExtension for DetachedSinkExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            RegistrySnapshot {
                tools: vec![tool_entry("detached")],
                providers: vec![ProviderSnapshotEntry {
                    name: "detached".to_owned(),
                    stream_simple: true,
                    ..ProviderSnapshotEntry::default()
                }],
                ..RegistrySnapshot::default()
            }
        }

        fn prepare_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move {
                if name == "detached" {
                    Ok(args)
                } else {
                    Err(ExtensionFault::not_found(name))
                }
            })
        }

        fn validate_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move {
                if name == "detached" {
                    Ok(args)
                } else {
                    Err(ExtensionFault::not_found(name))
                }
            })
        }

        fn execute_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ToolCall,
            updates: ToolUpdateSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let release = self.release.clone();
            Box::pin(async move {
                if call.name != "detached" {
                    return Err(ExtensionFault::not_found(call.name));
                }
                if !updates.send(json!({"stage": "queued"})) {
                    return Err(ExtensionFault::extension_error(
                        "detached update was not accepted",
                    ));
                }
                tokio::spawn(async move {
                    release.cancelled().await;
                    drop(updates);
                });
                Ok(json!({"done": true}))
            })
        }

        fn stream_provider(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ProviderStreamCall,
            events: ProviderEventSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let release = self.release.clone();
            Box::pin(async move {
                if call.provider_id != "detached" {
                    return Err(ExtensionFault::not_found(call.provider_id));
                }
                tokio::spawn(async move {
                    release.cancelled().await;
                    drop(events);
                });
                Ok(json!({}))
            })
        }
    }

    /// Lifecycle callback panics: the server must catch the unwind and answer
    /// a correlated `internal` error instead of going silent.
    struct PanickingExtension;

    impl NativeExtension for PanickingExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            RegistrySnapshot {
                handlers: vec!["boom".to_owned()],
                ..RegistrySnapshot::default()
            }
        }

        fn prepare_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn validate_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn execute_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ToolCall,
            _updates: ToolUpdateSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(call.name)) })
        }

        fn on_lifecycle(
            &self,
            _context: Arc<NativeExtensionContext>,
            event_type: String,
            _payload: Value,
            _events: NativeEventSink,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move {
                panic!("lifecycle callback exploded: {event_type}");
            })
        }
        fn on_theme_update(&self, _update: crate::protocol::ThemeUpdate) {
            panic!("theme callback exploded");
        }
    }

    /// Lifecycle callback clones the event sink into detached work that
    /// outlives the callback and retains an outbound sender indefinitely.
    struct DetachedEventSinkExtension {
        release: CancellationToken,
    }

    impl NativeExtension for DetachedEventSinkExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            RegistrySnapshot {
                handlers: vec!["detach".to_owned()],
                ..RegistrySnapshot::default()
            }
        }

        fn prepare_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn validate_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn execute_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ToolCall,
            _updates: ToolUpdateSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(call.name)) })
        }

        fn on_lifecycle(
            &self,
            _context: Arc<NativeExtensionContext>,
            _event_type: String,
            _payload: Value,
            events: NativeEventSink,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let release = self.release.clone();
            Box::pin(async move {
                tokio::spawn(async move {
                    release.cancelled().await;
                    drop(events);
                });
                Ok(json!({ "detached": true }))
            })
        }
    }

    /// Extension that overrides nothing but the required methods; used to
    /// assert the honest defaults of every optional surface.
    struct DefaultExtension;

    impl NativeExtension for DefaultExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            RegistrySnapshot {
                handlers: vec!["session_start".to_owned()],
                ..RegistrySnapshot::default()
            }
        }

        fn prepare_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn validate_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            name: String,
            _args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(name)) })
        }

        fn execute_tool(
            &self,
            _context: Arc<NativeExtensionContext>,
            call: ToolCall,
            _updates: ToolUpdateSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            Box::pin(async move { Err(ExtensionFault::not_found(call.name)) })
        }
    }

    type ObservedContext = (String, Arc<NativeExtensionContext>);

    #[derive(Clone)]
    struct ContextProbeHandles {
        observed: Arc<Mutex<Vec<ObservedContext>>>,
        blocked_started: Arc<Notify>,
        blocked_release: Arc<Notify>,
    }

    /// Records the exact context Arc passed to every native callback.
    struct ContextProbeExtension {
        handles: ContextProbeHandles,
    }

    impl ContextProbeExtension {
        fn new() -> (Self, ContextProbeHandles) {
            let handles = ContextProbeHandles {
                observed: Arc::new(Mutex::new(Vec::new())),
                blocked_started: Arc::new(Notify::new()),
                blocked_release: Arc::new(Notify::new()),
            };
            (
                Self {
                    handles: handles.clone(),
                },
                handles,
            )
        }

        fn observe(&self, callback: impl Into<String>, context: Arc<NativeExtensionContext>) {
            record(&self.handles.observed, (callback.into(), context));
        }
    }

    impl NativeExtension for ContextProbeExtension {
        fn snapshot(&self) -> RegistrySnapshot {
            RegistrySnapshot {
                handlers: vec!["context.lifecycle".to_owned()],
                ..RegistrySnapshot::default()
            }
        }

        fn prepare_tool(
            &self,
            context: Arc<NativeExtensionContext>,
            _name: String,
            args: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            self.observe("prepare_tool", context);
            Box::pin(async move { Ok(args) })
        }

        fn validate_tool(
            &self,
            context: Arc<NativeExtensionContext>,
            _name: String,
            args: Value,
            _tool_call_id: Option<String>,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            self.observe("validate_tool", context);
            Box::pin(async move { Ok(args) })
        }

        fn execute_tool(
            &self,
            context: Arc<NativeExtensionContext>,
            call: ToolCall,
            _updates: ToolUpdateSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let name = call.name;
            let blocks = name == "block";
            self.observe(format!("execute_tool:{name}"), context);
            let started = Arc::clone(&self.handles.blocked_started);
            let release = Arc::clone(&self.handles.blocked_release);
            Box::pin(async move {
                if blocks {
                    started.notify_one();
                    release.notified().await;
                }
                Ok(json!({}))
            })
        }

        fn execute_command(
            &self,
            context: Arc<NativeExtensionContext>,
            _command: String,
            _args: String,
        ) -> NativeFuture<Result<(), ExtensionFault>> {
            self.observe("execute_command", context);
            Box::pin(async { Ok(()) })
        }

        fn stream_provider(
            &self,
            context: Arc<NativeExtensionContext>,
            _call: ProviderStreamCall,
            _events: ProviderEventSink,
            _cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            self.observe("stream_provider", context);
            Box::pin(async { Ok(json!({})) })
        }

        fn set_flags(
            &self,
            context: Arc<NativeExtensionContext>,
            _values: BTreeMap<String, FlagValueWire>,
        ) -> NativeFuture<Result<bool, ExtensionFault>> {
            self.observe("set_flags", context);
            Box::pin(async { Ok(true) })
        }

        fn execute_shortcut(
            &self,
            context: Arc<NativeExtensionContext>,
            _key: String,
        ) -> NativeFuture<Result<bool, ExtensionFault>> {
            self.observe("execute_shortcut", context);
            Box::pin(async { Ok(true) })
        }

        fn handle_terminal_input(
            &self,
            context: Arc<NativeExtensionContext>,
            _data: String,
        ) -> NativeFuture<Result<TerminalInputResult, ExtensionFault>> {
            self.observe("handle_terminal_input", context);
            Box::pin(async { Ok(TerminalInputResult::default()) })
        }

        fn render_tool_html(
            &self,
            context: Arc<NativeExtensionContext>,
            _tool_name: String,
            _phase: String,
            _payload: Value,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            self.observe("render_tool_html", context);
            Box::pin(async { Ok(json!({})) })
        }

        fn on_lifecycle(
            &self,
            context: Arc<NativeExtensionContext>,
            _event_type: String,
            _payload: Value,
            _events: NativeEventSink,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            self.observe("on_lifecycle", context);
            Box::pin(async { Ok(json!({})) })
        }
    }

    /// Builds the ordered `(id, method, payload)` probe sequence used to
    /// enumerate every native callback surface. Both the pre-load rejection
    /// pass and the post-load acceptance pass share this sequence; only the
    /// starting id differs.
    fn probe_callback_requests(start_id: FrameId) -> Vec<(FrameId, &'static str, Value)> {
        let mut id = start_id;
        let mut push = |method: &'static str, payload: Value| {
            let entry = (id, method, payload);
            id += 1;
            entry
        };
        vec![
            push(
                methods::TOOL_PREPARE,
                json!({ "name": "probe", "args": {} }),
            ),
            push(
                methods::TOOL_VALIDATE,
                json!({ "name": "probe", "args": {}, "toolCallId": "probe" }),
            ),
            push(
                methods::TOOL_EXECUTE,
                json!({ "name": "probe", "toolCallId": "probe", "args": {} }),
            ),
            push(
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "probe", "args": "" }),
            ),
            push(
                methods::PROVIDER_STREAM,
                json!({ "providerId": "probe", "model": {}, "context": {}, "options": {} }),
            ),
            push(FLAGS_SET_METHOD, json!({ "values": { "probe": true } })),
            push(SHORTCUT_EXECUTE_METHOD, json!({ "key": "ctrl+probe" })),
            push(Method::TerminalInput.as_str(), json!({ "data": "probe" })),
            push(
                TOOL_RENDER_HTML_METHOD,
                json!({ "toolName": "probe", "phase": "call", "payload": {} }),
            ),
            push("context.lifecycle", json!({})),
        ]
    }

    /// Before any load, every callback-bearing request must surface an
    /// `extension_not_loaded` error and must not invoke the extension.
    async fn assert_callbacks_rejected_before_load(
        client: &mut RawClient,
        handles: &ContextProbeHandles,
    ) -> R {
        for (id, method, payload) in probe_callback_requests(2) {
            let error = client.request(id, method, payload).await?;
            assert_eq!(error.kind, FrameKind::Error);
            assert_eq!(error.payload["code"], "extension_not_loaded");
        }
        assert!(
            handles
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "pre-load requests must not invoke extension callbacks"
        );
        Ok(())
    }

    /// After a valid load, every callback-bearing request must succeed and
    /// must have invoked the extension exactly once, all sharing one Arc
    /// snapshot. Returns that shared context.
    async fn assert_callbacks_observed_after_load(
        client: &mut RawClient,
        handles: &ContextProbeHandles,
        synthetic_cwd: &str,
        process_cwd: &std::path::Path,
    ) -> Result<Arc<NativeExtensionContext>, Box<dyn Error>> {
        for (id, method, payload) in probe_callback_requests(21) {
            let response = client.request(id, method, payload).await?;
            assert_eq!(response.kind, FrameKind::Res);
        }

        let observed = handles
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            observed
                .iter()
                .map(|(callback, _)| callback.as_str())
                .collect::<Vec<_>>(),
            vec![
                "prepare_tool",
                "validate_tool",
                "execute_tool:probe",
                "execute_command",
                "stream_provider",
                "set_flags",
                "execute_shortcut",
                "handle_terminal_input",
                "render_tool_html",
                "on_lifecycle",
            ]
        );
        let first_context = Arc::clone(&observed[0].1);
        for (_, context) in &observed {
            assert!(
                Arc::ptr_eq(&first_context, context),
                "all callbacks from one load must receive the same Arc snapshot"
            );
            assert_eq!(context.cwd, synthetic_cwd);
            assert!(context.project_trusted);
        }
        assert_eq!(
            std::env::current_dir()?,
            process_cwd,
            "native context must not mutate the process-global cwd"
        );
        Ok(first_context)
    }

    /// Each subsequent valid load (including ones carrying malformed
    /// `projectTrusted` shapes that must coerce to untrusted) must publish a
    /// fresh, distinct Arc snapshot. Returns the last published context.
    async fn assert_untrusted_reloads_replace_snapshot(
        client: &mut RawClient,
        handles: &ContextProbeHandles,
        synthetic_cwd: &str,
        mut last_context: Arc<NativeExtensionContext>,
    ) -> Result<Arc<NativeExtensionContext>, Box<dyn Error>> {
        for (index, project_trusted) in [
            None,
            Some(Value::Null),
            Some(Value::Bool(false)),
            Some(Value::String("true".to_owned())),
        ]
        .into_iter()
        .enumerate()
        {
            let expected_cwd = format!("{synthetic_cwd}/untrusted-{index}");
            let mut payload = json!({ "cwd": expected_cwd.clone() });
            if let Some(project_trusted) = project_trusted {
                payload
                    .as_object_mut()
                    .ok_or("load payload is an object")?
                    .insert("projectTrusted".to_owned(), project_trusted);
            }
            let load_id = 40 + index as u64 * 2;
            let reload = client
                .request(load_id, EXTENSIONS_LOAD_METHOD, payload)
                .await?;
            assert_eq!(reload.kind, FrameKind::Res);
            let response = client
                .request(
                    load_id + 1,
                    methods::TOOL_PREPARE,
                    json!({ "name": "probe", "args": {} }),
                )
                .await?;
            assert_eq!(response.kind, FrameKind::Res);
            let context = {
                let observed = handles
                    .observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Arc::clone(&observed.last().ok_or("reload callback observed")?.1)
            };
            assert_eq!(context.cwd, expected_cwd);
            assert!(!context.project_trusted);
            assert!(
                !Arc::ptr_eq(&last_context, &context),
                "each valid load must replace the published Arc snapshot"
            );
            last_context = context;
        }
        Ok(last_context)
    }

    /// A malformed load must error and must not replace the previously
    /// published Arc snapshot, which stays untrusted.
    async fn assert_malformed_load_preserves_snapshot(
        client: &mut RawClient,
        handles: &ContextProbeHandles,
        last_context: &Arc<NativeExtensionContext>,
    ) -> R {
        let malformed = client
            .request(
                50,
                EXTENSIONS_LOAD_METHOD,
                json!({ "cwd": 7, "projectTrusted": true }),
            )
            .await?;
        assert_eq!(malformed.kind, FrameKind::Error);
        assert_eq!(malformed.payload["code"], "invalid_request");
        let response = client
            .request(
                51,
                methods::TOOL_PREPARE,
                json!({ "name": "probe", "args": {} }),
            )
            .await?;
        assert_eq!(response.kind, FrameKind::Res);
        let preserved_context = {
            let observed = handles
                .observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(&observed.last().ok_or("preserved callback observed")?.1)
        };
        assert!(Arc::ptr_eq(last_context, &preserved_context));
        assert!(!preserved_context.project_trusted);
        Ok(())
    }

    #[tokio::test]
    async fn native_context_contract_gates_callbacks_and_preserves_valid_snapshots() -> R {
        let process_cwd = std::env::current_dir()?;
        let synthetic_cwd = format!("{}-native-context-contract", process_cwd.display());
        assert_ne!(synthetic_cwd, process_cwd.to_string_lossy());

        let (extension, handles) = ContextProbeExtension::new();
        let (mut client, server) = spawn_raw(extension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;

        assert_callbacks_rejected_before_load(&mut client, &handles).await?;

        let loaded = client
            .request(
                20,
                EXTENSIONS_LOAD_METHOD,
                json!({
                    "extensionPaths": [],
                    "cwd": synthetic_cwd,
                    "projectTrusted": true,
                }),
            )
            .await?;
        assert_eq!(loaded.kind, FrameKind::Res);

        let first_context = assert_callbacks_observed_after_load(
            &mut client,
            &handles,
            &synthetic_cwd,
            &process_cwd,
        )
        .await?;

        let last_context = assert_untrusted_reloads_replace_snapshot(
            &mut client,
            &handles,
            &synthetic_cwd,
            first_context,
        )
        .await?;

        assert_malformed_load_preserves_snapshot(&mut client, &handles, &last_context).await?;

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn native_context_snapshot_survives_concurrent_replacement() -> R {
        let (extension, handles) = ContextProbeExtension::new();
        let (mut client, server) = spawn_raw(extension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        let old = client
            .request(
                2,
                EXTENSIONS_LOAD_METHOD,
                json!({ "cwd": "/context-before-reload", "projectTrusted": true }),
            )
            .await?;
        assert_eq!(old.kind, FrameKind::Res);

        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "block", "toolCallId": "block", "args": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.blocked_started.notified()).await?;

        let replacement = client
            .request(
                4,
                EXTENSIONS_LOAD_METHOD,
                json!({ "cwd": "/context-after-reload", "projectTrusted": false }),
            )
            .await?;
        assert_eq!(replacement.kind, FrameKind::Res);
        handles.blocked_release.notify_one();

        let blocked_terminal = client.recv().await?;
        assert_eq!(blocked_terminal.id, 3);
        assert_eq!(blocked_terminal.kind, FrameKind::Res);
        let fresh = client
            .request(
                5,
                methods::TOOL_EXECUTE,
                json!({ "name": "fresh", "toolCallId": "fresh", "args": {} }),
            )
            .await?;
        assert_eq!(fresh.kind, FrameKind::Res);

        let observed = handles
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let blocked_context = observed
            .iter()
            .find(|(callback, _)| callback == "execute_tool:block")
            .map(|(_, context)| Arc::clone(context))
            .ok_or("blocked callback was not observed")?;
        let fresh_context = observed
            .iter()
            .find(|(callback, _)| callback == "execute_tool:fresh")
            .map(|(_, context)| Arc::clone(context))
            .ok_or("fresh callback was not observed")?;
        assert_eq!(blocked_context.cwd, "/context-before-reload");
        assert!(blocked_context.project_trusted);
        assert_eq!(fresh_context.cwd, "/context-after-reload");
        assert!(!fresh_context.project_trusted);
        assert!(
            !Arc::ptr_eq(&blocked_context, &fresh_context),
            "a replacement load must not mutate an in-flight callback snapshot"
        );

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn detached_stream_sinks_do_not_hold_requests_open() -> R {
        let release = CancellationToken::new();
        let extension = DetachedSinkExtension {
            release: release.clone(),
        };
        let config = ServerConfig {
            max_in_flight: 1,
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(extension, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({
                    "name": "detached",
                    "toolCallId": "detached-tool",
                    "args": {},
                    "prepared": true,
                }),
            })
            .await?;
        let update = client.recv().await?;
        assert_eq!(update.id, 2);
        assert_eq!(update.kind, FrameKind::Event);
        assert_eq!(update.method, Method::ToolUpdate.as_str());
        assert_eq!(update.payload["partialResult"]["stage"], "queued");
        let tool_terminal = client.recv().await?;
        assert_eq!(tool_terminal.id, 2);
        assert_eq!(tool_terminal.kind, FrameKind::Res);

        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: methods::PROVIDER_STREAM.to_owned(),
                payload: json!({
                    "providerId": "detached",
                    "model": {},
                    "context": {},
                    "options": {},
                }),
            })
            .await?;
        let provider_terminal = client.recv().await?;
        assert_eq!(provider_terminal.id, 3);
        assert_eq!(provider_terminal.kind, FrameKind::Res);

        release.cancel();
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Handshake
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn hello_answers_with_compiled_constants_and_ignores_compatibility() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client
            .hello(PROTOCOL_VERSION, "0.0.0-not-typescript")
            .await?;
        let ack = client.recv().await?;
        assert_eq!(ack.id, 1);
        assert_eq!(ack.kind, FrameKind::Res);
        assert_eq!(ack.method, Method::Hello.as_str());
        assert_eq!(ack.payload["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            ack.payload["compatibilityVersion"], COMPATIBILITY_VERSION,
            "server must answer with compiled constants"
        );
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok(), "clean EOF must end serve_io: {result:?}");
        Ok(())
    }

    #[tokio::test]
    async fn eof_rejects_a_truncated_frame() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (client, server) = spawn_raw(ext, ServerConfig::default());
        let RawClient {
            mut write,
            read: _read,
        } = client;
        write.write_all(br#"{"id":1,"kind":"req""#).await?;
        write.shutdown().await?;

        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(
            matches!(&result, Err(ServerError::Protocol(message)) if message.contains("truncated")),
            "expected truncated frame failure, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn hello_rejects_protocol_mismatch() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(99, COMPATIBILITY_VERSION).await?;
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(
            matches!(result, Err(ServerError::Handshake(ref message)) if message.contains("protocol version mismatch")),
            "expected protocol mismatch handshake failure, got {result:?}"
        );
        Ok(())
    }

    /// Bad hello while the parent keeps stdin open must not leave a detached
    /// stdin pump wedging current-thread runtime shutdown.
    #[cfg(unix)]
    #[test]
    fn serve_exits_promptly_on_bad_hello_while_stdin_stays_open() -> R {
        use std::sync::mpsc;

        let (stdin_rx, mut stdin_tx) = std::io::pipe()?;
        let (_stdout_keep, stdout_tx) = std::io::pipe()?;
        let (done_tx, done_rx) = mpsc::channel();

        let worker = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = done_tx.send(Err(ServerError::Io(error)));
                    return;
                }
            };
            let (ext, _) = DemoExtension::new();
            let result = runtime.block_on(serve_stdio(
                stdin_rx,
                stdout_tx,
                ext,
                ServerConfig::default(),
            ));
            // Runtime drop waits on any leftover spawn_blocking work — the
            // regression target when the stdin pump is detached instead of joined.
            drop(runtime);
            let _ = done_tx.send(result);
        });

        let bad_hello = encode_frame(&Frame {
            id: 1,
            kind: FrameKind::Req,
            method: Method::Hello.as_str().to_owned(),
            payload: json!({
                "protocolVersion": 99,
                "compatibilityVersion": COMPATIBILITY_VERSION,
            }),
        })?;
        std::io::Write::write_all(&mut stdin_tx, &bad_hello)?;
        std::io::Write::flush(&mut stdin_tx)?;
        // Keep stdin_tx open across the wait — EOF must not be what unblocks us.

        let Ok(result) = done_rx.recv_timeout(TIMEOUT) else {
            // Unblock any stuck read and join before failing so a hung
            // current-thread runtime cannot leave the suite wedged.
            drop(stdin_tx);
            let _ = worker.join();
            return Err(
                "serve + current-thread runtime shutdown must finish while stdin stays open".into(),
            );
        };
        assert!(
            matches!(
                result,
                Err(ServerError::Handshake(ref message))
                    if message.contains("protocol version mismatch")
            ),
            "expected handshake failure, got {result:?}"
        );
        drop(stdin_tx);
        worker.join().map_err(|_| "serve worker thread panicked")?;
        Ok(())
    }

    #[tokio::test]
    async fn hello_rejects_non_hello_first_frame() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client
            .send(&Frame {
                id: 1,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({}),
            })
            .await?;
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(
            matches!(result, Err(ServerError::Handshake(_))),
            "expected handshake failure, got {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn callbacks_fail_closed_before_extensions_load() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;

        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_PREPARE.to_owned(),
                payload: json!({ "name": "echo", "args": {} }),
            })
            .await?;
        let error = client.recv().await?;
        assert_eq!(error.kind, FrameKind::Error);
        assert_eq!(error.payload["code"], "extension_not_loaded");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Unknown methods fail closed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unknown_method_fails_closed_without_killing_unrelated_requests() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: "totally.bogus".to_owned(),
                payload: json!({}),
            })
            .await?;
        let error = client.recv().await?;
        assert_eq!(error.id, 2);
        assert_eq!(error.kind, FrameKind::Error);
        assert_eq!(error.payload["code"], "unknown_method");

        // An unrelated valid request is still served afterwards.
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "extensionPaths": [], "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 3);
        assert_eq!(load.kind, FrameKind::Res);
        assert_eq!(load.payload["tools"][0]["name"], "echo");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Bounded in-flight requests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn in_flight_requests_are_bounded() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        // Occupy the single in-flight slot with a blocking execution.
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "block", "toolCallId": "t1", "args": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;

        // The second request is rejected immediately; the read loop keeps
        // going instead of stalling behind the blocked execution.
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "block", "toolCallId": "t2", "args": {} }),
            })
            .await?;
        let rejected = client.recv().await?;
        assert_eq!(rejected.id, 3);
        assert_eq!(rejected.kind, FrameKind::Error);
        assert_eq!(rejected.payload["code"], "overloaded");

        // Releasing the first execution lets it complete normally.
        handles.release.notify_one();
        let terminal = client.recv().await?;
        assert_eq!(terminal.id, 2);
        assert_eq!(terminal.kind, FrameKind::Res);
        assert_eq!(terminal.payload["done"], true);

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Full duplex integration over the real HostClient
    // -----------------------------------------------------------------------

    /// Live duplex fixture: a real `HostClient` handshaken against a native
    /// server running the scripted extension.
    struct DuplexFixture {
        client: HostClient,
        server: ServerHandle,
        handles: DemoHandles,
    }

    impl DuplexFixture {
        /// Connect a `HostClient` to a fresh `DemoExtension`, complete the
        /// protocol-only handshake, and establish a default test context.
        async fn connect() -> Result<Self, Box<dyn Error>> {
            let (ext, handles) = DemoExtension::new();
            let (client_to_server, server_rx) = tokio::io::duplex(64 * 1024);
            let (server_tx, client_from_server) = tokio::io::duplex(64 * 1024);
            let (client_err, _server_err) = tokio::io::duplex(4096);
            let server = tokio::spawn(async move {
                serve_io(server_rx, server_tx, ext, ServerConfig::default()).await
            });
            let client = HostClient::connect_boxed(
                Box::new(client_to_server),
                Box::new(client_from_server),
                Box::new(client_err),
                None,
            );
            client
                .handshake_with_policy(HandshakePolicy::ProtocolOnly)
                .await?;
            client
                .request_raw(
                    EXTENSIONS_LOAD_METHOD,
                    json!({
                        "extensionPaths": [],
                        "cwd": "/duplex-fixture",
                        "projectTrusted": false,
                    }),
                    TIMEOUT,
                )
                .await?;
            Ok(Self {
                client,
                server,
                handles,
            })
        }

        /// Clean shutdown: client EOF must end `serve_io` without error.
        async fn finish(self) -> R {
            self.client.shutdown().await?;
            let result = tokio::time::timeout(TIMEOUT, self.server).await??;
            assert!(result.is_ok(), "EOF shutdown must be clean: {result:?}");
            Ok(())
        }
    }

    #[tokio::test]
    async fn duplex_extensions_load_returns_registry_snapshot() -> R {
        let fixture = DuplexFixture::connect().await?;
        let load = fixture
            .client
            .request_raw(
                EXTENSIONS_LOAD_METHOD,
                json!({ "extensionPaths": [], "cwd": "/tmp", "projectTrusted": true }),
                TIMEOUT,
            )
            .await?;
        assert_eq!(load.payload["tools"][0]["name"], "echo");
        assert_eq!(load.payload["tools"][0]["label"], "echo label");
        assert_eq!(load.payload["commands"][0]["name"], "demo");
        assert_eq!(
            load.payload["providers"][0]["extensionPath"],
            "native://demo"
        );
        assert_eq!(load.payload["handlers"][0], "session_start");
        assert_eq!(load.payload["terminalInput"], true);
        assert_eq!(load.payload["extensions"], 1);
        assert_eq!(load.payload["errors"], json!([]));
        fixture.finish().await
    }

    #[tokio::test]
    async fn duplex_prepare_and_validate_round_trip() -> R {
        let fixture = DuplexFixture::connect().await?;

        // tool.prepare / tool.validate are real RPCs on the native endpoint.
        let prepared = fixture
            .client
            .request_raw(
                methods::TOOL_PREPARE,
                json!({ "name": "echo", "args": { "text": "hi" } }),
                TIMEOUT,
            )
            .await?;
        assert_eq!(prepared.payload["args"]["prepared"], true);
        assert_eq!(prepared.payload["args"]["text"], "hi");
        let validated = fixture
            .client
            .request_raw(
                methods::TOOL_VALIDATE,
                json!({ "name": "echo", "args": prepared.payload["args"].clone() }),
                TIMEOUT,
            )
            .await?;
        assert_eq!(validated.payload["args"]["validated"], true);

        // Unknown tool names surface a not_found error frame.
        let missing = fixture
            .client
            .request_raw(
                methods::TOOL_PREPARE,
                json!({ "name": "nope", "args": {} }),
                TIMEOUT,
            )
            .await;
        assert!(
            matches!(missing, Err(HostClientError::Remote { ref code, .. }) if code == "not_found"),
            "expected not_found remote error, got {missing:?}"
        );
        fixture.finish().await
    }

    #[tokio::test]
    async fn duplex_load_context_is_required_and_replaced_for_tool_calls() -> R {
        let fixture = DuplexFixture::connect().await?;

        for payload in [json!({}), json!({ "cwd": 7 })] {
            let result = fixture
                .client
                .request_raw(EXTENSIONS_LOAD_METHOD, payload, TIMEOUT)
                .await;
            assert!(
                matches!(result, Err(HostClientError::Remote { ref code, .. }) if code == "invalid_request"),
                "expected invalid_request remote error, got {result:?}"
            );
        }

        for cwd in ["/first", "/second"] {
            fixture
                .client
                .request_raw(EXTENSIONS_LOAD_METHOD, json!({ "cwd": cwd }), TIMEOUT)
                .await?;
            fixture
                .client
                .request_raw(
                    methods::TOOL_EXECUTE,
                    json!({ "name": "echo", "toolCallId": cwd, "args": {} }),
                    TIMEOUT,
                )
                .await?;
        }

        {
            let contexts = fixture
                .handles
                .tool_contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                contexts
                    .iter()
                    .map(|context| context.cwd.as_str())
                    .collect::<Vec<_>>(),
                ["/first", "/second"]
            );
            assert!(contexts.iter().all(|context| !context.project_trusted));
        }
        fixture.finish().await
    }

    #[tokio::test]
    async fn duplex_execute_streams_updates_before_terminal() -> R {
        let fixture = DuplexFixture::connect().await?;

        // tool.execute streams toolUpdate events before the terminal result.
        let mut stream = fixture
            .client
            .open_stream_raw(
                methods::TOOL_EXECUTE,
                json!({
                    "name": "echo",
                    "toolCallId": "call-1",
                    "args": { "text": "hi" },
                    "prepared": true,
                }),
                8,
            )
            .await?;
        let first = tokio::time::timeout(TIMEOUT, stream.next_event())
            .await?
            .ok_or("stream closed before first update")?;
        assert_eq!(first.method, Method::ToolUpdate.as_str());
        assert_eq!(first.payload["toolCallId"], "call-1");
        assert_eq!(first.payload["toolName"], "echo");
        assert_eq!(first.payload["partialResult"]["stage"], "half");
        let second = tokio::time::timeout(TIMEOUT, stream.next_event())
            .await?
            .ok_or("stream closed before second update")?;
        assert_eq!(second.payload["partialResult"]["stage"], "almost");
        let drained = tokio::time::timeout(TIMEOUT, stream.next_event()).await?;
        assert!(drained.is_none(), "expected clean EOS, got {drained:?}");
        let terminal = stream.finish(TIMEOUT).await?;
        assert_eq!(terminal.kind, FrameKind::Res);
        assert_eq!(terminal.payload["content"][0]["text"], "done");
        assert_eq!(terminal.payload["isError"], false);
        fixture.finish().await
    }

    #[tokio::test]
    async fn cancel_token_for_matching_kind_is_cancelled_after_terminal() -> R {
        let fixture = DuplexFixture::connect().await?;

        // A running tool is cancelled by a tool.cancel control event read
        // while the execution is active.
        let mut slow = fixture
            .client
            .open_stream_raw(
                methods::TOOL_EXECUTE,
                json!({ "name": "slow", "toolCallId": "call-2", "args": {}, "prepared": true }),
                8,
            )
            .await?;
        tokio::time::timeout(TIMEOUT, fixture.handles.started.notified()).await?;
        let token = fixture
            .handles
            .tool_cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or("extension did not retain the tool cancellation token")?;
        slow.cancel(methods::TOOL_CANCEL)?;
        let cancelled_terminal = slow.finish(TIMEOUT).await;
        assert!(
            matches!(cancelled_terminal, Err(HostClientError::Remote { ref code, .. }) if code == "cancelled"),
            "expected cancelled remote error, got {cancelled_terminal:?}"
        );
        assert!(
            token.is_cancelled(),
            "the retained tool cancellation token must be cancelled after its terminal"
        );
        fixture.finish().await
    }

    #[tokio::test]
    async fn cancel_events_only_cancel_matching_in_flight_kinds() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({
                    "name": "slow",
                    "toolCallId": "kind-isolation",
                    "args": {},
                }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;

        let token = handles
            .tool_cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or("extension did not retain the tool cancellation token")?;
        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::PROVIDER_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        // A correlated sibling terminal is an in-order fence: the read loop
        // has already dispatched the preceding provider.cancel by this point.
        let marker = client
            .request(
                3,
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "demo", "args": "kind-isolation-fence" }),
            )
            .await?;
        assert_eq!(marker.id, 3);
        assert_eq!(marker.kind, FrameKind::Res);
        assert_eq!(marker.payload, json!({ "ok": true }));
        assert!(
            !token.is_cancelled(),
            "provider.cancel must not cancel tool work"
        );

        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::TOOL_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        let terminal = client.recv().await?;
        assert_eq!(terminal.id, 2);
        assert_eq!(terminal.kind, FrameKind::Error);
        assert_eq!(terminal.payload["code"], "cancelled");
        assert!(
            token.is_cancelled(),
            "the retained tool token must be cancelled after its terminal"
        );

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn duplex_command_execute_and_unknown_command() -> R {
        let fixture = DuplexFixture::connect().await?;

        // command.execute works; unknown commands fail closed.
        fixture
            .client
            .request_raw(
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "demo", "args": "--flag value" }),
                TIMEOUT,
            )
            .await?;
        {
            let seen = fixture.handles.commands.lock().map_err(|e| e.to_string())?;
            assert_eq!(
                seen.as_slice(),
                &[("demo".to_owned(), "--flag value".to_owned())]
            );
        }
        let missing_command = fixture
            .client
            .request_raw(
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "nope", "args": "" }),
                TIMEOUT,
            )
            .await;
        assert!(
            matches!(missing_command, Err(HostClientError::Remote { ref code, .. }) if code == "not_found"),
            "expected not_found remote error, got {missing_command:?}"
        );
        fixture.finish().await
    }

    // -------------------------------------------------------------------
    // Reviewer regressions: encode containment and nonblocking overload
    // -------------------------------------------------------------------

    /// Poll a flag with a deadline.
    async fn wait_flag(flag: &std::sync::atomic::AtomicBool) -> R {
        tokio::time::timeout(TIMEOUT, async {
            while !flag.load(AtomicOrdering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
    }

    /// P1: an unencodable tool result (scalar or oversize) must be contained
    /// as a correlated `invalid_payload` error; sibling requests and the
    /// connection itself survive.
    #[tokio::test]
    async fn invalid_tool_results_are_contained_per_frame() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        for (id, name) in [(2u64, "scalar"), (3, "echo"), (4, "huge")] {
            client
                .send(&Frame {
                    id,
                    kind: FrameKind::Req,
                    method: methods::TOOL_EXECUTE.to_owned(),
                    payload: json!({ "name": name, "toolCallId": format!("t{id}"), "args": {} }),
                })
                .await?;
        }

        // Collect terminal frames; skip streaming toolUpdate events.
        let mut terminals = std::collections::HashMap::new();
        tokio::time::timeout(TIMEOUT, async {
            while terminals.len() < 3 {
                let frame = client.recv().await?;
                if matches!(frame.kind, FrameKind::Res | FrameKind::Error) {
                    terminals.insert(frame.id, frame);
                }
            }
            Ok::<(), Box<dyn Error>>(())
        })
        .await??;

        let scalar = &terminals[&2];
        assert_eq!(scalar.kind, FrameKind::Error);
        assert_eq!(scalar.method, methods::TOOL_EXECUTE);
        assert_eq!(scalar.payload["code"], "invalid_payload");
        let echo = &terminals[&3];
        assert_eq!(echo.kind, FrameKind::Res);
        assert_eq!(echo.payload["content"][0]["text"], "done");
        let huge = &terminals[&4];
        assert_eq!(huge.kind, FrameKind::Error);
        assert_eq!(huge.payload["code"], "invalid_payload");

        // The connection stays alive for subsequent requests.
        client
            .send(&Frame {
                id: 5,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 5);
        assert_eq!(load.kind, FrameKind::Res);
        assert_eq!(load.payload["tools"][0]["name"], "echo");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// P2: with the bounded outbound channel saturated, the overload
    /// rejection must not stall the read loop — a `tool.cancel` sent right
    /// after an overloaded request is still observed by the running tool.
    #[tokio::test]
    async fn overload_rejection_never_stalls_cancel() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            update_capacity: 4,
            outbound_capacity: 4,
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        // Occupy the single in-flight slot and saturate the whole bounded
        // pipeline (per-call update channel -> outbound -> duplex).
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "flood", "toolCallId": "t1", "args": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;
        wait_flag(&handles.saturated).await?;

        // The overloaded request must get a correlated rejection without
        // blocking the read loop...
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "echo", "toolCallId": "t2", "args": {} }),
            })
            .await?;
        // ...so this cancel is still read and delivered to the running tool.
        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::TOOL_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        wait_flag(&handles.cancelled).await?;

        // Drain until both correlated terminals arrive. Their order is not
        // stable because the rejection and cancelled request share out_tx.
        tokio::time::timeout(TIMEOUT, async {
            let mut cancelled = false;
            let mut overloaded = false;
            while !cancelled || !overloaded {
                let frame = client.recv().await?;
                match (frame.id, frame.kind) {
                    (2, FrameKind::Error) => {
                        assert_eq!(frame.payload["code"], "cancelled");
                        cancelled = true;
                    }
                    (3, FrameKind::Error) => {
                        assert_eq!(frame.payload["code"], "overloaded");
                        overloaded = true;
                    }
                    _ => {}
                }
            }
            Ok::<(), Box<dyn Error>>(())
        })
        .await??;

        // Graceful shutdown: close the request half, then drain queued
        // updates until the server closes its writer.
        let RawClient { write, mut read } = client;
        drop(write);
        tokio::time::timeout(TIMEOUT, async {
            let mut line = String::new();
            loop {
                line.clear();
                match read.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await?;
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Cached snapshot
    // -----------------------------------------------------------------------

    /// The registry snapshot is captured once at startup; repeated loads
    /// serve the cached copy.
    #[tokio::test]
    async fn load_snapshot_is_cached_once() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        for id in 2u64..=3 {
            client
                .send(&Frame {
                    id,
                    kind: FrameKind::Req,
                    method: EXTENSIONS_LOAD_METHOD.to_owned(),
                    payload: json!({ "cwd": "/tmp" }),
                })
                .await?;
            let load = client.recv().await?;
            assert_eq!(load.id, id);
            assert_eq!(load.kind, FrameKind::Res);
            assert_eq!(load.payload["tools"][0]["name"], "echo");
        }
        assert_eq!(
            handles.snapshot_calls.load(AtomicOrdering::SeqCst),
            1,
            "snapshot must be captured once at startup"
        );
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Advertised surfaces route
    // -----------------------------------------------------------------------

    /// An advertised lifecycle handler is dispatched and may emit
    /// unsolicited id-0 events through the native event sink.
    #[tokio::test]
    async fn lifecycle_routes_advertised_handler_with_native_events() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        let payload = json!({ "type": "session_start", "cwd": "/tmp" });
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: "session_start".to_owned(),
                payload: payload.clone(),
            })
            .await?;
        // The hook's uiSlot event precedes the correlated response.
        let event = client.recv().await?;
        assert_eq!(event.id, 0);
        assert_eq!(event.kind, FrameKind::Event);
        assert_eq!(event.method, "uiSlot");
        assert_eq!(event.payload["runs"][0][0]["text"], "session_start");
        let res = client.recv().await?;
        assert_eq!(res.id, 2);
        assert_eq!(res.kind, FrameKind::Res);
        assert_eq!(res.method, "session_start");
        assert_eq!(res.payload, json!({ "seen": "session_start" }));
        {
            let seen = handles
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.as_slice(), &[("session_start".to_owned(), payload)]);
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// A compact `message_update_delta` request maps onto the advertised
    /// `message_update` handler key.
    #[tokio::test]
    async fn lifecycle_message_update_delta_maps_to_advertised_key() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: MESSAGE_UPDATE_DELTA_METHOD.to_owned(),
                payload: json!({
                    "type": MESSAGE_UPDATE_DELTA_METHOD,
                    "event": { "type": "text_delta", "delta": "hi" },
                }),
            })
            .await?;
        let event = client.recv().await?;
        assert_eq!(event.id, 0);
        assert_eq!(event.payload["runs"][0][0]["text"], "message_update");
        let res = client.recv().await?;
        assert_eq!(res.id, 2);
        assert_eq!(res.kind, FrameKind::Res);
        // Correlation keeps the request method; the hook saw the advertised
        // key.
        assert_eq!(res.method, MESSAGE_UPDATE_DELTA_METHOD);
        assert_eq!(res.payload, json!({ "seen": "message_update" }));
        {
            let seen = handles
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, "message_update");
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Lifecycle requests for event types the snapshot does not advertise
    /// fail closed without affecting sibling requests.
    #[tokio::test]
    async fn lifecycle_unadvertised_method_fails_closed() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: "session_end".to_owned(),
                payload: json!({}),
            })
            .await?;
        let error = client.recv().await?;
        assert_eq!(error.id, 2);
        assert_eq!(error.kind, FrameKind::Error);
        assert_eq!(error.payload["code"], "unknown_method");
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 3);
        assert_eq!(load.kind, FrameKind::Res);
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// `flags.set` routes the validated overlay to the extension and
    /// reports its verdict.
    #[tokio::test]
    async fn flags_set_routes_and_applies() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: FLAGS_SET_METHOD.to_owned(),
                payload: json!({ "values": { "verbose": true, "profile": "fast" } }),
            })
            .await?;
        let res = client.recv().await?;
        assert_eq!(res.id, 2);
        assert_eq!(res.kind, FrameKind::Res);
        assert_eq!(res.payload, json!({ "ok": true }));
        {
            let flags = handles
                .flags
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(flags.get("verbose"), Some(&FlagValueWire::Boolean(true)));
            assert_eq!(
                flags.get("profile"),
                Some(&FlagValueWire::String("fast".to_owned()))
            );
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// `shortcut.execute` reports whether the extension owned the key.
    #[tokio::test]
    async fn shortcut_execute_reports_handled() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        for (id, key, handled) in [(2u64, "ctrl+alt+d", true), (3u64, "ctrl+alt+z", false)] {
            client
                .send(&Frame {
                    id,
                    kind: FrameKind::Req,
                    method: SHORTCUT_EXECUTE_METHOD.to_owned(),
                    payload: json!({ "key": key }),
                })
                .await?;
            let res = client.recv().await?;
            assert_eq!(res.id, id);
            assert_eq!(res.kind, FrameKind::Res);
            assert_eq!(res.payload, json!({ "handled": handled }));
        }
        {
            let seen = handles
                .shortcuts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                seen.as_slice(),
                &["ctrl+alt+d".to_owned(), "ctrl+alt+z".to_owned()]
            );
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Malformed/missing/non-string `key` answers `handled: false` without
    /// invoking extension code, matching both TypeScript hosts; empty and
    /// non-empty strings still dispatch.
    #[tokio::test]
    async fn shortcut_execute_rejects_non_string_key() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        // Malformed values: each must yield `handled: false` and never reach
        // the extension callback.
        let malformed: &[(u64, Value)] = &[
            (2u64, json!({})),
            (3u64, json!({ "key": null })),
            (4u64, json!({ "key": 42 })),
            (5u64, json!({ "key": true })),
            (6u64, json!({ "key": ["ctrl+alt+d"] })),
            (7u64, json!({ "key": { "key": "ctrl+alt+d" } })),
        ];
        for (id, payload) in malformed {
            let res = client
                .request(*id, SHORTCUT_EXECUTE_METHOD, payload.clone())
                .await?;
            assert_eq!(res.id, *id, "id mismatch for payload {payload}");
            assert_eq!(
                res.kind,
                FrameKind::Res,
                "non-response for payload {payload}"
            );
            assert_eq!(
                res.payload,
                json!({ "handled": false }),
                "payload {payload}"
            );
        }
        {
            let seen = handles
                .shortcuts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                seen.is_empty(),
                "extension callback invoked for malformed key: {seen:?}"
            );
        }

        // Valid strings, including the empty string, still dispatch and
        // preserve the callback result.
        for (id, key, handled) in [(8u64, "", false), (9u64, "ctrl+alt+d", true)] {
            let res = client
                .request(id, SHORTCUT_EXECUTE_METHOD, json!({ "key": key }))
                .await?;
            assert_eq!(res.id, id);
            assert_eq!(res.kind, FrameKind::Res);
            assert_eq!(res.payload, json!({ "handled": handled }));
        }
        {
            let seen = handles
                .shortcuts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.as_slice(), &[String::new(), "ctrl+alt+d".to_owned()]);
        }

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// `terminalInput` routes the chunk and returns the rewrite.
    #[tokio::test]
    async fn terminal_input_rewrites_data() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: Method::TerminalInput.as_str().to_owned(),
                payload: json!({ "data": "ls" }),
            })
            .await?;
        let res = client.recv().await?;
        assert_eq!(res.id, 2);
        assert_eq!(res.kind, FrameKind::Res);
        assert_eq!(res.payload, json!({ "data": "ls|rewritten" }));
        {
            let seen = handles
                .terminal_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.as_slice(), &["ls".to_owned()]);
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn terminal_input_timeout_stops_callback_and_releases_permit() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let started_at = std::time::Instant::now();
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: Method::TerminalInput.as_str().to_owned(),
                payload: json!({ "data": "stall" }),
            })
            .await?;
        tokio::time::timeout(
            Duration::from_millis(100),
            handles.terminal_started.notified(),
        )
        .await?;
        let terminal = tokio::time::timeout(Duration::from_millis(100), client.recv()).await??;
        let elapsed = started_at.elapsed();
        assert!(
            elapsed >= NATIVE_TERMINAL_INPUT_BUDGET,
            "terminal callback timed out before its budget: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(100),
            "terminal callback exceeded its bounded timeout: {elapsed:?}"
        );
        assert_eq!(terminal.id, 2);
        assert_eq!(terminal.kind, FrameKind::Error);
        assert_eq!(terminal.payload["code"], "timeout");
        assert!(
            handles.terminal_stopped.load(AtomicOrdering::SeqCst),
            "timed-out callback future must be dropped"
        );

        tokio::task::yield_now().await;
        let sibling = client
            .request(
                3,
                EXTENSIONS_LOAD_METHOD,
                json!({ "cwd": "/after-timeout" }),
            )
            .await?;
        assert_eq!(sibling.kind, FrameKind::Res);

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// `tool.renderHtml` routes phase/tool/payload and returns the HTML.
    #[tokio::test]
    async fn tool_render_html_returns_html() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: TOOL_RENDER_HTML_METHOD.to_owned(),
                payload: json!({ "phase": "result", "toolName": "echo", "payload": { "x": 1 } }),
            })
            .await?;
        let res = client.recv().await?;
        assert_eq!(res.id, 2);
        assert_eq!(res.kind, FrameKind::Res);
        assert_eq!(res.payload, json!({ "html": "<b>echo:result</b>" }));
        {
            let seen = handles
                .renders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(seen.as_slice(), &[("echo".to_owned(), "result".to_owned())]);
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// `provider.stream` events are correlated to the request id and
    /// precede the terminal response.
    #[tokio::test]
    async fn provider_stream_events_precede_terminal() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::PROVIDER_STREAM.to_owned(),
                payload: json!({
                    "providerId": "demoProv",
                    "model": { "id": "m1" },
                    "context": { "messages": [] },
                    "options": {},
                }),
            })
            .await?;
        for delta in ["one", "two"] {
            let event = client.recv().await?;
            assert_eq!(event.id, 2);
            assert_eq!(event.kind, FrameKind::Event);
            assert_eq!(event.method, Method::ProviderEvent.as_str());
            assert_eq!(event.payload["type"], "text_delta");
            assert_eq!(event.payload["delta"], delta);
        }
        let terminal = client.recv().await?;
        assert_eq!(terminal.id, 2);
        assert_eq!(terminal.kind, FrameKind::Res);
        assert_eq!(terminal.payload, json!({}));
        assert!(!handles.provider_send_failed.load(AtomicOrdering::SeqCst));
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// A `provider.cancel` control event interrupts a running stream while
    /// an unrelated request still completes: the read loop never stalls
    /// behind the stream.
    #[tokio::test]
    async fn provider_cancel_interrupts_stream_while_sibling_completes() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::PROVIDER_STREAM.to_owned(),
                payload: json!({ "providerId": "slowProv", "model": {}, "context": {}, "options": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.provider_started.notified()).await?;
        let token = handles
            .provider_cancel_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or("extension did not retain the provider cancellation token")?;
        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::PROVIDER_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        // Sibling request dispatched while the provider stream is running.
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: COMMAND_EXECUTE_METHOD.to_owned(),
                payload: json!({ "command": "demo", "args": "x" }),
            })
            .await?;
        let mut cancelled_terminal = false;
        let mut command_terminal = false;
        tokio::time::timeout(TIMEOUT, async {
            while !(cancelled_terminal && command_terminal) {
                let frame = client.recv().await?;
                match (frame.id, frame.kind) {
                    (2, FrameKind::Error) => {
                        assert_eq!(frame.payload["code"], "cancelled");
                        cancelled_terminal = true;
                    }
                    (3, FrameKind::Res) => {
                        assert_eq!(frame.payload, json!({ "ok": true }));
                        command_terminal = true;
                    }
                    other => return Err(format!("unexpected frame: {other:?}").into()),
                }
            }
            Ok::<(), Box<dyn Error>>(())
        })
        .await??;
        assert!(
            token.is_cancelled(),
            "the retained provider token must be cancelled after its terminal"
        );
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// An oversize provider event fails encode pre-validation: the terminal
    /// is `invalid_payload` (never success after loss), the in-flight entry
    /// is released, and the connection survives.
    #[tokio::test]
    async fn provider_oversize_event_is_contained_and_releases_in_flight() -> R {
        let (ext, handles) = DemoExtension::new();
        let (runtime, out_rx) = ServerRuntime::new(ext, ServerConfig::default());
        let runtime = Arc::new(runtime);
        let probe = Arc::clone(&runtime);
        let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let server =
            tokio::spawn(
                async move { serve_io_inner(server_rx, server_tx, runtime, out_rx).await },
            );
        let mut client = RawClient {
            write: client_tx,
            read: BufReader::new(client_rx),
        };
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::PROVIDER_STREAM.to_owned(),
                payload: json!({ "providerId": "hugeProv", "model": {}, "context": {}, "options": {} }),
            })
            .await?;
        let terminal = client.recv().await?;
        assert_eq!(terminal.id, 2);
        assert_eq!(terminal.kind, FrameKind::Error);
        assert_eq!(terminal.method, methods::PROVIDER_STREAM);
        assert_eq!(terminal.payload["code"], "invalid_payload");
        assert!(handles.provider_send_failed.load(AtomicOrdering::SeqCst));
        // The terminal is published only after the in-flight entry is
        // removed, so the map is observably empty here.
        assert!(
            probe
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "cancelled/terminated provider must release its in-flight slot"
        );
        // The probe keeps an `out_tx` clone alive; release it so the
        // writer can close during teardown.
        drop(probe);
        // The connection and sibling requests survive the poisoned stream.
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 3);
        assert_eq!(load.kind, FrameKind::Res);
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Optional surfaces an extension does not implement answer with the
    /// honest defaults: flags `ok:false`, shortcut `handled:false`,
    /// terminal `{}`, render `{}`, lifecycle `{}`, command/provider
    /// `not_found`.
    #[tokio::test]
    async fn unimplemented_surfaces_answer_honest_defaults() -> R {
        let (mut client, server) = spawn_raw(DefaultExtension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let cases: &[(u64, &str, Value, Value)] = &[
            (
                2,
                FLAGS_SET_METHOD,
                json!({ "values": { "a": true } }),
                json!({ "ok": false }),
            ),
            (
                3,
                SHORTCUT_EXECUTE_METHOD,
                json!({ "key": "ctrl+x" }),
                json!({ "handled": false }),
            ),
            (
                4,
                Method::TerminalInput.as_str(),
                json!({ "data": "ls" }),
                json!({}),
            ),
            (
                5,
                TOOL_RENDER_HTML_METHOD,
                json!({ "phase": "call", "toolName": "t", "payload": {} }),
                json!({}),
            ),
            (6, "session_start", json!({}), json!({})),
        ];
        for (id, method, payload, expected) in cases {
            client
                .send(&Frame {
                    id: *id,
                    kind: FrameKind::Req,
                    method: (*method).to_owned(),
                    payload: payload.clone(),
                })
                .await?;
            let res = client.recv().await?;
            assert_eq!(res.id, *id, "{method}");
            assert_eq!(res.kind, FrameKind::Res, "{method}");
            assert_eq!(&res.payload, expected, "{method}");
        }
        for (id, method, payload) in [
            (
                7u64,
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "nope", "args": "" }),
            ),
            (
                8u64,
                methods::PROVIDER_STREAM,
                json!({ "providerId": "nope" }),
            ),
        ] {
            client
                .send(&Frame {
                    id,
                    kind: FrameKind::Req,
                    method: method.to_owned(),
                    payload,
                })
                .await?;
            let error = client.recv().await?;
            assert_eq!(error.id, id, "{method}");
            assert_eq!(error.kind, FrameKind::Error, "{method}");
            assert_eq!(error.payload["code"], "not_found", "{method}");
        }
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Saturated-pipe cancellation: forwarders must not block cleanup
    // -----------------------------------------------------------------------

    /// Poll a condition with a deadline.
    async fn wait_until(mut condition: impl FnMut() -> bool) -> R {
        tokio::time::timeout(TIMEOUT, async move {
            while !condition() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
    }

    /// With the whole bounded pipeline saturated and the client not
    /// draining, `tool.cancel` must still reach in-flight cleanup: the
    // cancel-aware forwarder drops queued updates, the in-flight entry is
    /// removed, and the permit becomes available once the terminal drains.
    #[tokio::test]
    async fn tool_cancel_under_saturation_releases_in_flight_and_permit() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            update_capacity: 4,
            outbound_capacity: 4,
            ..ServerConfig::default()
        };
        let (runtime, out_rx) = ServerRuntime::new(ext, config);
        let runtime = Arc::new(runtime);
        let probe = Arc::clone(&runtime);
        let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let server =
            tokio::spawn(
                async move { serve_io_inner(server_rx, server_tx, runtime, out_rx).await },
            );
        let mut client = RawClient {
            write: client_tx,
            read: BufReader::new(client_rx),
        };
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        // Occupy the only slot and saturate the whole bounded pipeline.
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "flood", "toolCallId": "t1", "args": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;
        wait_flag(&handles.saturated).await?;

        // Cancel while the client is not draining: the forwarder is blocked
        // on a full outbound channel and must still return promptly.
        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::TOOL_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        wait_flag(&handles.cancelled).await?;
        wait_until(|| {
            probe
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        })
        .await?;

        // Drain: the cancelled terminal arrives, then the freed permit
        // admits a sibling request.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let frame = client.recv().await?;
                if frame.id == 2 && frame.kind == FrameKind::Error {
                    assert_eq!(frame.payload["code"], "cancelled");
                    break Ok::<(), Box<dyn Error>>(());
                }
            }
        })
        .await??;
        wait_until(|| probe.semaphore.available_permits() == 1).await?;
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 3);
        assert_eq!(load.kind, FrameKind::Res);
        drop(probe);
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Same contract for `provider.stream`: cancellation unblocks the
    /// bounded sink wait and the saturated forwarder, so cleanup and the
    /// permit release proceed without the client draining first.
    #[tokio::test]
    async fn provider_cancel_under_saturation_releases_in_flight_and_permit() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            update_capacity: 4,
            outbound_capacity: 4,
            ..ServerConfig::default()
        };
        let (runtime, out_rx) = ServerRuntime::new(ext, config);
        let runtime = Arc::new(runtime);
        let probe = Arc::clone(&runtime);
        let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let server =
            tokio::spawn(
                async move { serve_io_inner(server_rx, server_tx, runtime, out_rx).await },
            );
        let mut client = RawClient {
            write: client_tx,
            read: BufReader::new(client_rx),
        };
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::PROVIDER_STREAM.to_owned(),
                payload: json!({ "providerId": "floodProv", "model": {}, "context": {}, "options": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.provider_started.notified()).await?;
        // Plateau detection: while the producer floods, the tick counter
        // advances constantly; once every buffer in the pipeline (duplex,
        // outbound, per-call channel) is full, the producer is blocked in
        // the cancellation-aware sink wait and the counter stalls.
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let before = handles.provider_ticks.load(AtomicOrdering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                let after = handles.provider_ticks.load(AtomicOrdering::SeqCst);
                if before == after && after > 0 {
                    break;
                }
            }
        })
        .await?;

        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::PROVIDER_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        wait_flag(&handles.provider_cancelled).await?;
        wait_until(|| {
            probe
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        })
        .await?;

        tokio::time::timeout(TIMEOUT, async {
            loop {
                let frame = client.recv().await?;
                if frame.id == 2 && frame.kind == FrameKind::Error {
                    assert_eq!(frame.payload["code"], "cancelled");
                    break Ok::<(), Box<dyn Error>>(());
                }
            }
        })
        .await??;
        wait_until(|| probe.semaphore.available_permits() == 1).await?;
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: EXTENSIONS_LOAD_METHOD.to_owned(),
                payload: json!({ "cwd": "/tmp" }),
            })
            .await?;
        let load = client.recv().await?;
        assert_eq!(load.id, 3);
        assert_eq!(load.kind, FrameKind::Res);
        drop(probe);
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn provider_event_sink_retains_encoded_bytes() -> R {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(1);
        let invalid = Arc::new(AtomicBool::new(false));
        let sink = ProviderEventSink {
            id: 7,
            tx,
            cancel: CancellationToken::new(),
            invalid: Arc::clone(&invalid),
        };
        assert!(sink.send(demo_event("queued")).await);
        assert!(!invalid.load(AtomicOrdering::SeqCst));
        let outbound = rx.recv().await.ok_or("queued event missing")?;
        let OutboundFrame::Encoded(bytes) = outbound else {
            return Err("provider event was not retained as encoded bytes".into());
        };
        let frame = decode_frame_str(std::str::from_utf8(&bytes)?.trim_end())?;
        assert_eq!(frame.id, 7);
        assert_eq!(frame.kind, FrameKind::Event);
        assert_eq!(frame.method, Method::ProviderEvent.as_str());
        Ok(())
    }

    /// The native event sink pre-validates events: oversize/unencodable
    /// events fail the `send` itself instead of being dropped by the writer
    /// after a successful queue.
    #[tokio::test]
    async fn native_event_sink_rejects_unencodable_events() -> R {
        let (tx, mut rx) = mpsc::channel::<OutboundFrame>(2);
        let sink = NativeEventSink { tx };
        assert!(
            !sink
                .send("uiSlot", json!({ "blob": "x".repeat(9 * 1024 * 1024) }))
                .await,
            "oversize event must fail pre-validation"
        );
        assert!(
            sink.send(
                "uiSlot",
                json!({
                    "key": "demo",
                    "generation": 1,
                    "placement": "aboveEditor",
                    "height": 1,
                    "runs": [[{ "text": "hi", "style": {} }]],
                })
            )
            .await,
            "valid event must queue"
        );
        let outbound = rx.recv().await.ok_or("queued event missing")?;
        let OutboundFrame::Encoded(bytes) = outbound else {
            return Err("valid native event was not retained as encoded bytes".into());
        };
        let line = std::str::from_utf8(&bytes)?.trim_end();
        let frame = decode_frame_str(line)?;
        assert_eq!(frame.id, 0);
        assert_eq!(frame.kind, FrameKind::Event);
        assert_eq!(frame.method, "uiSlot");
        Ok(())
    }

    #[tokio::test]
    async fn deferred_rejection_overflow_terminates_blocked_transport() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            update_capacity: 1,
            outbound_capacity: 1,
            ..ServerConfig::default()
        };
        let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let writer_block = Arc::new(AtomicBool::new(false));
        let writer_writes = Arc::new(AtomicUsize::new(0));
        let writer = BlockingWriter {
            block: Arc::clone(&writer_block),
            writes: Arc::clone(&writer_writes),
        };
        let mut server =
            tokio::spawn(async move { serve_io(server_rx, writer, ext, config).await });

        let hello = Frame {
            id: 1,
            kind: FrameKind::Req,
            method: Method::Hello.as_str().to_owned(),
            payload: to_payload(&Hello {
                protocol_version: PROTOCOL_VERSION,
                compatibility_version: COMPATIBILITY_VERSION.to_owned(),
            })?,
        };
        client_tx.write_all(&encode_frame(&hello)?).await?;
        client_tx.flush().await?;
        wait_until(|| writer_writes.load(AtomicOrdering::SeqCst) >= 1).await?;

        let load = Frame {
            id: 2,
            kind: FrameKind::Req,
            method: EXTENSIONS_LOAD_METHOD.to_owned(),
            payload: json!({ "cwd": "/blocked-transport-context" }),
        };
        client_tx.write_all(&encode_frame(&load)?).await?;
        client_tx.flush().await?;
        wait_until(|| writer_writes.load(AtomicOrdering::SeqCst) >= 2).await?;
        writer_block.store(true, AtomicOrdering::SeqCst);

        let blocking = Frame {
            id: 3,
            kind: FrameKind::Req,
            method: methods::TOOL_EXECUTE.to_owned(),
            payload: json!({ "name": "block", "toolCallId": "hold", "args": {} }),
        };
        client_tx.write_all(&encode_frame(&blocking)?).await?;
        client_tx.flush().await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;

        let mut overloads = Vec::new();
        for id in 4..=9 {
            overloads.extend(encode_frame(&Frame {
                id,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "echo", "toolCallId": format!("overload-{id}"), "args": {} }),
            })?);
        }
        client_tx.write_all(&overloads).await?;
        client_tx.flush().await?;

        let joined = match tokio::time::timeout(TIMEOUT, &mut server).await {
            Ok(joined) => joined?,
            Err(elapsed) => {
                server.abort();
                return Err(elapsed.into());
            }
        };
        assert!(
            matches!(joined, Err(ServerError::OutboundOverflow)),
            "expected bounded overflow failure, got {joined:?}"
        );
        drop(client_tx);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // PR #2 review regressions
    // -----------------------------------------------------------------------

    /// A `theme.update` broadcast is delivered to the native theme callback
    /// instead of being silently discarded as an unknown event.
    #[tokio::test]
    async fn theme_update_is_delivered_to_native_callback() -> R {
        let (ext, handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: crate::protocol::THEME_UPDATE_METHOD.to_owned(),
                payload: json!({
                    "theme": { "colorMode": "truecolor", "fg": {}, "bg": {} },
                    "terminalTheme": "dark",
                    "themeMode": "auto",
                    "themeGeneration": 7,
                    "themes": [],
                }),
            })
            .await?;

        wait_until(|| {
            handles
                .theme_updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                == 1
        })
        .await?;
        {
            let updates = handles
                .theme_updates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(updates[0].theme_generation, 7);
            assert_eq!(updates[0].terminal_theme, "dark");
        }

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// A panicking theme callback is isolated and reported without stopping
    /// the native transport.
    #[tokio::test]
    async fn panicking_theme_callback_reports_error_and_keeps_serving() -> R {
        let (mut client, server) = spawn_raw(PanickingExtension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: crate::protocol::THEME_UPDATE_METHOD.to_owned(),
                payload: json!({
                    "theme": { "colorMode": "truecolor", "fg": {}, "bg": {} },
                    "terminalTheme": "dark",
                    "themeMode": "auto",
                    "themeGeneration": 8,
                    "themes": [],
                }),
            })
            .await?;
        let failure = tokio::time::timeout(TIMEOUT, client.recv()).await??;
        assert_eq!(failure.kind, FrameKind::Event);
        assert_eq!(failure.method, Method::ExtensionError.as_str());
        assert_eq!(failure.payload["code"], "internal");

        let sibling = client.request(3, "not_advertised", json!({})).await?;
        assert_eq!(sibling.kind, FrameKind::Error);
        assert_eq!(sibling.payload["code"], "unknown_method");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn theme_panic_report_uses_deferred_queue_under_backpressure() -> R {
        let config = ServerConfig {
            outbound_capacity: 1,
            ..ServerConfig::default()
        };
        let (runtime, _out_rx) = ServerRuntime::new(PanickingExtension, config);
        let runtime = Arc::new(runtime);
        runtime
            .out_tx
            .try_send(Frame::event(0, Method::Notify, json!({})).into())
            .map_err(|error| format!("fill outbound queue: {error}"))?;
        let (rejection_tx, mut rejection_rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();

        dispatch_ready(
            Frame {
                id: 0,
                kind: FrameKind::Event,
                method: crate::protocol::THEME_UPDATE_METHOD.to_owned(),
                payload: json!({
                    "theme": { "colorMode": "truecolor", "fg": {}, "bg": {} },
                    "terminalTheme": "dark",
                    "themeMode": "auto",
                    "themeGeneration": 9,
                    "themes": [],
                }),
            },
            &runtime,
            &rejection_tx,
            &mut tasks,
        )?;

        let failure = rejection_rx
            .try_recv()
            .map_err(|error| format!("missing deferred theme failure: {error}"))?;
        assert_eq!(failure.kind, FrameKind::Event);
        assert_eq!(failure.method, Method::ExtensionError.as_str());
        assert_eq!(failure.payload["code"], "internal");
        Ok(())
    }

    /// A panicking lifecycle callback is caught: the peer receives a
    /// correlated `internal` error frame and the server keeps serving.
    #[tokio::test]
    async fn panicking_callback_answers_correlated_internal_error() -> R {
        let (mut client, server) = spawn_raw(PanickingExtension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let response = client
            .request(2, "boom", json!({ "eventType": "boom", "payload": {} }))
            .await?;
        assert_eq!(response.id, 2);
        assert_eq!(response.kind, FrameKind::Error);
        assert_eq!(response.payload["code"], "internal");

        // The server is still alive: a sibling request completes normally.
        let sibling = client.request(3, "not_advertised", json!({})).await?;
        assert_eq!(sibling.id, 3);
        assert_eq!(sibling.kind, FrameKind::Error);
        assert_eq!(sibling.payload["code"], "unknown_method");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// A lifecycle callback that never resolves is dropped at the server-side
    /// deadline: the peer gets a correlated `timeout` error and the permit is
    /// released so a later request still succeeds.
    #[tokio::test]
    async fn lifecycle_deadline_drops_stalled_hook_and_releases_permit() -> R {
        let (ext, _handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            lifecycle_deadline: Duration::from_millis(50),
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let started_at = std::time::Instant::now();
        let response = client
            .request(2, "stall", json!({ "eventType": "stall", "payload": {} }))
            .await?;
        let elapsed = started_at.elapsed();
        assert_eq!(response.id, 2);
        assert_eq!(response.kind, FrameKind::Error);
        assert_eq!(response.payload["code"], "timeout");
        assert!(
            elapsed >= Duration::from_millis(50),
            "hook timed out before its deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "hook exceeded its bounded deadline: {elapsed:?}"
        );

        // The permit was released: a normal lifecycle hook still runs. The
        // hook emits an id-0 `uiSlot` event before its terminal response.
        client
            .send(&Frame {
                id: 3,
                kind: FrameKind::Req,
                method: "session_start".to_owned(),
                payload: json!({ "eventType": "session_start", "payload": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let frame = client.recv().await?;
                if frame.id == 3 {
                    assert_eq!(frame.kind, FrameKind::Res);
                    break Ok::<(), Box<dyn Error>>(());
                }
            }
        })
        .await??;

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// A stalled command times out under the shared callback deadline and frees
    /// the sole permit for the next command.
    #[tokio::test]
    async fn command_deadline_drops_stalled_callback_and_releases_permit() -> R {
        let (ext, _handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            lifecycle_deadline: Duration::from_millis(50),
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let started_at = std::time::Instant::now();
        let response = client
            .request(
                2,
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "stall", "args": "" }),
            )
            .await?;
        let elapsed = started_at.elapsed();
        assert_eq!(response.id, 2);
        assert_eq!(response.method, COMMAND_EXECUTE_METHOD);
        assert_eq!(response.kind, FrameKind::Error);
        assert_eq!(response.payload["code"], "timeout");
        assert!(
            elapsed >= Duration::from_millis(50),
            "command timed out before its deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "command exceeded its bounded deadline: {elapsed:?}"
        );

        let next = client
            .request(
                3,
                COMMAND_EXECUTE_METHOD,
                json!({ "command": "demo", "args": "after-timeout" }),
            )
            .await?;
        assert_eq!(next.id, 3);
        assert_eq!(next.kind, FrameKind::Res);
        assert_eq!(next.payload["ok"], true);

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Rendering has no cancellation token either, so its stalled callback
    /// must also time out and free capacity for the next render.
    #[tokio::test]
    async fn render_deadline_drops_stalled_callback_and_releases_permit() -> R {
        let (ext, _handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            lifecycle_deadline: Duration::from_millis(50),
            ..ServerConfig::default()
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let started_at = std::time::Instant::now();
        let response = client
            .request(
                2,
                TOOL_RENDER_HTML_METHOD,
                json!({ "toolName": "stall", "phase": "call", "payload": {} }),
            )
            .await?;
        let elapsed = started_at.elapsed();
        assert_eq!(response.id, 2);
        assert_eq!(response.method, TOOL_RENDER_HTML_METHOD);
        assert_eq!(response.kind, FrameKind::Error);
        assert_eq!(response.payload["code"], "timeout");
        assert!(
            elapsed >= Duration::from_millis(50),
            "render timed out before its deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "render exceeded its bounded deadline: {elapsed:?}"
        );

        let next = client
            .request(
                3,
                TOOL_RENDER_HTML_METHOD,
                json!({ "toolName": "echo", "phase": "call", "payload": {} }),
            )
            .await?;
        assert_eq!(next.id, 3);
        assert_eq!(next.kind, FrameKind::Res);
        assert_eq!(next.payload["html"], "<b>echo:call</b>");

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }

    /// Writer shutdown completes even when a detached event-sink clone retains
    /// an outbound sender indefinitely after the callback returns.
    #[tokio::test]
    async fn writer_shutdown_completes_despite_retained_event_sink() -> R {
        let release = CancellationToken::new();
        let extension = DetachedEventSinkExtension {
            release: release.clone(),
        };
        let (mut client, server) = spawn_raw(extension, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        let response = client
            .request(2, "detach", json!({ "eventType": "detach", "payload": {} }))
            .await?;
        assert_eq!(response.id, 2);
        assert_eq!(response.kind, FrameKind::Res);

        // EOF on stdin: the detached sink still holds an outbound sender, but
        // the explicit shutdown signal must let serve_io return anyway.
        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        // The detached task is still holding its sender; release it for hygiene.
        release.cancel();
        Ok(())
    }

    /// A poisoned `in_flight` lock must not strand a request's cancel token:
    /// registration recovers the lock, so a later `tool.cancel` still reaches
    /// the running execution and the peer receives a correlated terminal.
    #[tokio::test]
    async fn poisoned_in_flight_lock_still_registers_and_cancels() -> R {
        let (ext, handles) = DemoExtension::new();
        let config = ServerConfig {
            max_in_flight: 1,
            ..ServerConfig::default()
        };
        let (runtime, out_rx) = ServerRuntime::new(ext, config);
        let runtime = Arc::new(runtime);
        let probe = Arc::clone(&runtime);
        let (client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let server =
            tokio::spawn(
                async move { serve_io_inner(server_rx, server_tx, runtime, out_rx).await },
            );
        let mut client = RawClient {
            write: client_tx,
            read: BufReader::new(client_rx),
        };
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;
        client.load_context().await?;

        // Poison the in_flight lock before the request registers its token.
        let poisoner = {
            let probe = Arc::clone(&probe);
            std::thread::spawn(move || {
                let _guard = probe
                    .in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                panic!("poison the in_flight mutex");
            })
        };
        let _ = poisoner.join(); // Err(…) = the expected panic
        assert!(probe.in_flight.lock().is_err(), "lock must be poisoned");

        // Register under a poisoned lock: recovery must still insert the token.
        client
            .send(&Frame {
                id: 2,
                kind: FrameKind::Req,
                method: methods::TOOL_EXECUTE.to_owned(),
                payload: json!({ "name": "slow", "toolCallId": "t1", "args": {} }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, handles.started.notified()).await?;
        wait_until(|| {
            probe
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(&2)
        })
        .await?;

        // Cancel must find the token through the recovered lock.
        client
            .send(&Frame {
                id: 0,
                kind: FrameKind::Event,
                method: methods::TOOL_CANCEL.to_owned(),
                payload: json!({ "id": 2 }),
            })
            .await?;
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let frame = client.recv().await?;
                if frame.id == 2 && frame.kind == FrameKind::Error {
                    assert_eq!(frame.payload["code"], "cancelled");
                    break Ok::<(), Box<dyn Error>>(());
                }
            }
        })
        .await??;

        drop(client);
        let result = tokio::time::timeout(TIMEOUT, server).await??;
        assert!(result.is_ok());
        Ok(())
    }
}
