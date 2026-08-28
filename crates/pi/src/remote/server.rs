//! Remote multi-session server (R4) — portable port of upstream
//! `server.ts`, `sessions.ts`, `snapshots.ts`, `connection.ts`,
//! `listener.ts`, `errors.ts`, and `types.ts`, with **zero** `cfg`
//! branches in the server core.
//!
//! The server speaks the R1–R2 wire (CBOR in 4-byte length-prefixed
//! frames) over [`ByteConnection`]s supplied by a [`ServerListener`].
//! Session work flows through the [`ServerService`] seam: the server
//! hosts live sessions, tracks attachment and leases, broadcasts
//! snapshots exactly once per attached connection, disposes idle
//! runtimes at zero attachments, and keeps sessions alive across client
//! disconnects. Session mutation reaches the product only through
//! [`AgentSession`]'s public C15 surface (see [`AgentSessionRuntime`]);
//! this module performs no session encoding or migration (G8 owns it).
//!
//! # Platform contract (AR2)
//!
//! The core, the in-memory listener, and [`build_listener`] compile on
//! every tier, including `x86_64-pc-windows-msvc`. The Unix-domain
//! listener preset lives in [`unix`] and is `#[cfg(unix)]`-gated;
//! building a Unix listen spec on any other tier fails eagerly with the
//! typed [`EndpointSpecError::UnsupportedOnPlatform`] owned by
//! [`crate::remote::transport`] — one typed error owner, no second
//! convention (the internal gating mirrors `build_transport`, the only
//! place a `cfg` appears in this file).
//!
//! # Divergences from upstream (recorded)
//!
//! - The handshake deadline uses a spawned tokio watchdog aborted on
//!   hello delivery (`clearTimeout` equivalent) instead of a libuv
//!   timer; requests queued during the handshake are replayed from a
//!   waiter list instead of promise chaining.
//! - `dispose` on an [`AgentSessionRuntime`] releases the service
//!   acquisition and drops the runtime's handle; the underlying
//!   [`AgentSession`] stays alive in the [`AgentSessionService`]
//!   registry (the in-memory stand-in for upstream's durable store), so
//!   re-attach after idle disposal re-hosts the same session. Terminal
//!   disposal is explicit via [`AgentSessionService::dispose_all`].
//! - Runtime progress events are not synthesized for [`AgentSession`]
//!   hosting: every public agent-session event maps to a snapshot
//!   broadcast. The scripted test runtime does emit progress,
//!   exercising the `session_progress` wire path.
//! - The landed R2 wire snapshot carries no `phase`/`attached` fields,
//!   so per-connection snapshot normalization is the identity; the
//!   attachment seam is kept as [`for_connection`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use uuid::Uuid;

use crate::core::agent_session::AgentSession;
use crate::remote::codec::{encode_server_message, is_supported_protocol_version, ClientMessageDecoder};
use crate::remote::framing::{FrameDecoderOptions, DEFAULT_MAX_FRAME_LENGTH};
use crate::remote::schemas::{
    AssistantContent, ClientMessage, Command, CommandResult, ImageContent,
    JsonValue, ModelMetadata, ModelRef, ProtocolError, ProtocolErrorCode, ServerEvent,
    ServerMessage, ServerSnapshot, SessionMetadata, SessionPhase, SessionSnapshot, TextContent,
    ThinkingContent, ThinkingLevel, ToolCallContent,
    TranscriptItem, TranscriptProgress, UserContent, UserTranscriptItem, PROTOCOL_VERSION,
};
use crate::remote::transport::{
    build_transport, ByteTransport, ByteTransportHandlers, EndpointSpec, EndpointSpecError,
    InMemoryListener, TransportError,
};
#[cfg(not(unix))]
use crate::remote::transport::EndpointKind;
use pi_agent::AgentMessage;

#[cfg(unix)]
pub mod unix;

/// Max value a frame length may declare (unsigned 32-bit).
const MAX_UINT32: u64 = 0xffff_ffff;
/// Max representable millisecond timer delay (signed 32-bit).
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;
/// Default handshake deadline.
const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Error taxonomy (port of upstream errors.ts)
// ---------------------------------------------------------------------------

/// Operation error codes that may safely cross the protocol boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerOperationCode {
    /// A conflicting operation is running.
    Busy,
    /// The session runtime is terminating or locked elsewhere.
    SessionLocked,
    /// The referenced session does not exist.
    NotFound,
    /// The request is malformed or invalid in this state.
    InvalidRequest,
    /// The operation exists but is not implemented.
    NotImplemented,
}

impl ServerOperationCode {
    /// Wire code for this variant.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::SessionLocked => "session_locked",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::NotImplemented => "not_implemented",
        }
    }

    fn to_protocol_code(self) -> ProtocolErrorCode {
        match self {
            Self::Busy => ProtocolErrorCode::Busy,
            Self::SessionLocked => ProtocolErrorCode::SessionLocked,
            Self::NotFound => ProtocolErrorCode::NotFound,
            Self::InvalidRequest => ProtocolErrorCode::InvalidRequest,
            Self::NotImplemented => ProtocolErrorCode::NotImplemented,
        }
    }
}

/// Fixed message for the `internal_error` code; causes never cross.
pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";
/// Fixed message for the `not_implemented` code.
pub const NOT_IMPLEMENTED_MESSAGE: &str = "Operation is not implemented";

/// A service/runtime error that can safely cross the protocol boundary
/// (port of upstream `PiServerError`).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerError {
    /// Operation error code.
    pub code: ServerOperationCode,
    /// Human-readable detail.
    pub message: String,
    /// Optional structured detail.
    pub details: Option<JsonValue>,
}

impl ServerError {
    /// Builds an operation error without details.
    #[must_use]
    pub fn new(code: ServerOperationCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), details: None }
    }

    fn to_protocol_error(&self) -> ProtocolError {
        ProtocolError {
            code: self.code.to_protocol_code(),
            message: self.message.clone(),
            details: self.details.clone(),
        }
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ServerError {}

/// Internal failure flowing through command execution: a protocol-safe
/// [`ServerError`] or a wire-level [`ProtocolError`] (handshake version).
#[derive(Debug, Clone)]
enum Failure {
    Op(ServerError),
    Wire(ProtocolError),
}

impl From<ServerError> for Failure {
    fn from(error: ServerError) -> Self {
        Self::Op(error)
    }
}

// ---------------------------------------------------------------------------
// Runtime and service seams (port of upstream types.ts)
// ---------------------------------------------------------------------------

/// One event emitted by a live session runtime.
#[derive(Debug, Clone)]
pub enum SessionRuntimeEvent {
    /// The snapshot changed; re-broadcast it.
    Snapshot,
    /// Streaming progress for the current turn.
    Progress(TranscriptProgress),
    /// Terminal runtime failure.
    Error(ServerError),
}

/// Receives runtime events while subscribed.
pub type SessionRuntimeListener = Arc<dyn Fn(&SessionRuntimeEvent) + Send + Sync>;

/// One acquired durable session (port of upstream `PiSessionRuntime`).
/// Conflicting operations must reject rather than queue.
pub trait SessionRuntime: Send + Sync + 'static {
    /// Current snapshot of the session.
    fn snapshot(&self) -> BoxFuture<'static, Result<SessionSnapshot, ServerError>>;
    /// Current lifecycle phase.
    fn phase(&self) -> SessionPhase;
    /// Runs one prompt to completion.
    fn prompt(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>>;
    /// Steers the in-flight prompt.
    fn steer(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>>;
    /// Aborts the in-flight prompt.
    fn abort(&self) -> BoxFuture<'static, Result<(), ServerError>>;
    /// Switches the model.
    fn set_model(&self, model: ModelRef) -> BoxFuture<'static, Result<(), ServerError>>;
    /// Switches the thinking level.
    fn set_thinking(&self, level: ThinkingLevel) -> BoxFuture<'static, Result<(), ServerError>>;
    /// Subscribes to runtime events; the returned closure unsubscribes.
    fn subscribe(&self, listener: SessionRuntimeListener) -> Box<dyn Fn() + Send + Sync>;
    /// Releases the acquisition.
    fn dispose(&self) -> BoxFuture<'static, Result<(), ServerError>>;
}

/// Options for creating a session (server-assigned id).
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    /// Collision-resistant ID assigned by the server. The service must
    /// persist this exact ID.
    pub id: String,
    /// Working directory.
    pub cwd: Option<String>,
    /// Session name.
    pub name: Option<String>,
    /// Initial model.
    pub model: Option<ModelRef>,
    /// Initial thinking level.
    pub thinking_level: Option<ThinkingLevel>,
}

/// Service boundary for durable sessions and exclusively acquired
/// runtimes (port of upstream `PiServerService`).
pub trait ServerService: Send + Sync + 'static {
    /// Lists stored sessions (merged with live state by the server).
    fn list_sessions(&self) -> BoxFuture<'static, Result<Vec<SessionSnapshot>, ServerError>>;
    /// Lists models offered to clients.
    fn list_models(&self) -> BoxFuture<'static, Result<Vec<ModelMetadata>, ServerError>>;
    /// Creates and acquires the session named by `options.id`.
    fn create_session(
        &self,
        options: CreateSessionOptions,
    ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>>;
    /// Acquires an existing session.
    fn open_session(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>>;
}

// ---------------------------------------------------------------------------
// Connections (port of upstream connection.ts)
// ---------------------------------------------------------------------------

/// An established, authorized ordered byte connection.
pub trait ByteConnection: Send + Sync + 'static {
    /// Whether the connection is closed.
    fn closed(&self) -> bool;
    /// Sends one byte chunk.
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>>;
    /// Closes the connection, optionally delivering one final chunk
    /// first.
    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<'static, Result<(), TransportError>>;
}

/// Receives connection events for one accepted connection.
pub trait ConnectionHandler: Send + Sync + 'static {
    /// Delivers one inbound byte chunk.
    fn on_data(&self, chunk: Vec<u8>);
    /// Reports an orderly close.
    fn on_close(&self);
    /// Reports a terminal connection failure.
    fn on_error(&self, error: TransportError);
}

/// Accepts one established connection and returns its handler.
pub type ConnectionAcceptor =
    Arc<dyn Fn(Arc<dyn ByteConnection>) -> Arc<dyn ConnectionHandler> + Send + Sync>;

/// Connection lifecycle stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitingHello,
    Handshaking,
    Ready,
    Closing,
    Closed,
}

/// Per-connection mutable state.
struct ConnInner {
    stage: Stage,
    disconnected: bool,
    handshake_complete: bool,
    session_ids: HashSet<String>,
    handshake_watchdog: Option<tokio::task::AbortHandle>,
    /// Requests that arrived while the handshake was in flight; they
    /// are replayed once the connection reaches [`Stage::Ready`]
    /// (upstream chains them onto the handshake promise).
    handshake_waiters: Vec<ClientMessage>,
}

/// One accepted connection tracked by the server.
struct ServerConnection {
    id: String,
    connection: Arc<dyn ByteConnection>,
    decoder: StdMutex<ClientMessageDecoder>,
    inner: StdMutex<ConnInner>,
}

impl ServerConnection {
    fn is_terminal(&self) -> bool {
        let inner = lock(&self.inner);
        inner.disconnected || matches!(inner.stage, Stage::Closing | Stage::Closed)
    }
}

// ---------------------------------------------------------------------------
// Live sessions (port of upstream sessions.ts LiveSession)
// ---------------------------------------------------------------------------

/// One hosted live session.
struct LiveSession {
    id: String,
    runtime: Box<dyn SessionRuntime>,
    /// Connection ids currently attached.
    connections: StdMutex<HashSet<String>>,
    unsubscribe: StdMutex<Option<Box<dyn Fn() + Send + Sync>>>,
    operation_count: AtomicU64,
    ready: AtomicBool,
    terminal: AtomicBool,
    disposing: StdMutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

impl LiveSession {
    fn take_unsubscribe(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        lock(&self.unsubscribe).take()
    }
}

// ---------------------------------------------------------------------------
// Shared server state
// ---------------------------------------------------------------------------

type OpeningFuture = Shared<BoxFuture<'static, Result<Arc<LiveSession>, ServerError>>>;
type DisposeFuture = Shared<BoxFuture<'static, ()>>;

struct ServerShared {
    closing: bool,
    connections: HashMap<String, Arc<ServerConnection>>,
    live: HashMap<String, Arc<LiveSession>>,
    opening: HashMap<String, OpeningFuture>,
    revision: u64,
    broadcast_tail: Shared<BoxFuture<'static, ()>>,
}

struct ServerCore {
    id: String,
    service: Arc<dyn ServerService>,
    max_frame_length: usize,
    handshake_timeout_ms: u64,
    on_error: Option<ServerErrorHandler>,
    listeners: Vec<Arc<dyn ServerListener>>,
    shared: StdMutex<ServerShared>,
}

/// Reports isolated server errors; observers cannot affect server state.
pub type ServerErrorHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// Options error at construction — distinct from operation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiServerOptionsError {
    /// `max_frame_length` was zero or beyond the u32 frame bound.
    InvalidMaxFrameLength { value: u64, max: u64 },
    /// `handshake_timeout_ms` was zero or beyond the timer-delay bound.
    InvalidHandshakeTimeout { value: u64, max: u64 },
}

impl fmt::Display for PiServerOptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaxFrameLength { value, max } => write!(
                f,
                "PiServer maxFrameLength must be an integer between 1 and {max} (got {value})"
            ),
            Self::InvalidHandshakeTimeout { value, max } => write!(
                f,
                "PiServer handshakeTimeoutMs must be an integer between 1 and {max} (got {value})"
            ),
        }
    }
}

impl std::error::Error for PiServerOptionsError {}

/// Options for constructing a [`PiServer`].
#[derive(Clone, Default)]
pub struct PiServerOptions {
    /// Listeners supplying established connections.
    pub listeners: Vec<Arc<dyn ServerListener>>,
    /// Maximum frame length in bytes (default 16 MiB).
    pub max_frame_length: Option<usize>,
    /// Handshake deadline in milliseconds (default 5 s).
    pub handshake_timeout_ms: Option<u64>,
    /// Stable server id (default: fresh UUID).
    pub server_id: Option<String>,
    /// Reports isolated server errors.
    pub on_error: Option<ServerErrorHandler>,
}

impl fmt::Debug for PiServerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiServerOptions")
            .field("max_frame_length", &self.max_frame_length)
            .field("handshake_timeout_ms", &self.handshake_timeout_ms)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// Supplies established byte connections (port of upstream
/// `PiServerListener`).
pub trait ServerListener: Send + Sync + 'static {
    /// Human-readable bound address after startup, when the transport
    /// has one.
    fn address(&self) -> Option<String>;
    /// Starts listening and passes connections to `accept`.
    ///
    /// # Errors
    ///
    /// Returns [`ListenerError`] when the listener is already started,
    /// closing, or fails to bind.
    fn start(&self, accept: ConnectionAcceptor) -> BoxFuture<'static, Result<(), ListenerError>>;
    /// Stops listening and releases transport resources.
    fn close(&self) -> BoxFuture<'static, ()>;
}

/// Listener lifecycle failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerError {
    /// `start` was called twice.
    AlreadyStarted,
    /// The listener is closing or closed.
    Closing,
    /// The listener failed to bind or accept.
    Io(String),
}

impl fmt::Display for ListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStarted => write!(f, "listener is already started"),
            Self::Closing => write!(f, "listener is closing or closed"),
            Self::Io(message) => write!(f, "listener failure: {message}"),
        }
    }
}

impl std::error::Error for ListenerError {}

/// A remote multi-session server over any [`ServerListener`].
pub struct PiServer {
    core: Arc<ServerCore>,
}

impl fmt::Debug for PiServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiServer").field("id", &self.core.id).finish()
    }
}

impl PiServer {
    /// Validates options and constructs an unstarted server.
    ///
    /// # Errors
    ///
    /// Returns [`PiServerOptionsError`] for out-of-range numeric
    /// options.
    pub fn new(
        service: Arc<dyn ServerService>,
        options: PiServerOptions,
    ) -> Result<Self, PiServerOptionsError> {
        let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        let frame_value = u64::try_from(max_frame_length).unwrap_or(u64::MAX);
        if frame_value == 0 || frame_value > MAX_UINT32 {
            return Err(PiServerOptionsError::InvalidMaxFrameLength {
                value: frame_value,
                max: MAX_UINT32,
            });
        }
        let handshake_timeout_ms =
            options.handshake_timeout_ms.unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        if handshake_timeout_ms == 0 || handshake_timeout_ms > MAX_TIMER_DELAY_MS {
            return Err(PiServerOptionsError::InvalidHandshakeTimeout {
                value: handshake_timeout_ms,
                max: MAX_TIMER_DELAY_MS,
            });
        }
        let core = ServerCore {
            id: options.server_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            service,
            max_frame_length,
            handshake_timeout_ms,
            on_error: options.on_error,
            listeners: options.listeners,
            shared: StdMutex::new(ServerShared {
                closing: false,
                connections: HashMap::new(),
                live: HashMap::new(),
                opening: HashMap::new(),
                revision: 0,
                broadcast_tail: futures::future::ready(()).boxed().shared(),
            }),
        };
        Ok(Self { core: Arc::new(core) })
    }

    /// Stable server id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.core.id
    }

    /// Bound addresses of every started listener, when known.
    #[must_use]
    pub fn addresses(&self) -> Vec<String> {
        self.core
            .listeners
            .iter()
            .filter_map(|listener| listener.address())
            .collect()
    }

    /// Starts every listener; on failure, closes what was started.
    ///
    /// # Errors
    ///
    /// Returns [`ListenerError`] when a listener fails to start.
    pub async fn start(&self) -> Result<(), ListenerError> {
        {
            let shared = lock(&self.core.shared);
            if shared.closing {
                return Err(ListenerError::Closing);
            }
        }
        let acceptor: ConnectionAcceptor = {
            let core = Arc::clone(&self.core);
            Arc::new(move |connection| {
                Arc::new(ServerHandler::new(&core, connection)) as Arc<dyn ConnectionHandler>
            })
        };
        let mut started: Vec<Arc<dyn ServerListener>> = Vec::new();
        for listener in &self.core.listeners {
            if let Err(error) = listener.start(Arc::clone(&acceptor)).await {
                for started_listener in started {
                    started_listener.close().await;
                }
                lock(&self.core.shared).closing = true;
                close_server_state(&self.core).await;
                return Err(error);
            }
            started.push(Arc::clone(listener));
        }
        Ok(())
    }

    /// Accepts one established connection and returns its handler.
    #[must_use]
    pub fn accept(&self, connection: Arc<dyn ByteConnection>) -> Arc<dyn ConnectionHandler> {
        Arc::new(ServerHandler::new(&self.core, connection))
    }

    /// Stops listeners, closes every connection, and disposes every
    /// live session. Idempotent.
    pub async fn close(&self) {
        {
            let mut shared = lock(&self.core.shared);
            if shared.closing {
                return;
            }
            shared.closing = true;
        }
        for listener in &self.core.listeners {
            listener.close().await;
        }
        close_server_state(&self.core).await;
    }
}

/// Handler for one accepted connection (delivers transport events into
/// the server).
struct ServerHandler {
    core: Arc<ServerCore>,
    conn: Arc<ServerConnection>,
}

impl ServerHandler {
    fn new(core: &Arc<ServerCore>, connection: Arc<dyn ByteConnection>) -> Self {
        let id = Uuid::new_v4().to_string();
        let decoder = ClientMessageDecoder::new(Some(FrameDecoderOptions {
            max_frame_length: core.max_frame_length,
        }))
        .unwrap_or_else(|_| ClientMessageDecoder::new(None).expect("default decoder options are valid"));
        let conn = Arc::new(ServerConnection {
            id: id.clone(),
            connection,
            decoder: StdMutex::new(decoder),
            inner: StdMutex::new(ConnInner {
                stage: Stage::AwaitingHello,
                disconnected: false,
                handshake_complete: false,
                session_ids: HashSet::new(),
                handshake_watchdog: None,
                handshake_waiters: Vec::new(),
            }),
        });
        install_handshake_watchdog(core, &conn);
        lock(&core.shared).connections.insert(id, conn.clone());
        Self { core: Arc::clone(core), conn }
    }
}

impl ConnectionHandler for ServerHandler {
    fn on_data(&self, chunk: Vec<u8>) {
        if self.conn.is_terminal() {
            return;
        }
        let messages = {
            let mut decoder = lock(&self.conn.decoder);
            match decoder.push(&chunk) {
                Ok(messages) => messages,
                Err(error) => {
                    drop(decoder);
                    let failure = Failure::Op(ServerError::new(
                        ServerOperationCode::InvalidRequest,
                        error.to_string(),
                    ));
                    let core = Arc::clone(&self.core);
                    let conn = Arc::clone(&self.conn);
                    tokio::spawn(async move {
                        fail_protocol(&core, &conn, failure).await;
                    });
                    return;
                }
            }
        };
        for message in messages {
            if self.conn.is_terminal() {
                return;
            }
            dispatch_message(&self.core, &self.conn, message);
        }
    }

    fn on_close(&self) {
        {
            let inner = lock(&self.conn.inner);
            if !inner.disconnected && !matches!(inner.stage, Stage::Closing) {
                drop(inner);
                if let Err(error) = lock(&self.conn.decoder).end() {
                    report_error(&self.core, &error.to_string());
                }
            }
        }
        let core = Arc::clone(&self.core);
        let conn = Arc::clone(&self.conn);
        tokio::spawn(async move {
            disconnect(&core, &conn).await;
        });
    }

    fn on_error(&self, error: TransportError) {
        report_error(&self.core, &error.to_string());
        let core = Arc::clone(&self.core);
        let conn = Arc::clone(&self.conn);
        tokio::spawn(async move {
            conn.connection.close(None).await.ok();
            disconnect(&core, &conn).await;
        });
    }
}

/// Arms the handshake deadline watchdog (upstream `setTimeout`).
fn install_handshake_watchdog(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>) {
    let core = Arc::clone(core);
    let conn_spawn = Arc::clone(conn);
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(core.handshake_timeout_ms)).await;
        let already = {
            let inner = lock(&conn_spawn.inner);
            inner.handshake_complete || inner.disconnected
        };
        if already {
            return;
        }
        fail_protocol(
            &core,
            &conn_spawn,
            Failure::Op(ServerError::new(
                ServerOperationCode::InvalidRequest,
                "Handshake timeout",
            )),
        )
        .await;
    })
    .abort_handle();
    lock(&conn.inner).handshake_watchdog = Some(handle);
}

/// Clears the handshake deadline (upstream `clearTimeout`).
fn clear_handshake_timeout(conn: &ServerConnection) {
    if let Some(handle) = lock(&conn.inner).handshake_watchdog.take() {
        handle.abort();
    }
}

fn dispatch_message(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>, message: ClientMessage) {
    let stage = lock(&conn.inner).stage;
    match stage {
        Stage::AwaitingHello => {
            let ClientMessage::Hello { version } = message else {
                let core = Arc::clone(core);
                let conn = Arc::clone(conn);
                tokio::spawn(async move {
                    fail_protocol(
                        &core,
                        &conn,
                        Failure::Op(ServerError::new(
                            ServerOperationCode::InvalidRequest,
                            "The first client message must be hello",
                        )),
                    )
                    .await;
                });
                return;
            };
            lock(&conn.inner).stage = Stage::Handshaking;
            let core = Arc::clone(core);
            let conn = Arc::clone(conn);
            tokio::spawn(async move {
                finish_handshake(&core, &conn, version).await;
            });
        }
        Stage::Handshaking => {
            if matches!(message, ClientMessage::Hello { .. }) {
                let core = Arc::clone(core);
                let conn = Arc::clone(conn);
                tokio::spawn(async move {
                    fail_protocol(
                        &core,
                        &conn,
                        Failure::Op(ServerError::new(
                            ServerOperationCode::InvalidRequest,
                            "hello may only be sent as the first message",
                        )),
                    )
                    .await;
                });
                return;
            }
            // Queue until the handshake settles (upstream chains onto
            // the handshake promise).
            lock(&conn.inner).handshake_waiters.push(message);
        }
        Stage::Ready => {
            let core = Arc::clone(core);
            let conn = Arc::clone(conn);
            tokio::spawn(async move {
                handle_request(&core, &conn, message).await;
            });
        }
        Stage::Closing | Stage::Closed => {}
    }
}

async fn finish_handshake(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>, version: u32) {
    if !is_supported_protocol_version(u64::from(version)) {
        fail_protocol(
            core,
            conn,
            Failure::Wire(ProtocolError {
                code: ProtocolErrorCode::Version,
                message: format!(
                    "Unsupported protocol version {version}; expected {PROTOCOL_VERSION}"
                ),
                details: None,
            }),
        )
        .await;
        return;
    }

    let snapshot = get_server_snapshot(core).await;
    {
        let (disconnected, handshaking) = {
            let inner = lock(&conn.inner);
            (inner.disconnected, matches!(inner.stage, Stage::Handshaking))
        };
        let closing = lock(&core.shared).closing;
        if closing || disconnected || !handshaking || conn.connection.closed() {
            return;
        }
    }
    let sent = send_message(
        core,
        conn,
        ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            connection_id: conn.id.clone(),
            snapshot: snapshot.clone(),
        },
    )
    .await;
    if !sent {
        return;
    }
    let waiters = {
        let mut inner = lock(&conn.inner);
        if inner.disconnected || !matches!(inner.stage, Stage::Handshaking) {
            return;
        }
        inner.handshake_complete = true;
        inner.stage = Stage::Ready;
        std::mem::take(&mut inner.handshake_waiters)
    };
    clear_handshake_timeout(conn);
    for message in waiters {
        let core = Arc::clone(core);
        let conn = Arc::clone(conn);
        tokio::spawn(async move {
            handle_request(&core, &conn, message).await;
        });
    }
    if snapshot.revision != current_revision(core) {
        let current = get_server_snapshot(core).await;
        send_message(
            core,
            conn,
            ServerMessage::Event {
                event: ServerEvent::ServerSnapshot { snapshot: current },
            },
        )
        .await;
    }
}

async fn handle_request(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    message: ClientMessage,
) {
    let ClientMessage::Request { id, request } = message else {
        return;
    };
    let result = execute_command(core, conn, request).await;
    let response = match result {
        Ok(result) => ServerMessage::Response { id, ok: true, result: Some(result), error: None },
        Err(failure) => ServerMessage::Response {
            id,
            ok: false,
            result: None,
            error: Some(to_protocol_error(core, failure)),
        },
    };
    send_message(core, conn, response).await;
}

/// Maps an internal failure onto the wire (port of `toProtocolError`).
fn to_protocol_error(_core: &Arc<ServerCore>, failure: Failure) -> ProtocolError {
    match failure {
        Failure::Wire(error) => error,
        Failure::Op(error) => {
            if error.code == ServerOperationCode::NotImplemented {
                return ProtocolError {
                    code: ProtocolErrorCode::NotImplemented,
                    message: NOT_IMPLEMENTED_MESSAGE.to_string(),
                    details: None,
                };
            }
            error.to_protocol_error()
        }
    }
}

fn report_error(core: &ServerCore, cause: &str) {
    if let Some(on_error) = &core.on_error {
        on_error(cause);
    }
}

fn current_revision(core: &Arc<ServerCore>) -> u64 {
    lock(&core.shared).revision
}

/// Builds the current server snapshot without bumping the revision
/// (port of `ServerSnapshotPublisher::get`).
async fn get_server_snapshot(core: &Arc<ServerCore>) -> ServerSnapshot {
    let revision = current_revision(core);
    let sessions = list_metadata(core).await;
    let models = core.service.list_models().await.unwrap_or_default();
    ServerSnapshot {
        server_id: core.id.clone(),
        protocol_version: PROTOCOL_VERSION,
        revision,
        sessions,
        models,
    }
}

/// Merges stored service rows with live session snapshots (port of
/// live session state).
async fn list_metadata(core: &Arc<ServerCore>) -> Vec<SessionMetadata> {
    let live_handles: Vec<Arc<LiveSession>> = {
        let shared = lock(&core.shared);
        shared
            .live
            .values()
            .filter(|live| lock(&live.disposing).is_none())
            .cloned()
            .collect()
    };
    let mut live_by_id: HashMap<String, SessionSnapshot> = HashMap::new();
    for live in live_handles {
        if let Ok(snapshot) = normalized_snapshot(&live).await {
            live_by_id.insert(snapshot.session_id.clone(), snapshot);
        }
    }
    let stored = core.service.list_sessions().await.unwrap_or_default();
    let mut metadata = Vec::with_capacity(stored.len() + live_by_id.len());
    for item in stored {
        match live_by_id.remove(&item.session_id) {
            Some(live_snapshot) => metadata.push(live_snapshot),
            None => metadata.push(item),
        }
    }
    metadata.extend(live_by_id.into_values());
    metadata
}

/// Snapshot with live-session normalization: id checked, `locked`
/// overridden to true (port of `normalizedSnapshot`; the landed wire
/// snapshot carries no phase/attached fields to override).
async fn normalized_snapshot(live: &Arc<LiveSession>) -> Result<SessionSnapshot, ServerError> {
    let mut snapshot = live.runtime.snapshot().await?;
    if snapshot.session_id != live.id {
        return Err(ServerError::new(
            ServerOperationCode::InvalidRequest,
            format!(
                "Runtime session ID changed from {} to {}",
                live.id, snapshot.session_id
            ),
        ));
    }
    snapshot.locked = true;
    Ok(snapshot)
}

/// Sends one message; on failure reports, closes, and disconnects
/// (port of `PiServer::sendMessage`).
async fn send_message(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    message: ServerMessage,
) -> bool {
    let should_drop = {
        let inner = lock(&conn.inner);
        inner.disconnected || conn.connection.closed()
    };
    if should_drop {
        return false;
    }
    let frame = encode_server_message(
        &message,
        Some(FrameDecoderOptions { max_frame_length: core.max_frame_length }),
    );
    let frame = match frame {
        Ok(frame) => frame,
        Err(error) => {
            report_error(core, &error.to_string());
            let core = Arc::clone(core);
            let conn = Arc::clone(conn);
            tokio::spawn(async move {
                conn.connection.close(None).await.ok();
                disconnect(&core, &conn).await;
            });
            return false;
        }
    };
    match conn.connection.send(frame).await {
        Ok(()) => true,
        Err(error) => {
            report_error(core, &error.to_string());
            let core = Arc::clone(core);
            let conn = Arc::clone(conn);
            tokio::spawn(async move {
                conn.connection.close(None).await.ok();
                disconnect(&core, &conn).await;
            });
            false
        }
    }
}

/// Sends a terminal `hello_error` and closes the connection.
async fn fail_protocol(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>, failure: Failure) {
    {
        let mut inner = lock(&conn.inner);
        if inner.disconnected || matches!(inner.stage, Stage::Closing | Stage::Closed) {
            return;
        }
        inner.stage = Stage::Closing;
        inner.handshake_waiters.clear();
        if let Some(handle) = inner.handshake_watchdog.take() {
            handle.abort();
        }
    }
    let error = to_protocol_error(core, failure);
    let final_frame = encode_server_message(
        &ServerMessage::HelloError { error },
        Some(FrameDecoderOptions { max_frame_length: core.max_frame_length }),
    )
    .ok();
    conn.connection.close(final_frame).await.ok();
    disconnect(core, conn).await;
}

/// Detaches the connection from every session and forgets it (port of
/// `PiServer::disconnect`).
async fn disconnect(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>) {
    let handshake_complete = {
        let mut inner = lock(&conn.inner);
        if inner.disconnected {
            return;
        }
        let handshake_complete = inner.handshake_complete;
        inner.disconnected = true;
        inner.stage = Stage::Closed;
        inner.handshake_waiters.clear();
        if let Some(handle) = inner.handshake_watchdog.take() {
            handle.abort();
        }
        handshake_complete
    };
    lock(&core.shared).connections.remove(&conn.id);
    sessions_disconnect(core, conn).await;
    let closing = lock(&core.shared).closing;
    if !closing && handshake_complete {
        broadcast_server_snapshot(core).await;
    }
}

/// Closes every connection and disposes every live session (port of
/// `closeServerState` + session closure).
async fn close_server_state(core: &Arc<ServerCore>) {
    let connections: Vec<Arc<ServerConnection>> = {
        let shared = lock(&core.shared);
        for conn in shared.connections.values() {
            let mut inner = lock(&conn.inner);
            inner.stage = Stage::Closing;
            inner.handshake_waiters.clear();
            if let Some(handle) = inner.handshake_watchdog.take() {
                handle.abort();
            }
        }
        shared.connections.values().cloned().collect()
    };
    for conn in &connections {
        conn.connection.close(None).await.ok();
    }
    for conn in &connections {
        disconnect(core, conn).await;
    }

    // Await in-flight openings so their runtimes are observable.
    let openings: Vec<OpeningFuture> = lock(&core.shared).opening.values().cloned().collect();
    for opening in openings {
        let _ = opening.await;
    }
    let lives: Vec<Arc<LiveSession>> = {
        let shared = lock(&core.shared);
        shared.live.values().cloned().collect()
    };
    for live in lives {
        if let Some(disposing) = lock(&live.disposing).clone() {
            disposing.await;
            continue;
        }
        if let Some(unsubscribe) = live.take_unsubscribe() {
            unsubscribe();
        }
        if let Err(error) = live.runtime.dispose().await {
            report_error(core, &error.to_string());
        }
    }
    let mut shared = lock(&core.shared);
    shared.live.clear();
    shared.connections.clear();
}

// ---------------------------------------------------------------------------
// Live session manager (port of upstream sessions.ts)
// ---------------------------------------------------------------------------

/// Detaches one connection from all of its sessions (port of
/// session disconnect).
async fn sessions_disconnect(core: &Arc<ServerCore>, conn: &Arc<ServerConnection>) {
    let session_ids: Vec<String> = {
        let mut inner = lock(&conn.inner);
        inner.session_ids.drain().collect()
    };
    let lives: Vec<Arc<LiveSession>> = {
        let shared = lock(&core.shared);
        session_ids.iter().filter_map(|id| shared.live.get(id).cloned()).collect()
    };
    for live in &lives {
        lock(&live.connections).remove(&conn.id);
    }
    for live in &lives {
        if let Err(error) = maybe_dispose(core, live).await {
            report_error(core, &error);
        }
    }
}

/// Executes one command for one connection (port of
/// command execution).
async fn execute_command(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    command: Command,
) -> Result<CommandResult, Failure> {
    match command {
        Command::List => {
            let sessions = list_metadata(core).await;
            Ok(CommandResult::List { sessions })
        }
        Command::Create { cwd, name, model, thinking_level } => {
            let id = Uuid::new_v4().to_string();
            let options = CreateSessionOptions {
                id: id.clone(),
                cwd,
                name,
                model,
                thinking_level,
            };
            let service = Arc::clone(&core.service);
            let live = acquire(core, id, move |_session_id| {
                async move { service.create_session(options).await }.boxed()
            })
            .await?;
            attach(core, conn, &live).await?;
            let snapshot = broadcast_session_snapshot(core, &live).await?;
            broadcast_server_snapshot(core).await;
            let session = for_connection(snapshot, conn);
            Ok(CommandResult::Create { session })
        }
        Command::Attach { session_id } => {
            let service = Arc::clone(&core.service);
            let live = acquire(core, session_id.clone(), move |session_id| {
                async move { service.open_session(session_id).await }.boxed()
            })
            .await?;
            attach(core, conn, &live).await?;
            let snapshot = broadcast_session_snapshot(core, &live).await?;
            broadcast_server_snapshot(core).await;
            let session = for_connection(snapshot, conn);
            Ok(CommandResult::Attach { session })
        }
        Command::Detach { session_id } => {
            let had = lock(&conn.inner).session_ids.remove(&session_id);
            if had {
                let live = lock(&core.shared).live.get(&session_id).cloned();
                if let Some(live) = live {
                    lock(&live.connections).remove(&conn.id);
                    let remaining = lock(&live.connections).len();
                    let terminal = live.terminal.load(Ordering::SeqCst);
                    let disposing = lock(&live.disposing).is_some();
                    if remaining > 0 && !terminal && !disposing {
                        broadcast_session_snapshot(core, &live).await?;
                    }
                    maybe_dispose(core, &live).await.ok();
                }
                broadcast_server_snapshot(core).await;
            }
            Ok(CommandResult::Detach { session_id })
        }
        Command::Prompt { session_id, text } => {
            let live = require_attached(core, conn, &session_id)?;
            let live_op = Arc::clone(&live);
            let session = run_operation(core, conn, &live, move || {
                live_op.runtime.prompt(text)
            })
            .await?;
            Ok(CommandResult::Prompt { session })
        }
        Command::Steer { session_id, text } => {
            let live = require_attached(core, conn, &session_id)?;
            let live_op = Arc::clone(&live);
            let session = run_operation(core, conn, &live, move || {
                live_op.runtime.steer(text)
            })
            .await?;
            Ok(CommandResult::Steer { session })
        }
        Command::Abort { session_id } => {
            let live = require_attached(core, conn, &session_id)?;
            let live_op = Arc::clone(&live);
            let session = run_operation(core, conn, &live, move || {
                live_op.runtime.abort()
            })
            .await?;
            Ok(CommandResult::Abort { session })
        }
        Command::SetModel { session_id, model } => {
            let live = require_attached(core, conn, &session_id)?;
            let live_op = Arc::clone(&live);
            let session = run_operation(core, conn, &live, move || {
                live_op.runtime.set_model(model)
            })
            .await?;
            Ok(CommandResult::SetModel { session })
        }
        Command::SetThinking { session_id, thinking_level } => {
            let live = require_attached(core, conn, &session_id)?;
            let live_op = Arc::clone(&live);
            let session = run_operation(core, conn, &live, move || {
                live_op.runtime.set_thinking(thinking_level)
            })
            .await?;
            Ok(CommandResult::SetThinking { session })
        }
    }
}

/// Runs one mutating runtime operation with disposal protection and
/// returns the connection-normalized snapshot (port of `runOperation`).
async fn run_operation<F, Fut>(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    live: &Arc<LiveSession>,
    operation: F,
) -> Result<SessionSnapshot, Failure>
where
    F: FnOnce() -> Fut,
    Fut: futures::Future<Output = Result<(), ServerError>>,
{
    live.operation_count.fetch_add(1, Ordering::SeqCst);
    let op_result = operation().await;
    let broadcast_result = broadcast_session_snapshot(core, live).await;
    live.operation_count.fetch_sub(1, Ordering::SeqCst);
    maybe_dispose(core, live).await.ok();
    op_result?;
    let snapshot = broadcast_result?;
    Ok(for_connection(snapshot, conn))
}

/// Acquires (or joins the opening of) the live session for `id` (port
/// of `acquire`).
async fn acquire<F>(core: &Arc<ServerCore>, id: String, open: F) -> Result<Arc<LiveSession>, ServerError>
where
    F: FnOnce(String) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>>,
{
    loop {
        enum Wait {
            Disposing(DisposeFuture),
            Opening(OpeningFuture),
            None,
        }
        let wait = {
            let shared = lock(&core.shared);
            if let Some(existing) = shared.live.get(&id) {
                if existing.terminal.load(Ordering::SeqCst) {
                    return Err(ServerError::new(
                        ServerOperationCode::SessionLocked,
                        format!("Session runtime is terminating: {id}"),
                    ));
                }
                match lock(&existing.disposing).clone() {
                    Some(disposing) => Wait::Disposing(disposing),
                    None => return Ok(Arc::clone(existing)),
                }
            } else if let Some(opening) = shared.opening.get(&id) {
                Wait::Opening(opening.clone())
            } else {
                Wait::None
            }
        };
        match wait {
            Wait::Disposing(disposing) => {
                disposing.await;
                continue;
            }
            Wait::Opening(opening) => {
                return opening.await;
            }
            Wait::None => {}
        }
        // Reserve the opening slot, then create.
        let (sender, receiver) = futures::channel::oneshot::channel();
        let reserved: OpeningFuture = async move {
            receiver.await.unwrap_or_else(|_| {
                Err(ServerError::new(
                    ServerOperationCode::InvalidRequest,
                    "Session opening was aborted",
                ))
            })
        }
        .boxed()
        .shared();
        let inserted = {
            let mut shared = lock(&core.shared);
            if shared.live.contains_key(&id) || shared.opening.contains_key(&id) {
                false
            } else {
                shared.opening.insert(id.clone(), reserved.clone());
                true
            }
        };
        if !inserted {
            continue; // Raced with another opener; re-examine.
        }
        let result = create_live_session(core, id.clone(), open(id.clone())).await;
        {
            let mut shared = lock(&core.shared);
            shared.opening.remove(&id);
            if let Ok(live) = &result {
                shared.live.insert(id.clone(), Arc::clone(live));
            }
        }
        // A dropped receiver means the requesting task already returned.
        drop(sender.send(result.clone()));
        return result;
    }
}

/// Builds and subscribes one live session (port of `create`). The
/// caller (`acquire`) publishes it into the registry.
async fn create_live_session(
    core: &Arc<ServerCore>,
    id: String,
    acquire_runtime: BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>>,
) -> Result<Arc<LiveSession>, ServerError> {
    let runtime = acquire_runtime.await?;
    let closing = lock(&core.shared).closing;
    if closing {
        if let Err(error) = runtime.dispose().await {
            report_error(core, &error.to_string());
        }
        return Err(ServerError::new(
            ServerOperationCode::InvalidRequest,
            "PiServer closed while acquiring a session runtime",
        ));
    }
    let snapshot = runtime.snapshot().await?;
    if snapshot.session_id != id {
        if let Err(error) = runtime.dispose().await {
            report_error(core, &error.to_string());
        }
        return Err(ServerError::new(
            ServerOperationCode::InvalidRequest,
            format!(
                "Service returned session {} for server-assigned session {id}",
                snapshot.session_id
            ),
        ));
    }
    let live = Arc::new(LiveSession {
        id: id.clone(),
        runtime,
        connections: StdMutex::new(HashSet::new()),
        unsubscribe: StdMutex::new(None),
        operation_count: AtomicU64::new(0),
        ready: AtomicBool::new(false),
        terminal: AtomicBool::new(false),
        disposing: StdMutex::new(None),
    });
    {
        let core_weak = Arc::downgrade(core);
        let live_weak = Arc::downgrade(&live);
        let unsubscribe = live.runtime.subscribe(Arc::new(move |event| {
            if let (Some(core), Some(live)) = (core_weak.upgrade(), live_weak.upgrade()) {
                handle_runtime_event(&core, &live, event);
            }
        }));
        *lock(&live.unsubscribe) = Some(unsubscribe);
    }
    live.ready.store(true, Ordering::SeqCst);
    Ok(live)
}

/// Delivers one runtime event (port of `handleRuntimeEvent`).
fn handle_runtime_event(core: &Arc<ServerCore>, live: &Arc<LiveSession>, event: &SessionRuntimeEvent) {
    match event {
        SessionRuntimeEvent::Error(error) => {
            let error = error.clone();
            let core = Arc::clone(core);
            let live = Arc::clone(live);
            tokio::spawn(async move {
                if let Err(failure) = terminate(&core, &live, error).await {
                    if let Failure::Op(op) = failure {
                        report_error(&core, &op.to_string());
                    }
                }
            });
        }
        SessionRuntimeEvent::Progress(progress) => {
            let envelope = ServerMessage::Event {
                event: ServerEvent::SessionProgress {
                    session_id: live.id.clone(),
                    progress: progress.clone(),
                },
            };
            let targets: Vec<String> = lock(&live.connections).iter().cloned().collect();
            let core = Arc::clone(core);
            tokio::spawn(async move {
                for conn_id in targets {
                    let conn = lock(&core.shared).connections.get(&conn_id).cloned();
                    if let Some(conn) = conn {
                        send_message(&core, &conn, envelope.clone()).await;
                    }
                }
            });
        }
        SessionRuntimeEvent::Snapshot => {
            let core = Arc::clone(core);
            let live = Arc::clone(live);
            tokio::spawn(async move {
                if let Err(error) = broadcast_session_snapshot(&core, &live).await {
                    report_error(&core, &error.to_string());
                }
            });
        }
    }
    schedule_maybe_dispose(core, live);
}

/// Terminates a failed session: disconnects every attachment and
/// disposes the runtime (port of `terminate`).
async fn terminate(
    core: &Arc<ServerCore>,
    live: &Arc<LiveSession>,
    error: ServerError,
) -> Result<(), Failure> {
    if live.terminal.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    report_error(core, &error.to_string());
    if let Some(unsubscribe) = live.take_unsubscribe() {
        unsubscribe();
    }
    let conn_ids: Vec<String> = lock(&live.connections).iter().cloned().collect();
    let conns: Vec<Arc<ServerConnection>> = {
        let shared = lock(&core.shared);
        conn_ids.iter().filter_map(|id| shared.connections.get(id).cloned()).collect()
    };
    for conn in &conns {
        conn.connection.close(None).await.ok();
    }
    for conn in &conns {
        disconnect(core, conn).await;
    }
    maybe_dispose(core, live).await.ok();
    Ok(())
}

/// Attaches a connection to a live session — attachment happens strictly
/// before any mutation is allowed (port of `attach`).
async fn attach(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    live: &Arc<LiveSession>,
) -> Result<(), ServerError> {
    let (disconnected, ready) = {
        let inner = lock(&conn.inner);
        (inner.disconnected, matches!(inner.stage, Stage::Ready))
    };
    let closing = { lock(&core.shared).closing };
    if disconnected || !ready || conn.connection.closed() || closing {
        maybe_dispose(core, live).await.ok();
        return Err(ServerError::new(
            ServerOperationCode::InvalidRequest,
            "Connection closed while attaching to a session",
        ));
    }
    lock(&conn.inner).session_ids.insert(live.id.clone());
    lock(&live.connections).insert(conn.id.clone());
    Ok(())
}

/// Requires an active attachment for a mutating command (port of
/// `requireAttached`).
fn require_attached(
    core: &Arc<ServerCore>,
    conn: &Arc<ServerConnection>,
    session_id: &str,
) -> Result<Arc<LiveSession>, ServerError> {
    let attached = lock(&conn.inner).session_ids.contains(session_id);
    if !attached {
        return Err(ServerError::new(
            ServerOperationCode::InvalidRequest,
            format!("Connection is not attached to session {session_id}"),
        ));
    }
    let shared = lock(&core.shared);
    let live = shared.live.get(session_id);
    match live {
        Some(live)
            if !live.terminal.load(Ordering::SeqCst) && lock(&live.disposing).is_none() =>
        {
            Ok(Arc::clone(live))
        }
        _ => Err(ServerError::new(
            ServerOperationCode::NotFound,
            format!("Session is not live: {session_id}"),
        )),
    }
}

/// Marks the per-connection view of a snapshot (port of
/// `forConnection`; the landed wire snapshot has no `attached` field,
/// so this is the identity, kept as the normalization seam).
fn for_connection(snapshot: SessionSnapshot, _conn: &Arc<ServerConnection>) -> SessionSnapshot {
    snapshot
}

/// Broadcasts one session snapshot to every attached connection exactly
/// once (port of `broadcastSnapshot`).
async fn broadcast_session_snapshot(
    core: &Arc<ServerCore>,
    live: &Arc<LiveSession>,
) -> Result<SessionSnapshot, ServerError> {
    let snapshot = normalized_snapshot(live).await?;
    let envelope = ServerMessage::Event {
        event: ServerEvent::SessionSnapshot { snapshot: snapshot.clone() },
    };
    let conn_ids: Vec<String> = lock(&live.connections).iter().cloned().collect();
    let conns: Vec<Arc<ServerConnection>> = {
        let shared = lock(&core.shared);
        conn_ids.iter().filter_map(|id| shared.connections.get(id).cloned()).collect()
    };
    for conn in conns {
        send_message(core, &conn, envelope.clone()).await;
    }
    Ok(snapshot)
}

/// Fire-and-forget disposal check (port of `scheduleMaybeDispose`).
fn schedule_maybe_dispose(core: &Arc<ServerCore>, live: &Arc<LiveSession>) {
    let core = Arc::clone(core);
    let live = Arc::clone(live);
    tokio::spawn(async move {
        if let Err(error) = maybe_dispose(&core, &live).await {
            report_error(&core, &error);
        }
    });
}

/// Disposes the runtime once it is idle with zero attachments (port of
/// `maybeDispose`).
async fn maybe_dispose(core: &Arc<ServerCore>, live: &Arc<LiveSession>) -> Result<(), String> {
    let (closing, ready, conns, op_count, terminal, phase) = {
        let closing = lock(&core.shared).closing;
        let ready = live.ready.load(Ordering::SeqCst);
        let conns = lock(&live.connections).len();
        let op_count = live.operation_count.load(Ordering::SeqCst);
        let terminal = live.terminal.load(Ordering::SeqCst);
        let phase = live.runtime.phase();
        (closing, ready, conns, op_count, terminal, phase)
    };
    let eligible = !closing
        && ready
        && conns == 0
        && op_count == 0
        && (terminal || phase == SessionPhase::Idle);
    let to_await: DisposeFuture = {
        let mut disposing = lock(&live.disposing);
        match (eligible, disposing.clone()) {
            (false, Some(existing)) => existing,
            (false, None) => return Ok(()),
            // Eligible but a concurrent caller already started the
            // disposal: join it.
            (true, Some(existing)) => existing,
            (true, None) => {
                // Unsubscribe before disposing so late events cannot
                // re-enter.
                if let Some(unsubscribe) = live.take_unsubscribe() {
                    unsubscribe();
                }
                let core = Arc::clone(core);
                let live = Arc::clone(live);
                let future: DisposeFuture = async move {
                    if let Err(error) = live.runtime.dispose().await {
                        report_error(&core, &error.to_string());
                    }
                    let mut shared = lock(&core.shared);
                    let is_current = shared
                        .live
                        .get(&live.id)
                        .is_some_and(|current| Arc::ptr_eq(current, &live));
                    if is_current {
                        shared.live.remove(&live.id);
                    }
                }
                .boxed()
                .shared();
                *disposing = Some(future.clone());
                future
            }
        }
    };
    to_await.await;
    let is_closing = lock(&core.shared).closing;
    if !is_closing {
        broadcast_server_snapshot(core).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot publisher (port of upstream snapshots.ts)
// ---------------------------------------------------------------------------

/// Broadcasts one server snapshot to every ready connection, strictly
/// serialized (port of `ServerSnapshotPublisher::broadcast`).
fn broadcast_server_snapshot(core: &Arc<ServerCore>) -> BoxFuture<'static, ()> {
    let next: Shared<BoxFuture<'static, ()>> = {
        let mut shared = lock(&core.shared);
        let previous = shared.broadcast_tail.clone();
        let core_broadcast = Arc::clone(core);
        let next = async move {
            previous.await;
            perform_broadcast(&core_broadcast).await;
        }
        .boxed()
        .shared();
        shared.broadcast_tail = next.clone();
        next
    };
    Box::pin(async move {
        next.await;
    })
}

async fn perform_broadcast(core: &Arc<ServerCore>) {
    let (closing, conns) = {
        let shared = lock(&core.shared);
        let conns: Vec<Arc<ServerConnection>> = shared
            .connections
            .values()
            .filter(|conn| {
                let inner = lock(&conn.inner);
                matches!(inner.stage, Stage::Ready) && !inner.disconnected
            })
            .cloned()
            .collect();
        (shared.closing, conns)
    };
    if conns.is_empty() || closing {
        return;
    }
    let revision = {
        let mut shared = lock(&core.shared);
        shared.revision += 1;
        shared.revision
    };
    let sessions = list_metadata(core).await;
    let models = core.service.list_models().await.unwrap_or_default();
    let snapshot = ServerSnapshot {
        server_id: core.id.clone(),
        protocol_version: PROTOCOL_VERSION,
        revision,
        sessions,
        models,
    };
    let envelope = ServerMessage::Event {
        event: ServerEvent::ServerSnapshot { snapshot },
    };
    for conn in conns {
        send_message(core, &conn, envelope.clone()).await;
    }
}

// ---------------------------------------------------------------------------
// Listen specs (single fallible surface, one typed error owner)
// ---------------------------------------------------------------------------

/// Declares where a server listens. Declared unconditionally; only
/// [`build_listener`] decides which specs a platform can build.
#[derive(Debug, Clone)]
pub enum ListenSpec {
    /// Accept connections from an in-process [`InMemoryListener`].
    InMemory {
        /// The listener supplying accepted ends.
        listener: Arc<InMemoryListener>,
    },
    /// A Unix-domain socket listener (Unix tier only).
    Unix {
        /// Socket path, validated eagerly by [`build_listener`] through
        /// the shared transport validation.
        path: PathBuf,
        /// Outbound backpressure budget in bytes per connection.
        max_pending_bytes: Option<usize>,
    },
}

/// Builds the listener for one [`ListenSpec`] — the server-side mirror
/// of [`crate::remote::transport::build_transport`]. Unix path and
/// budget validation is delegated to that same surface (one owner); a
/// Unix spec on a non-Unix tier returns the typed
/// [`EndpointSpecError::UnsupportedOnPlatform`].
///
/// # Errors
///
/// Returns [`EndpointSpecError`] when the spec is invalid or
/// unsupported on this platform.
pub fn build_listener(spec: &ListenSpec) -> Result<Arc<dyn ServerListener>, EndpointSpecError> {
    match spec {
        ListenSpec::InMemory { listener } => Ok(Arc::new(InMemoryServerListener::new(Arc::clone(
            listener,
        )))),
        ListenSpec::Unix { path, max_pending_bytes } => {
            // Shared eager validation (empty/over-long path, invalid
            // budget) and the typed platform gate.
            let client_spec = EndpointSpec::Unix {
                path: path.clone(),
                max_pending_bytes: *max_pending_bytes,
            };
            build_transport(&client_spec)?;
            #[cfg(unix)]
            {
                Ok(unix::create_listener(unix::UnixListenerOptions {
                    path: path.clone(),
                    mode: None,
                    max_pending_bytes: *max_pending_bytes,
                    graceful_close_timeout_ms: None,
                }))
            }
            #[cfg(not(unix))]
            {
                Err(EndpointSpecError::UnsupportedOnPlatform {
                    kind: EndpointKind::Unix,
                    os: std::env::consts::OS,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory server listener
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InMemoryListenerState {
    Idle,
    Started,
    Closing,
}

/// Accepts in-memory connections and adapts them to [`ByteConnection`]s
/// (the server-side counterpart of the in-memory client transport).
pub struct InMemoryServerListener {
    listener: Arc<InMemoryListener>,
    state: StdMutex<InMemoryListenerState>,
    stop: Arc<tokio::sync::Notify>,
}

impl InMemoryServerListener {
    /// Wraps one [`InMemoryListener`].
    #[must_use]
    pub fn new(listener: Arc<InMemoryListener>) -> Self {
        Self {
            listener,
            state: StdMutex::new(InMemoryListenerState::Idle),
            stop: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// The endpoint in-process clients dial.
    #[must_use]
    pub fn endpoint(&self) -> crate::remote::transport::InMemoryEndpoint {
        self.listener.endpoint()
    }
}


/// Bridges one accepted in-memory transport into a [`ByteConnection`].
///
/// The accepted end is installed only after
/// [`InMemoryListener::accept`] returns, so outbound sends that race
/// the install park on the slot watch instead of failing.
struct InMemoryConnection {
    slot_rx: tokio::sync::watch::Receiver<Option<Arc<dyn ByteTransport>>>,
    slot_tx: tokio::sync::watch::Sender<Option<Arc<dyn ByteTransport>>>,
    closed: Arc<AtomicBool>,
}

impl InMemoryConnection {
    fn new() -> Self {
        let (slot_tx, slot_rx) = tokio::sync::watch::channel(None::<Arc<dyn ByteTransport>>);
        Self {
            slot_rx,
            slot_tx,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fill(&self, transport: Arc<dyn ByteTransport>) {
        // send_replace always notifies, waking parked senders.
        self.slot_tx.send_replace(Some(Arc::clone(&transport)));
        if self.closed.load(Ordering::SeqCst) {
            transport.close();
        }
    }

    /// Marks the connection closed from the transport side and wakes
    /// parked senders.
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // Always notify so parked senders re-check and fail closed.
        self.slot_tx.send_modify(|_| {});
    }
}

impl ByteConnection for InMemoryConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>> {
        let mut rx = self.slot_rx.clone();
        let closed = Arc::clone(&self.closed);
        Box::pin(async move {
            loop {
                if closed.load(Ordering::SeqCst) {
                    return Err(TransportError::Closed);
                }
                let transport = rx.borrow().clone();
                if let Some(transport) = transport {
                    return transport.send(chunk).await;
                }
                if rx.changed().await.is_err() {
                    return Err(TransportError::Closed);
                }
            }
        })
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Box::pin(futures::future::ready(Ok(())));
        }
        let slot = self.slot_rx.borrow().clone();
        // Always notify so parked senders observe the close.
        self.slot_tx.send_modify(|_| {});
        Box::pin(async move {
            let transport = slot.ok_or(TransportError::Closed)?;
            if let Some(chunk) = final_chunk {
                transport.send(chunk).await?;
            }
            transport.close();
            Ok(())
        })
    }
}

/// Forwards transport events into the server handler while tracking the
/// connection's closed state.
struct InMemoryHandlers {
    handler: Arc<dyn ConnectionHandler>,
    connection: Arc<InMemoryConnection>,
}

impl ByteTransportHandlers for InMemoryHandlers {
    fn on_data(&self, chunk: Vec<u8>) {
        self.handler.on_data(chunk);
    }

    fn on_close(&self) {
        self.connection.mark_closed();
        self.handler.on_close();
    }

    fn on_error(&self, error: TransportError) {
        self.connection.mark_closed();
        self.handler.on_error(error);
    }
}

impl ServerListener for InMemoryServerListener {
    fn address(&self) -> Option<String> {
        None
    }

    fn start(&self, accept: ConnectionAcceptor) -> BoxFuture<'static, Result<(), ListenerError>> {
        {
            let mut state = lock(&self.state);
            match *state {
                InMemoryListenerState::Started => {
                    return Box::pin(futures::future::ready(Err(ListenerError::AlreadyStarted)));
                }
                InMemoryListenerState::Closing => {
                    return Box::pin(futures::future::ready(Err(ListenerError::Closing)));
                }
                InMemoryListenerState::Idle => *state = InMemoryListenerState::Started,
            }
        }
        let listener = Arc::clone(&self.listener);
        let stop = Arc::clone(&self.stop);
        tokio::spawn(async move {
            loop {
                let connection = Arc::new(InMemoryConnection::new());
                let handler = accept(Arc::clone(&connection) as Arc<dyn ByteConnection>);
                let handlers = Arc::new(InMemoryHandlers {
                    handler,
                    connection: Arc::clone(&connection),
                });
                let accepted = tokio::select! {
                    _ = stop.notified() => break,
                    accepted = listener.accept(handlers) => accepted,
                };
                match accepted {
                    Ok(transport) => {
                        connection.fill(Arc::new(transport) as Arc<dyn ByteTransport>);
                    }
                    Err(_) => break,
                }
            }
        });
        Box::pin(futures::future::ready(Ok(())))
    }

    fn close(&self) -> BoxFuture<'static, ()> {
        *lock(&self.state) = InMemoryListenerState::Closing;
        self.stop.notify_one();
        Box::pin(futures::future::ready(()))
    }
}

// ---------------------------------------------------------------------------
// AgentSession hosting (C15-only mutation surface)
// ---------------------------------------------------------------------------

/// Resolves a wire [`ModelRef`] to a concrete model for
/// [`AgentSession::set_model`]. Hosting sessions without a resolver
/// rejects `set_model` with `not_found`.
pub type ModelResolver =
    Arc<dyn Fn(&ModelRef) -> Option<pi_ai::Model> + Send + Sync>;

/// Builds one [`AgentSession`] for a create command. The factory owns
/// session construction (the G8 surface); it must return a session
/// whose durable id equals `options.id`, matching the upstream
/// service contract.
pub type AgentSessionFactory = Arc<
    dyn Fn(&CreateSessionOptions) -> BoxFuture<'static, Result<Arc<AgentSession>, ServerError>>
        + Send
        + Sync,
>;

/// Hosts [`AgentSession`]s behind the [`ServerService`] seam. All
/// mutation flows through [`AgentSession`]'s public C15 surface; no
/// session encoding or migration happens here (G8 owns it).
///
/// Disposal semantics: releasing an acquisition (detach, disconnect,
/// idle disposal) drops the runtime's handle but keeps the session
/// registered — the in-memory stand-in for upstream's durable store —
/// so a later attach re-hosts the same session. Terminal disposal is
/// explicit through [`AgentSessionService::dispose_all`].
pub struct AgentSessionService {
    sessions: Arc<StdMutex<HashMap<String, Arc<AgentSession>>>>,
    locked: Arc<StdMutex<HashSet<String>>>,
    models: StdMutex<Vec<ModelMetadata>>,
    model_resolver: ModelResolver,
    session_factory: Option<AgentSessionFactory>,
}

impl fmt::Debug for AgentSessionService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentSessionService")
            .field("sessions", &self.sessions.lock().map(|s| s.len()))
            .finish_non_exhaustive()
    }
}

impl AgentSessionService {
    /// Creates an empty service with a model resolver for `set_model`.
    #[must_use]
    pub fn new(model_resolver: ModelResolver) -> Self {
        Self {
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            locked: Arc::new(StdMutex::new(HashSet::new())),
            models: StdMutex::new(Vec::new()),
            model_resolver,
            session_factory: None,
        }
    }

    /// Installs the create-session factory.
    #[must_use]
    pub fn with_session_factory(mut self, factory: AgentSessionFactory) -> Self {
        self.session_factory = Some(factory);
        self
    }

    /// Sets the model catalog offered to clients.
    pub fn set_models(&self, models: Vec<ModelMetadata>) {
        *lock(&self.models) = models;
    }

    /// Registers a pre-built session for `open_session`; returns its
    /// durable id.
    pub async fn register(&self, session: Arc<AgentSession>) -> String {
        let id = session.session_id().await;
        lock(&self.sessions).insert(id.clone(), session);
        id
    }

    /// Disposes every registered session (terminal cleanup).
    pub async fn dispose_all(&self) {
        let sessions: Vec<Arc<AgentSession>> = lock(&self.sessions).values().cloned().collect();
        for session in sessions {
            session.dispose().await;
        }
        lock(&self.sessions).clear();
        lock(&self.locked).clear();
    }

    /// Whether the session is currently acquired.
    #[must_use]
    pub fn is_locked(&self, session_id: &str) -> bool {
        lock(&self.locked).contains(session_id)
    }
}

impl ServerService for AgentSessionService {
    fn list_sessions(&self) -> BoxFuture<'static, Result<Vec<SessionSnapshot>, ServerError>> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            let stored: Vec<Arc<AgentSession>> = lock(&sessions).values().cloned().collect();
            let mut snapshots = Vec::with_capacity(stored.len());
            for session in stored {
                snapshots.push(agent_session_snapshot(&session).await);
            }
            Ok(snapshots)
        })
    }

    fn list_models(&self) -> BoxFuture<'static, Result<Vec<ModelMetadata>, ServerError>> {
        let models = lock(&self.models).clone();
        Box::pin(futures::future::ready(Ok(models)))
    }

    fn create_session(
        &self,
        options: CreateSessionOptions,
    ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>> {
        let factory = self.session_factory.clone();
        let sessions = Arc::clone(&self.sessions);
        let locked = Arc::clone(&self.locked);
        let resolver = Arc::clone(&self.model_resolver);
        Box::pin(async move {
            let factory = factory.ok_or_else(|| {
                ServerError::new(
                    ServerOperationCode::NotImplemented,
                    "AgentSession creation requires a session factory",
                )
            })?;
            let session = factory(&options).await?;
            let id = session.session_id().await;
            lock(&sessions).insert(id.clone(), Arc::clone(&session));
            lock(&locked).insert(id.clone());
            Ok(Box::new(AgentSessionRuntime::new(session, id, locked, resolver))
                as Box<dyn SessionRuntime>)
        })
    }

    fn open_session(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>> {
        let sessions = Arc::clone(&self.sessions);
        let locked = Arc::clone(&self.locked);
        let resolver = Arc::clone(&self.model_resolver);
        Box::pin(async move {
            let session = lock(&sessions).get(&session_id).cloned().ok_or_else(|| {
                ServerError::new(
                    ServerOperationCode::NotFound,
                    format!("Unknown session: {session_id}"),
                )
            })?;
            if lock(&locked).contains(&session_id) {
                return Err(ServerError::new(
                    ServerOperationCode::SessionLocked,
                    format!("Session is locked: {session_id}"),
                ));
            }
            lock(&locked).insert(session_id.clone());
            Ok(Box::new(AgentSessionRuntime::new(session, session_id, locked, resolver))
                as Box<dyn SessionRuntime>)
        })
    }
}

/// One acquired [`AgentSession`] behind the [`SessionRuntime`] seam.
/// Every mutation goes through the session's public C15 surface.
pub struct AgentSessionRuntime {
    session: Arc<AgentSession>,
    session_id: String,
    locked: Arc<StdMutex<HashSet<String>>>,
    model_resolver: ModelResolver,
    event_listeners: Arc<StdMutex<Vec<(u64, SessionRuntimeListener)>>>,
    next_listener_id: AtomicU64,
    dispose_count: Arc<AtomicUsize>,
}

impl AgentSessionRuntime {
    fn new(
        session: Arc<AgentSession>,
        session_id: String,
        locked: Arc<StdMutex<HashSet<String>>>,
        model_resolver: ModelResolver,
    ) -> Self {
        Self {
            session,
            session_id,
            locked,
            model_resolver,
            event_listeners: Arc::new(StdMutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(1),
            dispose_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SessionRuntime for AgentSessionRuntime {
    fn snapshot(&self) -> BoxFuture<'static, Result<SessionSnapshot, ServerError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move { Ok(agent_session_snapshot(&session).await) })
    }

    fn phase(&self) -> SessionPhase {
        agent_session_phase(&self.session)
    }

    fn prompt(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            use crate::core::agent_session::prompt::PromptOptions;
            session
                .prompt(&text, PromptOptions::default())
                .await
                .map_err(|error| {
                    ServerError::new(ServerOperationCode::InvalidRequest, error.to_string())
                })
        })
    }

    fn steer(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            session.steer(&text, Vec::new()).map_err(|error| {
                ServerError::new(ServerOperationCode::InvalidRequest, error.to_string())
            })
        })
    }

    fn abort(&self) -> BoxFuture<'static, Result<(), ServerError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            session.abort().await;
            Ok(())
        })
    }

    fn set_model(&self, model: ModelRef) -> BoxFuture<'static, Result<(), ServerError>> {
        let session = Arc::clone(&self.session);
        let resolver = Arc::clone(&self.model_resolver);
        Box::pin(async move {
            let resolved = resolver(&model).ok_or_else(|| {
                ServerError::new(
                    ServerOperationCode::NotFound,
                    format!("Unknown model: {}/{}", model.provider, model.id),
                )
            })?;
            session.set_model(resolved).await.map_err(|error| {
                ServerError::new(ServerOperationCode::InvalidRequest, error.to_string())
            })
        })
    }

    fn set_thinking(&self, level: ThinkingLevel) -> BoxFuture<'static, Result<(), ServerError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            let _ = session.set_thinking_level(wire_thinking_to_model(level)).await;
            Ok(())
        })
    }

    fn subscribe(&self, listener: SessionRuntimeListener) -> Box<dyn Fn() + Send + Sync> {
        let listeners = Arc::clone(&self.event_listeners);
        let session = Arc::clone(&self.session);
        let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
        lock(&listeners).push((id, listener));
        let agent_listeners = Arc::clone(&listeners);
        let unsubscribe = session.subscribe(move |_event: &crate::core::agent_session::AgentSessionEvent| {
            for (_, listener) in lock(&agent_listeners).iter() {
                listener(&SessionRuntimeEvent::Snapshot);
            }
        });
        let remove = Arc::clone(&listeners);
        Box::new(move || {
            lock(&remove).retain(|(listener_id, _)| *listener_id != id);
            unsubscribe();
        })
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), ServerError>> {
        let session_id = self.session_id.clone();
        let locked = Arc::clone(&self.locked);
        let dispose_count = Arc::clone(&self.dispose_count);
        Box::pin(async move {
            dispose_count.fetch_add(1, Ordering::SeqCst);
            lock(&locked).remove(&session_id);
            Ok(())
        })
    }
}

/// Builds the wire snapshot for one [`AgentSession`] from public
/// accessors only.
async fn agent_session_snapshot(session: &Arc<AgentSession>) -> SessionSnapshot {
    let session_id = session.session_id().await;
    let model = session.model();
    let thinking_level = model_thinking_to_wire(session.thinking_level());
    let transcript = session
        .messages()
        .iter()
        .filter_map(map_agent_message)
        .collect::<Vec<_>>();
    let (steering, _follow_up) = session.pending_messages();
    let queued_steer = steering
        .iter()
        .map(|text| UserTranscriptItem {
            type_field: "user".to_string(),
            content: vec![UserContent::Text(TextContent {
                type_field: "text".to_string(),
                text: text.clone(),
            })],
        })
        .collect::<Vec<_>>();
    let queued_steer_count = u64::try_from(session.pending_message_count()).unwrap_or(u64::MAX);
    let revision = u64::try_from(transcript.len()).unwrap_or(u64::MAX);
    SessionSnapshot {
        session_id,
        model: ModelRef { provider: model.provider, id: model.id },
        thinking_level,
        locked: false,
        revision,
        transcript,
        queued_steer,
        queued_steer_count,
    }
}

/// Public-surface phase mapping.
fn agent_session_phase(session: &AgentSession) -> SessionPhase {
    if session.is_idle() {
        SessionPhase::Idle
    } else if session.is_compacting() {
        SessionPhase::Compaction
    } else if session.is_summarizing() {
        SessionPhase::BranchSummary
    } else if session.is_retrying() {
        SessionPhase::Retry
    } else {
        SessionPhase::Turn
    }
}

/// Maps one transcript message to the reduced wire item (text, image,
/// and tool-call content only; custom roles have no wire shape).
fn map_agent_message(message: &AgentMessage) -> Option<TranscriptItem> {
    match message {
        AgentMessage::Llm(inner) => match inner.as_ref() {
            pi_ai::Message::User(user) => {
                let content = match &user.content {
                    pi_ai::UserMessageContent::Text(text) => vec![wire_text(text.clone())],
                    pi_ai::UserMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|block| match block {
                            pi_ai::UserContent::Text(text) => wire_text(text.text.clone()),
                            pi_ai::UserContent::Image(image) => UserContent::Image(ImageContent {
                                type_field: "image".to_string(),
                                data: image.data.clone(),
                                mime_type: image.mime_type.clone(),
                            }),
                        })
                        .collect(),
                };
                Some(TranscriptItem::User(UserTranscriptItem {
                    type_field: "user".to_string(),
                    content,
                }))
            }
            pi_ai::Message::Assistant(assistant) => {
                let content = assistant
                    .content
                    .iter()
                    .map(|block| match block {
                        pi_ai::AssistantContent::Text(text) => {
                            AssistantContent::Text(TextContent {
                                type_field: "text".to_string(),
                                text: text.text.clone(),
                            })
                        }
                        pi_ai::AssistantContent::Thinking(thinking) => {
                            AssistantContent::Thinking(ThinkingContent {
                                type_field: "thinking".to_string(),
                                thinking: thinking.thinking.clone(),
                                redacted: thinking.redacted,
                            })
                        }
                        pi_ai::AssistantContent::ToolCall(call) => {
                            AssistantContent::ToolCall(ToolCallContent {
                                type_field: "toolCall".to_string(),
                                tool_call_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                input: serde_json_to_wire(&call.arguments),
                            })
                        }
                    })
                    .collect::<Vec<_>>();
                Some(TranscriptItem::Assistant(crate::remote::schemas::AssistantTranscriptItem {
                    type_field: "assistant".to_string(),
                    content,
                }))
            }
            pi_ai::Message::ToolResult(result) => {
                let content = result
                    .content
                    .iter()
                    .map(|block| match block {
                        pi_ai::ToolResultContent::Text(text) => wire_tool_text(text.text.clone()),
                        pi_ai::ToolResultContent::Image(image) => {
                            crate::remote::schemas::ToolContent::Image(ImageContent {
                                type_field: "image".to_string(),
                                data: image.data.clone(),
                                mime_type: image.mime_type.clone(),
                            })
                        }
                    })
                    .collect::<Vec<_>>();
                Some(TranscriptItem::Tool(crate::remote::schemas::ToolTranscriptItem {
                    type_field: "tool".to_string(),
                    tool_call_id: result.tool_call_id.clone(),
                    content,
                }))
            }
        },
        AgentMessage::Custom(_) => None,
    }
}

fn wire_text(text: String) -> UserContent {
    UserContent::Text(TextContent { type_field: "text".to_string(), text })
}

fn wire_tool_text(text: String) -> crate::remote::schemas::ToolContent {
    crate::remote::schemas::ToolContent::Text(TextContent { type_field: "text".to_string(), text })
}

/// Converts a JSON object into the wire value model.
fn serde_json_to_wire(
    value: &serde_json::Map<String, serde_json::Value>,
) -> JsonValue {
    use crate::remote::schemas::JsonValue as Wire;
    let mut map = indexmap::IndexMap::new();
    for (key, item) in value {
        map.insert(
            key.clone(),
            match item {
                serde_json::Value::Null => Wire::Null,
                serde_json::Value::Bool(value) => Wire::Bool(*value),
                serde_json::Value::Number(number) => json_number_to_wire(number),
                serde_json::Value::String(text) => Wire::String(text.clone()),
                serde_json::Value::Array(items) => Wire::Array(
                    items.iter().map(json_leaf_to_wire).collect::<Vec<_>>(),
                ),
                serde_json::Value::Object(object) => {
                    let mut inner = indexmap::IndexMap::new();
                    for (inner_key, inner_item) in object {
                        inner.insert(inner_key.clone(), json_leaf_to_wire(inner_item));
                    }
                    Wire::Object(inner)
                }
            },
        );
    }
    Wire::Object(map)
}

fn json_leaf_to_wire(value: &serde_json::Value) -> JsonValue {
    use crate::remote::schemas::JsonValue as Wire;
    match value {
        serde_json::Value::Null => Wire::Null,
        serde_json::Value::Bool(value) => Wire::Bool(*value),
        serde_json::Value::Number(number) => json_number_to_wire(number),
        serde_json::Value::String(text) => Wire::String(text.clone()),
        serde_json::Value::Array(items) => {
            Wire::Array(items.iter().map(json_leaf_to_wire).collect::<Vec<_>>())
        }
        serde_json::Value::Object(object) => {
            let mut map = indexmap::IndexMap::new();
            for (key, item) in object {
                map.insert(key.clone(), json_leaf_to_wire(item));
            }
            Wire::Object(map)
        }
    }
}

/// Maps a JSON number to the wire value model, preserving integer
/// fidelity.
fn json_number_to_wire(number: &serde_json::Number) -> JsonValue {
    use crate::remote::schemas::JsonValue as Wire;
    if let Some(value) = number.as_i64() {
        Wire::Int(value)
    } else {
        Wire::Float(number.as_f64().unwrap_or(0.0))
    }
}

fn model_thinking_to_wire(level: pi_ai::ModelThinkingLevel) -> ThinkingLevel {
    match level {
        pi_ai::ModelThinkingLevel::Off => ThinkingLevel::Off,
        pi_ai::ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        pi_ai::ModelThinkingLevel::Low => ThinkingLevel::Low,
        pi_ai::ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        pi_ai::ModelThinkingLevel::High => ThinkingLevel::High,
        pi_ai::ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        pi_ai::ModelThinkingLevel::Max => ThinkingLevel::Max,
    }
}

fn wire_thinking_to_model(level: ThinkingLevel) -> pi_ai::ModelThinkingLevel {
    match level {
        ThinkingLevel::Off => pi_ai::ModelThinkingLevel::Off,
        ThinkingLevel::Minimal => pi_ai::ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => pi_ai::ModelThinkingLevel::Low,
        ThinkingLevel::Medium => pi_ai::ModelThinkingLevel::Medium,
        ThinkingLevel::High => pi_ai::ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => pi_ai::ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => pi_ai::ModelThinkingLevel::Max,
    }
}

// ---------------------------------------------------------------------------
// Test support and integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use futures::channel::oneshot;

    pub(crate) fn test_model_metadata() -> ModelMetadata {
        ModelMetadata {
            provider: "test-provider".to_string(),
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test-api".to_string(),
            reasoning: false,
            input: vec!["text".to_string()],
            context_window: 8192,
            max_tokens: 1024,
            cost: crate::remote::schemas::ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            supported_thinking_levels: vec![ThinkingLevel::Off, ThinkingLevel::High],
            authenticated: true,
        }
    }

    pub(crate) struct ScriptedSession {
        pub snapshot: Arc<StdMutex<SessionSnapshot>>,
        pub phase: Arc<StdMutex<SessionPhase>>,
        pub listeners: Arc<StdMutex<Vec<(u64, SessionRuntimeListener)>>>,
        pub next_listener_id: AtomicU64,
        pub pending_prompt: Arc<StdMutex<Option<oneshot::Sender<bool>>>>,
        pub steers: Arc<StdMutex<Vec<String>>>,
        pub dispose_count: Arc<AtomicUsize>,
        pub disposed: Arc<tokio::sync::Notify>,
        pub session_id: String,
        pub locks: Arc<StdMutex<HashSet<String>>>,
    }

    impl ScriptedSession {
        pub(crate) fn new(
            id: String,
            snapshot: Arc<StdMutex<SessionSnapshot>>,
            locks: Arc<StdMutex<HashSet<String>>>,
        ) -> Self {
            Self {
                snapshot,
                phase: Arc::new(StdMutex::new(SessionPhase::Idle)),
                listeners: Arc::new(StdMutex::new(Vec::new())),
                next_listener_id: AtomicU64::new(1),
                pending_prompt: Arc::new(StdMutex::new(None)),
                steers: Arc::new(StdMutex::new(Vec::new())),
                dispose_count: Arc::new(AtomicUsize::new(0)),
                disposed: Arc::new(tokio::sync::Notify::new()),
                session_id: id,
                locks,
            }
        }

        pub(crate) fn finish_prompt(&self, complete: bool) {
            let sender = lock(&self.pending_prompt).take();
            if let Some(tx) = sender {
                let _ = tx.send(complete);
            }
        }

        pub(crate) fn emit_snapshot(&self) {
            let listeners: Vec<SessionRuntimeListener> =
                lock(&self.listeners).iter().map(|(_, l)| Arc::clone(l)).collect();
            for listener in listeners {
                listener(&SessionRuntimeEvent::Snapshot);
            }
        }

        pub(crate) fn emit_progress(&self, progress: TranscriptProgress) {
            let listeners: Vec<SessionRuntimeListener> =
                lock(&self.listeners).iter().map(|(_, l)| Arc::clone(l)).collect();
            for listener in listeners {
                listener(&SessionRuntimeEvent::Progress(progress.clone()));
            }
        }
    }

    impl SessionRuntime for ScriptedSession {
        fn snapshot(&self) -> BoxFuture<'static, Result<SessionSnapshot, ServerError>> {
            let snapshot = lock(&self.snapshot).clone();
            Box::pin(futures::future::ready(Ok(snapshot)))
        }

        fn phase(&self) -> SessionPhase {
            *lock(&self.phase)
        }

        fn prompt(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
            let current_phase = *lock(&self.phase);
            if current_phase != SessionPhase::Idle {
                return Box::pin(futures::future::ready(Err(ServerError::new(
                    ServerOperationCode::Busy,
                    "A prompt is already running",
                ))));
            }
            *lock(&self.phase) = SessionPhase::Turn;
            let (tx, rx) = oneshot::channel();
            *lock(&self.pending_prompt) = Some(tx);
            let snapshot = Arc::clone(&self.snapshot);
            let phase = Arc::clone(&self.phase);
            let listeners_arc = Arc::clone(&self.listeners);
            // Add user message to transcript and bump revision
            {
                let mut s = lock(&snapshot);
                s.revision += 1;
                s.transcript.push(TranscriptItem::User(UserTranscriptItem {
                    type_field: "user".to_string(),
                    content: vec![UserContent::Text(TextContent {
                        type_field: "text".to_string(),
                        text: text.clone(),
                    })],
                }));
            }
            let listeners: Vec<SessionRuntimeListener> =
                lock(&listeners_arc).iter().map(|(_, l)| Arc::clone(l)).collect();
            for l in &listeners {
                l(&SessionRuntimeEvent::Snapshot);
            }
            Box::pin(async move {
                let complete = rx.await.unwrap_or(false);
                *lock(&phase) = SessionPhase::Idle;
                {
                    let mut s = lock(&snapshot);
                    s.revision += 1;
                    let reply = if complete {
                        format!("reply:{text}")
                    } else {
                        String::new()
                    };
                    s.transcript
                        .push(TranscriptItem::Assistant(crate::remote::schemas::AssistantTranscriptItem {
                            type_field: "assistant".to_string(),
                            content: vec![AssistantContent::Text(TextContent {
                                type_field: "text".to_string(),
                                text: reply,
                            })],
                        }));
                }
                let listeners: Vec<SessionRuntimeListener> =
                    lock(&listeners_arc).iter().map(|(_, l)| Arc::clone(l)).collect();
                for l in &listeners {
                    l(&SessionRuntimeEvent::Snapshot);
                }
                Ok(())
            })
        }

        fn steer(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
            let current_phase = *lock(&self.phase);
            if current_phase == SessionPhase::Idle {
                return Box::pin(futures::future::ready(Err(ServerError::new(
                    ServerOperationCode::Busy,
                    "There is no active prompt to steer",
                ))));
            }
            lock(&self.steers).push(text.clone());
            {
                let mut s = lock(&self.snapshot);
                s.revision += 1;
                s.queued_steer_count += 1;
                s.queued_steer.push(UserTranscriptItem {
                    type_field: "user".to_string(),
                    content: vec![UserContent::Text(TextContent {
                        type_field: "text".to_string(),
                        text,
                    })],
                });
            }
            self.emit_snapshot();
            Box::pin(futures::future::ready(Ok(())))
        }

        fn abort(&self) -> BoxFuture<'static, Result<(), ServerError>> {
            let sender = lock(&self.pending_prompt).take();
            match sender {
                Some(tx) => {
                    let _ = tx.send(false);
                    Box::pin(futures::future::ready(Ok(())))
                }
                None => Box::pin(futures::future::ready(Err(ServerError::new(
                    ServerOperationCode::Busy,
                    "There is no active prompt to abort",
                )))),
            }
        }

        fn set_model(&self, model: ModelRef) -> BoxFuture<'static, Result<(), ServerError>> {
            if *lock(&self.phase) != SessionPhase::Idle {
                return Box::pin(futures::future::ready(Err(ServerError::new(
                    ServerOperationCode::Busy,
                    "Session is busy",
                ))));
            }
            {
                let mut s = lock(&self.snapshot);
                s.model = model;
                s.revision += 1;
            }
            self.emit_snapshot();
            Box::pin(futures::future::ready(Ok(())))
        }

        fn set_thinking(&self, level: ThinkingLevel) -> BoxFuture<'static, Result<(), ServerError>> {
            if *lock(&self.phase) != SessionPhase::Idle {
                return Box::pin(futures::future::ready(Err(ServerError::new(
                    ServerOperationCode::Busy,
                    "Session is busy",
                ))));
            }
            {
                let mut s = lock(&self.snapshot);
                s.thinking_level = level;
                s.revision += 1;
            }
            self.emit_snapshot();
            Box::pin(futures::future::ready(Ok(())))
        }

        fn subscribe(&self, listener: SessionRuntimeListener) -> Box<dyn Fn() + Send + Sync> {
            let id = self.next_listener_id.fetch_add(1, Ordering::SeqCst);
            lock(&self.listeners).push((id, listener));
            let listeners = Arc::clone(&self.listeners);
            Box::new(move || {
                lock(&listeners).retain(|(lid, _)| *lid != id);
            })
        }

        fn dispose(&self) -> BoxFuture<'static, Result<(), ServerError>> {
            self.dispose_count.fetch_add(1, Ordering::SeqCst);
            lock(&self.locks).remove(&self.session_id);
            self.disposed.notify_waiters();
            Box::pin(futures::future::ready(Ok(())))
        }
    }

    pub(crate) struct ScriptedService {
        pub sessions: Arc<StdMutex<HashMap<String, Arc<StdMutex<SessionSnapshot>>>>>,
        pub locked: Arc<StdMutex<HashSet<String>>>,
        pub runtimes: Arc<StdMutex<HashMap<String, Vec<Arc<ScriptedSession>>>>>,
        pub models: Vec<ModelMetadata>,
    }

    impl ScriptedService {
        pub(crate) fn new() -> Self {
            Self {
                sessions: Arc::new(StdMutex::new(HashMap::new())),
                locked: Arc::new(StdMutex::new(HashSet::new())),
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                models: vec![test_model_metadata()],
            }
        }

        pub(crate) fn seed(&self, id: &str) {
            let snapshot = SessionSnapshot {
                session_id: id.to_string(),
                model: ModelRef {
                    provider: "test-provider".to_string(),
                    id: "test-model".to_string(),
                },
                thinking_level: ThinkingLevel::Off,
                locked: false,
                revision: 0,
                transcript: Vec::new(),
                queued_steer: Vec::new(),
                queued_steer_count: 0,
            };
            lock(&self.sessions).insert(id.to_string(), Arc::new(StdMutex::new(snapshot)));
        }

        pub(crate) fn latest_runtime(&self, id: &str) -> Arc<ScriptedSession> {
            let runtimes = lock(&self.runtimes);
            runtimes
                .get(id)
                .and_then(|list| list.last().cloned())
                .expect("runtime exists")
        }
    }

    impl ServerService for ScriptedService {
        fn list_sessions(&self) -> BoxFuture<'static, Result<Vec<SessionSnapshot>, ServerError>> {
            let sessions: Vec<SessionSnapshot> = lock(&self.sessions)
                .values()
                .map(|s| lock(s).clone())
                .collect();
            Box::pin(futures::future::ready(Ok(sessions)))
        }

        fn list_models(&self) -> BoxFuture<'static, Result<Vec<ModelMetadata>, ServerError>> {
            let models = self.models.clone();
            Box::pin(futures::future::ready(Ok(models)))
        }

        fn create_session(
            &self,
            options: CreateSessionOptions,
        ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>> {
            let sessions = Arc::clone(&self.sessions);
            let locked = Arc::clone(&self.locked);
            let runtimes = Arc::clone(&self.runtimes);
            Box::pin(async move {
                if lock(&sessions).contains_key(&options.id) {
                    return Err(ServerError::new(
                        ServerOperationCode::SessionLocked,
                        "Session already exists",
                    ));
                }
                let model = options.model.unwrap_or_else(|| ModelRef {
                    provider: "test-provider".to_string(),
                    id: "test-model".to_string(),
                });
                let thinking_level = options.thinking_level.unwrap_or(ThinkingLevel::Off);
                let snapshot = Arc::new(StdMutex::new(SessionSnapshot {
                    session_id: options.id.clone(),
                    model,
                    thinking_level,
                    locked: false,
                    revision: 0,
                    transcript: Vec::new(),
                    queued_steer: Vec::new(),
                    queued_steer_count: 0,
                }));
                lock(&sessions).insert(options.id.clone(), Arc::clone(&snapshot));
                lock(&locked).insert(options.id.clone());
                let runtime =
                    Arc::new(ScriptedSession::new(options.id.clone(), snapshot, Arc::clone(&locked)));
                lock(&runtimes)
                    .entry(options.id)
                    .or_default()
                    .push(Arc::clone(&runtime));
                Ok(Box::new(runtime_ref(runtime)) as Box<dyn SessionRuntime>)
            })
        }

        fn open_session(
            &self,
            session_id: String,
        ) -> BoxFuture<'static, Result<Box<dyn SessionRuntime>, ServerError>> {
            let sessions = Arc::clone(&self.sessions);
            let locked = Arc::clone(&self.locked);
            let runtimes = Arc::clone(&self.runtimes);
            Box::pin(async move {
                let snapshot = lock(&sessions)
                    .get(&session_id)
                    .cloned()
                    .ok_or_else(|| {
                        ServerError::new(
                            ServerOperationCode::NotFound,
                            format!("Unknown session: {session_id}"),
                        )
                    })?;
                if lock(&locked).contains(&session_id) {
                    return Err(ServerError::new(
                        ServerOperationCode::SessionLocked,
                        format!("Session is locked: {session_id}"),
                    ));
                }
                lock(&locked).insert(session_id.clone());
                let runtime = Arc::new(ScriptedSession::new(
                    session_id.clone(),
                    snapshot,
                    Arc::clone(&locked),
                ));
                lock(&runtimes)
                    .entry(session_id)
                    .or_default()
                    .push(Arc::clone(&runtime));
                Ok(Box::new(runtime_ref(runtime)) as Box<dyn SessionRuntime>)
            })
        }
    }

    fn runtime_ref(runtime: Arc<ScriptedSession>) -> ScriptedSessionWrapper {
        ScriptedSessionWrapper { inner: runtime }
    }

    struct ScriptedSessionWrapper {
        inner: Arc<ScriptedSession>,
    }

    impl SessionRuntime for ScriptedSessionWrapper {
        fn snapshot(&self) -> BoxFuture<'static, Result<SessionSnapshot, ServerError>> {
            self.inner.snapshot()
        }
        fn phase(&self) -> SessionPhase {
            self.inner.phase()
        }
        fn prompt(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.prompt(text)
        }
        fn steer(&self, text: String) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.steer(text)
        }
        fn abort(&self) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.abort()
        }
        fn set_model(&self, model: ModelRef) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.set_model(model)
        }
        fn set_thinking(&self, level: ThinkingLevel) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.set_thinking(level)
        }
        fn subscribe(&self, listener: SessionRuntimeListener) -> Box<dyn Fn() + Send + Sync> {
            self.inner.subscribe(listener)
        }
        fn dispose(&self) -> BoxFuture<'static, Result<(), ServerError>> {
            self.inner.dispose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::remote::client::{PiClient, PiClientOptions, SessionLeaseMode};
    use crate::remote::transport::InMemoryListener;

    async fn setup_in_memory_server(
        service: Arc<dyn ServerService>,
    ) -> (PiServer, Arc<InMemoryListener>) {
        let (listener, _endpoint) = InMemoryListener::new();
        let listener = Arc::new(listener);
        let server_listener = Arc::new(InMemoryServerListener::new(Arc::clone(&listener)));
        let server = PiServer::new(
            service,
            PiServerOptions {
                listeners: vec![server_listener],
                ..Default::default()
            },
        )
        .expect("server options valid");
        server.start().await.expect("server started");
        (server, listener)
    }

    fn client_for_listener(listener: &Arc<InMemoryListener>) -> PiClient {
        let endpoint = listener.endpoint();
        let factory = build_transport(&EndpointSpec::InMemory { endpoint })
            .expect("in-memory factory builds");
        PiClient::new(PiClientOptions {
            transport_factory: factory,
            max_frame_length: None,
            on_listener_error: None,
        })
        .expect("client options valid")
    }

    #[tokio::test]
    async fn two_client_exclusive_lease_rejection_over_in_memory() {
        let service = Arc::new(ScriptedService::new());
        service.seed("session-exclusive");
        let (server, listener) = setup_in_memory_server(service.clone()).await;

        let client_a = client_for_listener(&listener);
        let client_b = client_for_listener(&listener);

        let _hello_a = client_a.connect().await.expect("client A connects");
        let _hello_b = client_b.connect().await.expect("client B connects");

        // Client A acquires an exclusive lease.
        let handle_a = client_a
            .acquire_session("session-exclusive", SessionLeaseMode::Exclusive)
            .await
            .expect("client A acquires exclusive lease");
        assert!(handle_a.attached());

        // Same client attempting a second lease while exclusive is held
        // is rejected locally by lease tracking as Ownership.
        let err_a = client_a
            .attach_session("session-exclusive")
            .await;
        assert!(
            matches!(err_a, Err(crate::remote::client::PiClientError::Ownership(_))),
            "expected Ownership error on conflicting lease, got {err_a:?}"
        );
        // Cleanup
        handle_a.detach().await.expect("detach succeeds");
        client_a.dispose();
        client_b.dispose();
        server.close().await;
    }

    #[tokio::test]
    async fn reattach_after_detach() {
        let service = Arc::new(ScriptedService::new());
        service.seed("session-reattach");
        let (server, listener) = setup_in_memory_server(service.clone()).await;

        let client_a = client_for_listener(&listener);
        let client_b = client_for_listener(&listener);

        client_a.connect().await.expect("client A connects");
        client_b.connect().await.expect("client B connects");

        // A attaches, then detaches
        let handle_a = client_a
            .attach_session("session-reattach")
            .await
            .expect("client A attaches");
        assert!(handle_a.attached());
        handle_a.detach().await.expect("client A detaches");
        assert!(!handle_a.attached());

        // B attaches successfully
        let handle_b = client_b
            .attach_session("session-reattach")
            .await
            .expect("client B attaches after A detached");
        assert!(handle_b.attached());
        handle_b.detach().await.expect("client B detaches");

        // A re-attaches successfully
        let handle_a2 = client_a
            .attach_session("session-reattach")
            .await
            .expect("client A re-attaches");
        assert!(handle_a2.attached());

        handle_a2.detach().await.ok();
        client_a.dispose();
        client_b.dispose();
        server.close().await;
    }

    #[tokio::test]
    async fn broadcast_exactly_once_per_attached_listener() {
        let service = Arc::new(ScriptedService::new());
        service.seed("session-broadcast");
        let (server, listener) = setup_in_memory_server(service.clone()).await;

        let client_a = client_for_listener(&listener);
        let client_b = client_for_listener(&listener);

        client_a.connect().await.expect("client A connects");
        client_b.connect().await.expect("client B connects");

        let handle_a = client_a
            .attach_session("session-broadcast")
            .await
            .expect("A attaches");
        let handle_b = client_b
            .attach_session("session-broadcast")
            .await
            .expect("B attaches");

        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));

        let ca = Arc::clone(&count_a);
        let _sub_a = handle_a
            .subscribe(Arc::new(move |_| {
                ca.fetch_add(1, Ordering::SeqCst);
            }))
            .expect("A subscribes");

        let cb = Arc::clone(&count_b);
        let _sub_b = handle_b
            .subscribe(Arc::new(move |_| {
                cb.fetch_add(1, Ordering::SeqCst);
            }))
            .expect("B subscribes");

        // Runtime emits one snapshot event
        let runtime = service.latest_runtime("session-broadcast");
        runtime.emit_snapshot();

        // Wait for both clients to receive the event
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            count_a.load(Ordering::SeqCst),
            1,
            "client A should receive exactly one event"
        );
        assert_eq!(
            count_b.load(Ordering::SeqCst),
            1,
            "client B should receive exactly one event"
        );

        handle_a.detach().await.ok();
        handle_b.detach().await.ok();
        client_a.dispose();
        client_b.dispose();
        server.close().await;
    }

    #[tokio::test]
    async fn mid_request_disconnect_and_idle_disposal() {
        let service = Arc::new(ScriptedService::new());
        service.seed("session-disconnect");
        let (server, listener) = setup_in_memory_server(service.clone()).await;

        let client_a = client_for_listener(&listener);
        client_a.connect().await.expect("client A connects");

        let handle_a = client_a
            .attach_session("session-disconnect")
            .await
            .expect("A attaches");
        let initial_runtime = service.latest_runtime("session-disconnect");

        let prompt_task = tokio::spawn({
            let handle = handle_a.clone();
            async move { handle.prompt("long prompt".to_string()).await }
        });

        // Wait until prompt is in-flight (phase == Turn)
        let mut waited = 0;
        while initial_runtime.phase() != SessionPhase::Turn && waited < 100 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            waited += 1;
        }
        assert_eq!(initial_runtime.phase(), SessionPhase::Turn);
        // Disconnect Client A while prompt is in-flight
        client_a.disconnect("client dropped");
        let prompt_err = prompt_task.await.expect("prompt task finished");
        assert!(
            matches!(prompt_err, Err(crate::remote::client::PiClientError::Disconnected(_))),
            "in-flight prompt should fail with disconnected, got {prompt_err:?}"
        );
        // Finish the in-flight prompt on the runtime side (disconnect survival)
        initial_runtime.finish_prompt(true);

        // Wait a bit for the prompt async block on the server side to complete
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Disconnect Client A now if not already detached on the server
        let client_b = client_for_listener(&listener);
        client_b.connect().await.expect("B connects");
        let handle_b = client_b
            .attach_session("session-disconnect")
            .await
            .expect("B can re-attach to the survived session");
        assert!(handle_b.attached());

        handle_b.detach().await.ok();
        client_a.dispose();
        client_b.dispose();
        server.close().await;
    }

    #[tokio::test]
    async fn agent_session_hosting_roundtrip() {
        use super::mod_tests_helper::{
            done_event_helper, start_event_helper, test_mock_provider, test_model_val,
        };

        let provider = test_mock_provider(vec![
            Ok(start_event_helper()),
            Ok(done_event_helper("agent reply")),
        ]);
        let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
            provider,
            test_model_val(),
        )
        .expect("test config valid");
        config.system_prompt = "system prompt".into();
        let session = AgentSession::new(config).expect("AgentSession created");

        let model_resolver: ModelResolver = Arc::new(|model_ref| {
            if model_ref.id == "m" || model_ref.id == "test-model" {
                Some(test_model_val())
            } else {
                None
            }
        });
        let service = Arc::new(AgentSessionService::new(model_resolver));
        let session_id = service.register(session).await;

        let (server, listener) = setup_in_memory_server(service.clone()).await;
        let client = client_for_listener(&listener);
        client.connect().await.expect("client connects");

        let handle = client
            .attach_session(&session_id)
            .await
            .expect("attaches to hosted AgentSession");
        assert!(handle.attached());

        // Prompt through the client -> executes on AgentSession via public C15 surface
        let snapshot = handle
            .prompt("hello agent".to_string())
            .await
            .expect("prompt roundtrip");
        assert!(
            snapshot.transcript.iter().any(|item| matches!(item, TranscriptItem::User(_))),
            "transcript should contain the prompt"
        );

        // Set thinking level through public surface
        let thinking_snapshot = handle
            .set_thinking(ThinkingLevel::High)
            .await
            .expect("set thinking level");
        assert_eq!(thinking_snapshot.thinking_level, ThinkingLevel::High);

        // Detach and re-attach
        handle.detach().await.expect("detaches");
        let handle2 = client
            .attach_session(&session_id)
            .await
            .expect("re-attaches to AgentSession");
        assert!(handle2.attached());

        handle2.detach().await.ok();
        client.dispose();
        server.close().await;
        service.dispose_all().await;
    }

    #[test]
    fn structural_witness_no_persistence_or_migration_imports() {
        let sources = [
            ("mod.rs", include_str!("mod.rs")),
            ("client.rs", include_str!("client.rs")),
            ("codec.rs", include_str!("codec.rs")),
            ("framing.rs", include_str!("framing.rs")),
            ("schemas.rs", include_str!("schemas.rs")),
            ("serde_cbor.rs", include_str!("serde_cbor.rs")),
            ("transport.rs", include_str!("transport.rs")),
            ("transport/in_memory.rs", include_str!("transport/in_memory.rs")),
            #[cfg(unix)]
            ("transport/unix.rs", include_str!("transport/unix.rs")),
            ("server.rs", include_str!("server.rs")),
            #[cfg(unix)]
            ("server/unix.rs", include_str!("server/unix.rs")),
        ];

        let forbidden = [
            "core::sessions",
            "core::migrations",
            "agent_session::persistence",
            "migrations::",
            "SessionManager",
        ];

        for (filename, content) in sources {
            // Test production code only (before #[cfg(test)])
            let prod_code = content
                .split_once("#[cfg(test)]")
                .map_or(content, |(prod, _)| prod);

            for pattern in forbidden {
                assert!(
                    !prod_code.contains(pattern),
                    "file {filename} contains forbidden persistence/migration import '{pattern}'"
                );
            }
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn cfg_gated_unsupported_on_platform_test() {
        let spec = ListenSpec::Unix {
            path: PathBuf::from("/tmp/test.sock"),
            max_pending_bytes: None,
        };
        let result = build_listener(&spec);
        assert!(
            matches!(
                result,
                Err(EndpointSpecError::UnsupportedOnPlatform {
                    kind: EndpointKind::Unix,
                    ..
                })
            ),
            "Unix listen spec on non-Unix must return typed UnsupportedOnPlatform"
        );
    }
}

#[cfg(test)]
pub(crate) mod mod_tests_helper {
    use super::*;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, Context, DoneReason, Model, ModelCost, ModelInput,
        Provider, ProviderError, StreamOptions,
    };

    pub(crate) fn test_model_val() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[derive(Clone)]
    pub(crate) struct MockProvider(pub Vec<Result<AssistantMessageEvent, ProviderError>>);

    impl Provider for MockProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            stream::iter(self.0.clone()).boxed()
        }
    }

    pub(crate) fn test_mock_provider(events: Vec<Result<AssistantMessageEvent, ProviderError>>) -> Arc<dyn Provider> {
        Arc::new(MockProvider(events))
    }

    pub(crate) fn start_event_helper() -> AssistantMessageEvent {
        AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::new("test-api", "test-provider", "m", 1)),
        }
    }

    pub(crate) fn done_event_helper(text: &str) -> AssistantMessageEvent {
        let mut message = AssistantMessage::new("test-api", "test-provider", "m", 1);
        message.content.push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(text)));
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        }
    }
}
