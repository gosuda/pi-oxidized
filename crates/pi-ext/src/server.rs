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
//! `command.execute`, and honors `tool.cancel` control events. Request
//! handling is concurrent: the read loop keeps consuming frames while tool
//! executions run, so cancel frames are observed mid-execution. In-flight
//! work and per-call update channels are bounded; unknown methods fail
//! closed with a correlated error frame without affecting other requests.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use pi_agent::ToolExecutionMode;

use crate::adapters::methods;
use crate::protocol::{
    ErrorPayload, Frame, FrameDecoder, FrameId, FrameKind, Hello, HelloAck, Method,
    PROTOCOL_VERSION, ToolUpdate, encode_frame, from_payload, to_payload,
};

/// Wire method for the registry snapshot request.
pub const EXTENSIONS_LOAD_METHOD: &str = "extensions.load";
/// Wire method for slash-command execution.
pub const COMMAND_EXECUTE_METHOD: &str = "command.execute";

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
    /// Maximum number of requests handled concurrently. Requests beyond the
    /// bound are rejected with a correlated `overloaded` error frame; the
    /// read loop never stalls, so cancel frames stay observable.
    pub max_in_flight: usize,
    /// Maximum queued streaming updates per `tool.execute` call. When full,
    /// the stale update is dropped (matching the client's stream
    /// backpressure policy).
    pub update_capacity: usize,
    /// Bound on the shared outbound frame channel.
    pub outbound_capacity: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            update_capacity: DEFAULT_UPDATE_CAPACITY,
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
        }
    }
}

/// Fatal server error (transport, protocol, or handshake failure).
#[derive(Debug, Error)]
pub enum ServerError {
    /// The first frame was not an acceptable `hello`.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// A malformed inbound frame was received.
    #[error("protocol error: {0}")]
    Protocol(String),
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
// Registry snapshot mirror
// ---------------------------------------------------------------------------

/// Tool entry in the `extensions.load` snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSnapshotEntry {
    /// Tool name used in LLM tool calls.
    pub name: String,
    /// Human-readable label.
    pub label: String,
    /// Description for the LLM.
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub parameters: Value,
    /// Optional execution-mode override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<ToolExecutionMode>,
}

/// Slash-command entry in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSnapshotEntry {
    /// Invocation name (without the leading slash).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Origin path (diagnostics).
    pub source: String,
}

/// Keyboard-shortcut entry in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutSnapshotEntry {
    /// Chord (e.g. `ctrl+k`).
    pub key: String,
    /// Human-readable description.
    pub description: String,
    /// Registering extension path.
    pub extension_path: String,
}

/// CLI-flag entry in the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagSnapshotEntry {
    /// Flag name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Flag type tag (`boolean`, `string`, …).
    #[serde(rename = "type")]
    pub kind: String,
    /// Registering extension path.
    pub extension_path: String,
    /// Declared default value, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Current value, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Renderer entry in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererSnapshotEntry {
    /// Renderer kind (`message` or `widget`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Renderer name.
    pub name: String,
}

/// Custom-provider entry in the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshotEntry {
    /// Provider id.
    pub name: String,
    /// Whether the endpoint holds a live `streamSimple` handler.
    pub stream_simple: bool,
    /// Optional base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Optional API shape tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Optional display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional static API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Optional static headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    /// Whether the API key is sent as an `Authorization` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    /// Optional model catalog (open JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Value>,
}

/// Per-path load diagnostic (sibling isolation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadErrorEntry {
    /// Extension path that failed.
    pub path: String,
    /// Failure detail.
    pub error: String,
}

/// pi-ext-owned `Serialize` mirror of the `extensions.load` snapshot served
/// by the TypeScript host.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySnapshot {
    /// Registered tools (first-wins order).
    pub tools: Vec<ToolSnapshotEntry>,
    /// Registered slash commands.
    pub commands: Vec<CommandSnapshotEntry>,
    /// Registered keyboard shortcuts.
    pub shortcuts: Vec<ShortcutSnapshotEntry>,
    /// Registered CLI flags with current values.
    pub flags: Vec<FlagSnapshotEntry>,
    /// Registered renderers.
    pub renderers: Vec<RendererSnapshotEntry>,
    /// Registered custom providers.
    pub providers: Vec<ProviderSnapshotEntry>,
    /// Lifecycle event types with at least one handler installed.
    pub handlers: Vec<String>,
    /// Whether a terminal-input handler is active.
    pub terminal_input: bool,
    /// Number of extensions successfully loaded.
    pub extensions: u64,
    /// Per-path load errors.
    pub errors: Vec<LoadErrorEntry>,
}

// ---------------------------------------------------------------------------
// Tool execution surface
// ---------------------------------------------------------------------------

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
/// policy. Not `Clone`: dropping the sink at the end of `execute_tool`
/// closes the channel so the server can flush queued updates before the
/// terminal response.
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

/// Unstable Mode 3 native extension contract.
///
/// This trait is pre-1.0: methods may change between releases. Implementors
/// are compiled into a native endpoint executable served by [`serve`].
pub trait NativeExtension: Send + Sync + 'static {
    /// Registry snapshot mirror returned for `extensions.load`.
    fn snapshot(&self) -> RegistrySnapshot;

    /// Prepare raw tool arguments (`tool.prepare`).
    fn prepare_tool(
        &self,
        name: String,
        args: Value,
    ) -> NativeFuture<Result<Value, ExtensionFault>>;

    /// Validate prepared tool arguments (`tool.validate`).
    fn validate_tool(
        &self,
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
        call: ToolCall,
        updates: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> NativeFuture<Result<Value, ExtensionFault>>;

    /// Run a slash command (`command.execute`).
    fn execute_command(
        &self,
        command: String,
        args: String,
    ) -> NativeFuture<Result<(), ExtensionFault>>;
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
/// Returns [`ServerError`] on handshake, protocol, or io failure.
pub async fn serve<E: NativeExtension>(extension: E) -> Result<(), ServerError> {
    // pi-ext does not enable tokio's `io-std` feature; bridge blocking
    // stdin/stdout through in-memory duplex streams pumped by blocking
    // threads. The stdin pump detaches on exit (a blocked `read` cannot be
    // aborted); process teardown reclaims it for a real endpoint binary.
    let (stdin_reader, mut stdin_sink) = tokio::io::duplex(64 * 1024);
    let (mut stdout_source, stdout_writer) = tokio::io::duplex(64 * 1024);
    let stdin_pump = tokio::task::spawn_blocking(move || {
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
    });
    let stdout_pump = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        let mut buf = [0u8; 8192];
        loop {
            match block_on(stdout_source.read(&mut buf)) {
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
    });
    let result = serve_io(
        stdin_reader,
        stdout_writer,
        extension,
        ServerConfig::default(),
    )
    .await;
    drop(stdin_pump);
    let _ = stdout_pump.await;
    result
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
/// Returns [`ServerError`] on handshake, protocol, or io failure.
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
    let writer_dead = CancellationToken::new();
    let mut tasks: JoinSet<()> = JoinSet::new();

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(config.outbound_capacity.max(1));
    let runtime = Arc::new(ServerRuntime {
        extension,
        semaphore: Arc::new(Semaphore::new(config.max_in_flight.max(1))),
        in_flight: Mutex::new(HashMap::new()),
        out_tx,
        update_capacity: config.update_capacity,
    });
    let writer_task = {
        let writer_dead = writer_dead.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            let result: Result<(), ServerError> = async {
                while let Some(frame) = out_rx.recv().await {
                    // Contain per-frame encode failures: one bad payload
                    // (scalar, oversize, or invalid) must not kill the
                    // endpoint or its sibling requests.
                    let Some(bytes) = encode_frame(&frame)
                        .ok()
                        .or_else(|| encode_fallback(&frame))
                    else {
                        continue;
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

    let run_result = drive(reader, &runtime, &writer_dead, &mut tasks).await;

    // Teardown: cancel cooperative executions, stop request tasks, then
    // drop the runtime. The outbound channel closes only after the joined
    // tasks released their runtime clones and the detached update
    // forwarders flushed and released their sender clones, so awaiting the
    // writer below drains every queued frame exactly once.
    if let Ok(map) = runtime.in_flight.lock() {
        for token in map.values() {
            token.cancel();
        }
    }
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        let _ = joined;
    }
    drop(runtime);
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

/// Shared server state: one per `serve_io`, shared by the read loop and
/// every spawned request task. Dropping the last `Arc` clone closes the
/// outbound channel, so the writer drains only after all tasks finish.
struct ServerRuntime<E: NativeExtension> {
    /// Extension implementation served by this endpoint.
    extension: E,
    /// Bound on concurrently handled requests.
    semaphore: Arc<Semaphore>,
    /// Cancel tokens for in-flight `tool.execute` calls, keyed by frame id.
    in_flight: Mutex<HashMap<FrameId, CancellationToken>>,
    /// Shared outbound (server → client) frame channel.
    out_tx: mpsc::Sender<Frame>,
    /// Bound on queued per-call streaming updates.
    update_capacity: usize,
}

/// Read/dispatch loop: runs until EOF, writer death, or a fatal error.
async fn drive<R, E>(
    reader: R,
    runtime: &Arc<ServerRuntime<E>>,
    writer_dead: &CancellationToken,
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
            Ok(0) => break,
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
                    runtime.out_tx.send(ack).await.map_err(|_| {
                        ServerError::Io(std::io::Error::other("outbound channel closed"))
                    })?;
                    state = ServerState::Ready;
                }
                ServerState::Ready => {
                    dispatch_ready(frame, runtime, tasks);
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

/// Dispatch one post-handshake frame.
fn dispatch_ready<E: NativeExtension>(
    frame: Frame,
    runtime: &Arc<ServerRuntime<E>>,
    tasks: &mut JoinSet<()>,
) {
    match frame.kind {
        FrameKind::Req => {
            if let Ok(permit) = runtime.semaphore.clone().try_acquire_owned() {
                // Register the cancel token BEFORE spawning: a `tool.cancel`
                // event read right after this request must find its target.
                let token = (frame.method == methods::TOOL_EXECUTE).then(|| {
                    let token = CancellationToken::new();
                    if let Ok(mut map) = runtime.in_flight.lock() {
                        map.insert(frame.id, token.clone());
                    }
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
                // Fail closed without stalling the read loop. The rejection
                // is best-effort: when the bounded outbound channel is
                // saturated the frame is dropped rather than blocking the
                // loop, so `tool.cancel` events stay observable.
                let overloaded = error_frame(
                    frame.id,
                    &frame.method,
                    "overloaded",
                    "too many in-flight requests",
                    true,
                );
                let _ = runtime.out_tx.try_send(overloaded);
            }
        }
        FrameKind::Event => {
            if frame.method == methods::TOOL_CANCEL
                && let Some(id) = frame.payload.get("id").and_then(Value::as_u64)
                && let Ok(map) = runtime.in_flight.lock()
                && let Some(token) = map.get(&id)
            {
                token.cancel();
            }
            // Unknown events are fire-and-forget: ignored by design.
        }
        // The native endpoint initiates no requests; stray res/error frames
        // carry no correlation state and are ignored.
        FrameKind::Res | FrameKind::Error => {}
    }
}

/// Handle one request to a terminal frame and send it.
async fn handle_request<E: NativeExtension>(
    runtime: Arc<ServerRuntime<E>>,
    frame: Frame,
    token: Option<CancellationToken>,
) {
    let id = frame.id;
    let method = frame.method.clone();
    let terminal = match method.as_str() {
        EXTENSIONS_LOAD_METHOD => handle_load(&runtime.extension, id, &method),
        methods::TOOL_PREPARE => {
            handle_prepare(&runtime.extension, id, &method, &frame.payload).await
        }
        methods::TOOL_VALIDATE => {
            handle_validate(&runtime.extension, id, &method, &frame.payload).await
        }
        methods::TOOL_EXECUTE => {
            execute_tool_request(&runtime, id, &method, frame.payload, token).await;
            return;
        }
        COMMAND_EXECUTE_METHOD => {
            handle_command(&runtime.extension, id, &method, &frame.payload).await
        }
        m if m == Method::Hello.as_str() => error_frame(
            id,
            &method,
            "invalid_request",
            "hello already completed",
            false,
        ),
        _ => error_frame(
            id,
            &method,
            "unknown_method",
            &format!("unknown method: {method}"),
            false,
        ),
    };
    let _ = runtime.out_tx.send(terminal).await;
}

/// `extensions.load`: encode the registry snapshot mirror.
fn handle_load<E: NativeExtension>(extension: &E, id: FrameId, method: &str) -> Frame {
    match serde_json::to_value(extension.snapshot()) {
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

/// `tool.prepare`: run the extension's argument preparation.
async fn handle_prepare<E: NativeExtension>(
    extension: &E,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
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
    match extension.prepare_tool(name.to_owned(), args).await {
        Ok(args) => res_frame(id, method, json!({ "args": args })),
        Err(fault) => fault_frame(id, method, &fault),
    }
}

/// `tool.validate`: run the extension's argument validation.
async fn handle_validate<E: NativeExtension>(
    extension: &E,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
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
    match extension
        .validate_tool(name.to_owned(), args, tool_call_id)
        .await
    {
        Ok(args) => res_frame(id, method, json!({ "args": args })),
        Err(fault) => fault_frame(id, method, &fault),
    }
}

/// `command.execute`: run a slash command.
async fn handle_command<E: NativeExtension>(
    extension: &E,
    id: FrameId,
    method: &str,
    payload: &Value,
) -> Frame {
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
    match extension.execute_command(command, args).await {
        Ok(()) => res_frame(id, method, json!({ "ok": true })),
        Err(fault) => fault_frame(id, method, &fault),
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
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        if let Ok(mut map) = runtime.in_flight.lock() {
            map.remove(&id);
        }
        let _ = runtime
            .out_tx
            .send(error_frame(
                id,
                method,
                "invalid_request",
                "tool.execute requires a string name",
                false,
            ))
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

    // Forward updates from a dedicated task: backpressure on the bounded
    // outbound channel stalls only the forwarder, never the extension, so a
    // `tool.cancel` is observed even while the pipe is saturated.
    let forwarder = {
        let out = runtime.out_tx.clone();
        let tool_call_id = tool_call_id.clone();
        let name = call.name.clone();
        tokio::spawn(async move {
            while let Some(partial) = update_rx.recv().await {
                if out
                    .send(update_frame(id, &tool_call_id, &name, partial))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    let result = runtime
        .extension
        .execute_tool(call, ToolUpdateSink { tx: update_tx }, token.clone())
        .await;

    // The sink was consumed by the call, so the update channel is closed;
    // awaiting the forwarder flushes every queued update before the
    // terminal frame is published.
    let _ = forwarder.await;
    if let Ok(mut map) = runtime.in_flight.lock() {
        map.remove(&id);
    }

    let terminal = if token.is_cancelled() {
        error_frame(id, method, "cancelled", "extension tool cancelled", false)
    } else {
        match result {
            Ok(value) => res_frame(id, method, value),
            Err(fault) => fault_frame(id, method, &fault),
        }
    };
    let _ = runtime.out_tx.send(terminal).await;
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
mod tests {
    use super::*;
    use crate::client::{HandshakePolicy, HostClient, HostClientError};
    use crate::protocol::{COMPATIBILITY_VERSION, decode_frame_str};
    use std::error::Error;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
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
        saturated: Arc<AtomicBool>,
        commands: Arc<Mutex<Vec<(String, String)>>>,
    }

    /// Scripted extension used across the server tests.
    struct DemoExtension {
        handles: DemoHandles,
    }

    impl DemoExtension {
        fn new() -> (Self, DemoHandles) {
            let handles = DemoHandles {
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                cancelled: Arc::new(AtomicBool::new(false)),
                saturated: Arc::new(AtomicBool::new(false)),
                commands: Arc::new(Mutex::new(Vec::new())),
            };
            let ext = Self {
                handles: DemoHandles {
                    started: Arc::clone(&handles.started),
                    release: Arc::clone(&handles.release),
                    cancelled: Arc::clone(&handles.cancelled),
                    saturated: Arc::clone(&handles.saturated),
                    commands: Arc::clone(&handles.commands),
                },
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
                shortcuts: vec![],
                flags: vec![],
                renderers: vec![],
                providers: vec![],
                handlers: vec!["session_start".to_owned()],
                terminal_input: true,
                extensions: 1,
                errors: vec![],
            }
        }

        fn prepare_tool(
            &self,
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
            call: ToolCall,
            updates: ToolUpdateSink,
            cancel: CancellationToken,
        ) -> NativeFuture<Result<Value, ExtensionFault>> {
            let started = Arc::clone(&self.handles.started);
            let release = Arc::clone(&self.handles.release);
            let cancelled = Arc::clone(&self.handles.cancelled);
            let saturated = Arc::clone(&self.handles.saturated);
            Box::pin(async move {
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
            command: String,
            args: String,
        ) -> NativeFuture<Result<(), ExtensionFault>> {
            let commands = Arc::clone(&self.handles.commands);
            Box::pin(async move {
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

    // -----------------------------------------------------------------------
    // Unknown methods fail closed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unknown_method_fails_closed_without_killing_unrelated_requests() -> R {
        let (ext, _handles) = DemoExtension::new();
        let (mut client, server) = spawn_raw(ext, ServerConfig::default());
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;

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
        /// Connect a `HostClient` to a fresh `DemoExtension` server and
        /// complete the protocol-only handshake against the native endpoint.
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
    async fn duplex_cancel_maps_to_cancelled_error() -> R {
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
        slow.cancel(methods::TOOL_CANCEL).await?;
        let cancelled_terminal = slow.finish(TIMEOUT).await;
        assert!(
            matches!(cancelled_terminal, Err(HostClientError::Remote { ref code, .. }) if code == "cancelled"),
            "expected cancelled remote error, got {cancelled_terminal:?}"
        );
        assert!(
            fixture.handles.cancelled.load(AtomicOrdering::SeqCst),
            "extension must observe the cancellation token"
        );
        fixture.finish().await
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
                payload: json!({}),
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
        };
        let (mut client, server) = spawn_raw(ext, config);
        client.hello(PROTOCOL_VERSION, "anything").await?;
        let _ack = client.recv().await?;

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

        // Overloaded request: its rejection is dropped (outbound full) and
        // must not block the read loop...
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

        // Drain: the cancelled terminal for id 2 eventually arrives once the
        // client starts reading again.
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
}
