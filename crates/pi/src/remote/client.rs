//! Transport-neutral remote-session client (R3) — portable port of upstream
//! `client.ts`, `connection.ts`, `state.ts`, `session-handle.ts`, and
//! `errors.ts`, with **zero** `cfg` branches.
//!
//! The client speaks the R1–R2 wire (CBOR in 4-byte length-prefixed frames)
//! over any [`ByteTransport`](crate::remote::transport::ByteTransport)
//! produced by a factory. There is no automatic reconnect: every connection
//! attempt asks the factory for a fresh transport.
//!
//! # Error taxonomy
//!
//! Every failure the client can hand a caller is one typed variant of
//! [`PiClientError`]; the five runtime classes are
//! [`PiClientDisposedError`], [`PiDisconnectedError`], [`PiServerError`],
//! [`PiSessionOwnershipError`], and [`PiSessionDetachedError`]. Server
//! `ProtocolError` codes are mapped at this seam — `session_locked` becomes
//! the ownership class, everything else the server class — and every
//! transport failure (open, send, orderly close, terminal error) maps to
//! exactly one variant: the disconnected class. Wire violations surface as
//! the protocol class owned by the R1–R2 layers. The pinned mapping table
//! lives in the tests below.
//!
//! # Divergences from upstream (recorded)
//!
//! - `session_locked` server errors map to the ownership class at the seam
//!   (ticket-mandated); upstream surfaced them as generic server errors.
//! - Listener panics are isolated — caught and reported through
//!   [`PiClientOptions::on_listener_error`] — but never fail the
//!   connection; upstream failed the connection when a snapshot listener
//!   threw during the handshake.
//! - Dropping [`PiClient`] disposes it synchronously (Rust has no GC finalizer);
//!   dropping [`PiSessionHandle`] does **not** release its lease, matching
//!   upstream's explicit-only disposal.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::oneshot;

use crate::remote::codec::{CodecError, ServerMessageDecoder, encode_client_message};
use crate::remote::framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoderOptions, FrameError};
use crate::remote::schemas::{
    ClientMessage, Command, CommandResult, JsonValue, ModelRef, PROTOCOL_VERSION, ProtocolError,
    ProtocolErrorCode, ServerEvent, ServerMessage, ServerSnapshot, SessionMetadata,
    SessionSnapshot, ThinkingLevel,
};
use crate::remote::transport::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, TransportError,
};

/// Max value a frame length may declare (unsigned 32-bit).
const MAX_UINT32: u64 = 0xffff_ffff;

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Error taxonomy
// ---------------------------------------------------------------------------

/// A server-reported failure (`ok: false` response or `hello_error`).
#[derive(Debug, Clone, PartialEq)]
pub struct PiServerError {
    /// Typed protocol error code.
    pub code: ProtocolErrorCode,
    /// Human-readable server message.
    pub message: String,
    /// Optional structured details.
    pub details: Option<JsonValue>,
}

impl fmt::Display for PiServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", error_code_name(&self.code), self.message)
    }
}

impl std::error::Error for PiServerError {}

fn error_code_name(code: &ProtocolErrorCode) -> &'static str {
    match code {
        ProtocolErrorCode::Version => "version",
        ProtocolErrorCode::Busy => "busy",
        ProtocolErrorCode::SessionLocked => "session_locked",
        ProtocolErrorCode::NotFound => "not_found",
        ProtocolErrorCode::InvalidRequest => "invalid_request",
        ProtocolErrorCode::NotImplemented => "not_implemented",
        ProtocolErrorCode::InternalError => "internal_error",
    }
}

/// The transport went away. Every transport failure maps here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiDisconnectedError {
    /// Cause text.
    pub message: String,
}

impl PiDisconnectedError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PiDisconnectedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiDisconnectedError {}

impl Default for PiDisconnectedError {
    fn default() -> Self {
        Self::new("Pi client is disconnected")
    }
}

/// The client was disposed and can no longer serve requests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PiClientDisposedError;

impl fmt::Display for PiClientDisposedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pi client is disposed")
    }
}

impl std::error::Error for PiClientDisposedError {}

/// A session lease could not be acquired, or the server reported the
/// session locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSessionOwnershipError {
    /// Session the conflict is about, when known.
    pub session_id: Option<String>,
    /// Conflict description.
    pub message: String,
}

impl fmt::Display for PiSessionOwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiSessionOwnershipError {}

/// A session-scoped operation ran on a handle whose session is not attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiSessionDetachedError {
    /// Session that is not attached.
    pub session_id: String,
}

impl PiSessionDetachedError {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

impl fmt::Display for PiSessionDetachedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session {} is not attached", self.session_id)
    }
}

impl std::error::Error for PiSessionDetachedError {}

/// A wire-level protocol violation (framing, codec, or correlation
/// mismatch) — the class the R1–R2 layers own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolViolationError {
    /// Violation description.
    pub message: String,
}

impl ProtocolViolationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolViolationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProtocolViolationError {}

impl From<CodecError> for ProtocolViolationError {
    fn from(error: CodecError) -> Self {
        Self::new(error.to_string())
    }
}

impl From<FrameError> for ProtocolViolationError {
    fn from(error: FrameError) -> Self {
        Self::new(error.to_string())
    }
}

/// Every failure the client can hand a caller: exactly one typed variant
/// per failure site, pinned by the mapping-table test.
#[derive(Debug, Clone, PartialEq)]
pub enum PiClientError {
    /// The client was disposed.
    Disposed(PiClientDisposedError),
    /// The transport went away (open failure, send failure, orderly close,
    /// or terminal transport error).
    Disconnected(PiDisconnectedError),
    /// The server reported a failure that is not session ownership.
    Server(PiServerError),
    /// A lease conflict or a server-side session lock.
    Ownership(PiSessionOwnershipError),
    /// A session-scoped operation on a detached session.
    SessionDetached(PiSessionDetachedError),
    /// A wire-level protocol violation.
    Protocol(ProtocolViolationError),
}

impl fmt::Display for PiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disposed(error) => write!(f, "{error}"),
            Self::Disconnected(error) => write!(f, "{error}"),
            Self::Server(error) => write!(f, "{error}"),
            Self::Ownership(error) => write!(f, "{error}"),
            Self::SessionDetached(error) => write!(f, "{error}"),
            Self::Protocol(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PiClientError {}

impl PiClientError {
    fn disconnected(message: impl Into<String>) -> Self {
        Self::Disconnected(PiDisconnectedError::new(message))
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(ProtocolViolationError::new(message))
    }
}

fn to_disconnected(error: &TransportError) -> PiClientError {
    PiClientError::disconnected(error.to_string())
}

/// Options error at construction — distinct from the five runtime classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiClientOptionsError {
    /// `max_frame_length` outside `1..=u32::MAX`.
    InvalidMaxFrameLength {
        /// The rejected value.
        value: u64,
        /// The inclusive upper bound.
        max: u64,
    },
}

impl fmt::Display for PiClientOptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaxFrameLength { value, max } => {
                write!(
                    f,
                    "PiClient maxFrameLength must be an integer between 1 and {max}, got {value}"
                )
            }
        }
    }
}

impl std::error::Error for PiClientOptionsError {}

// ---------------------------------------------------------------------------
// Listener plumbing
// ---------------------------------------------------------------------------

/// Receives the latest server snapshot.
pub type ServerSnapshotListener = Arc<dyn Fn(&ServerSnapshot) + Send + Sync>;
/// Receives every unsolicited server event.
pub type ServerEventListener = Arc<dyn Fn(&ServerEvent) + Send + Sync>;
/// Receives snapshots for one session.
pub type SessionSnapshotListener = Arc<dyn Fn(&SessionSnapshot) + Send + Sync>;
/// Receives events for one session.
pub type SessionEventListener = Arc<dyn Fn(&ServerEvent) + Send + Sync>;
/// Receives connection-state transitions.
pub type ConnectionStateListener = Arc<dyn Fn(&ConnectionStateChange) + Send + Sync>;
/// Reports isolated listener panics.
pub type ListenerErrorHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// Cancels one listener subscription on drop.
pub struct Subscription {
    remove: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    fn new(remove: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            remove: Some(Box::new(remove)),
        }
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(remove) = self.remove.take() {
            remove();
        }
    }
}

/// Connection lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No transport.
    Disconnected,
    /// A connection attempt is in flight.
    Connecting,
    /// Handshake complete.
    Connected,
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "disconnected"),
            Self::Connecting => write!(f, "connecting"),
            Self::Connected => write!(f, "connected"),
        }
    }
}

/// One connection-state transition, with the failure that caused a
/// transition into [`ConnectionState::Disconnected`] when applicable.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionStateChange {
    /// New state.
    pub state: ConnectionState,
    /// Failure that caused a disconnection.
    pub error: Option<PiClientError>,
}

/// Deferred callback deliveries collected under the client lock and invoked
/// after it is released, so listeners may safely call back into the client.
#[derive(Default)]
struct Notifications {
    connection_state: Vec<ConnectionStateChange>,
    snapshot_listeners: Vec<ServerSnapshotListener>,
    snapshot_values: Vec<ServerSnapshot>,
    event_listeners: Vec<ServerEventListener>,
    event_values: Vec<ServerEvent>,
    session_snapshot_listeners: Vec<SessionSnapshotListener>,
    session_snapshot_values: Vec<SessionSnapshot>,
    session_event_listeners: Vec<SessionEventListener>,
    session_event_values: Vec<ServerEvent>,
    handshake: Option<Box<dyn FnOnce() + Send>>,
}

impl Notifications {
    fn deliver(&self, on_listener_error: Option<&ListenerErrorHandler>) {
        for (listener, value) in self
            .snapshot_listeners
            .iter()
            .zip(self.snapshot_values.iter())
        {
            invoke_isolated(listener, value, on_listener_error);
        }
        for (listener, value) in self.event_listeners.iter().zip(self.event_values.iter()) {
            invoke_isolated(listener, value, on_listener_error);
        }
        for (listener, value) in self
            .session_snapshot_listeners
            .iter()
            .zip(self.session_snapshot_values.iter())
        {
            invoke_isolated(listener, value, on_listener_error);
        }
        for (listener, value) in self
            .session_event_listeners
            .iter()
            .zip(self.session_event_values.iter())
        {
            invoke_isolated(listener, value, on_listener_error);
        }
    }
}

fn invoke_isolated<T>(
    listener: &Arc<dyn Fn(&T) + Send + Sync>,
    value: &T,
    on_error: Option<&ListenerErrorHandler>,
) {
    if catch_unwind(AssertUnwindSafe(|| listener(value))).is_err()
        && let Some(report) = on_error
    {
        catch_unwind(AssertUnwindSafe(|| report("listener panicked"))).ok();
    }
}

// ---------------------------------------------------------------------------
// Client state (port of upstream state.ts)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ClientState {
    next_listener_id: u64,
    snapshot: Option<ServerSnapshot>,
    session_snapshots: HashMap<String, SessionSnapshot>,
    attached_sessions: HashSet<String>,
    snapshot_listeners: Vec<(u64, ServerSnapshotListener)>,
    event_listeners: Vec<(u64, ServerEventListener)>,
    session_snapshot_listeners: HashMap<String, Vec<(u64, SessionSnapshotListener)>>,
    session_event_listeners: HashMap<String, Vec<(u64, SessionEventListener)>>,
}

impl ClientState {
    fn reset(&mut self) {
        self.snapshot = None;
        self.session_snapshots.clear();
        self.attached_sessions.clear();
    }

    fn clear_attachments(&mut self) {
        self.attached_sessions.clear();
    }

    fn dispose(&mut self) {
        self.reset();
        self.snapshot_listeners.clear();
        self.event_listeners.clear();
        self.session_snapshot_listeners.clear();
        self.session_event_listeners.clear();
    }

    fn next_id(&mut self) -> u64 {
        self.next_listener_id += 1;
        self.next_listener_id
    }

    fn is_session_attached(&self, session_id: &str) -> bool {
        self.attached_sessions.contains(session_id)
    }

    fn forget_session_snapshot(&mut self, session_id: &str) -> Option<SessionSnapshot> {
        self.session_snapshots.remove(session_id)
    }

    fn restore_session_snapshot(&mut self, snapshot: SessionSnapshot) {
        self.session_snapshots
            .entry(snapshot.session_id.clone())
            .or_insert(snapshot);
    }

    fn apply_result(&mut self, result: &CommandResult, notifications: &mut Notifications) {
        match result {
            CommandResult::List { .. } => {}
            CommandResult::Detach { session_id } => {
                self.attached_sessions.remove(session_id);
            }
            other => {
                if let Some(session) = result_session(other) {
                    self.apply_session_snapshot(session.clone(), false, notifications);
                }
            }
        }
    }

    fn apply_event(&mut self, event: &ServerEvent, notifications: &mut Notifications) {
        match event {
            ServerEvent::ServerSnapshot { snapshot } => {
                self.apply_server_snapshot(snapshot, notifications);
            }
            ServerEvent::SessionSnapshot { snapshot } => {
                self.apply_session_snapshot(snapshot.clone(), false, notifications);
            }
            ServerEvent::SessionRemoved { session_id } => {
                self.session_snapshots.remove(session_id);
                self.attached_sessions.remove(session_id);
            }
            ServerEvent::SessionProgress { .. } => {}
        }
        let global_listeners = self
            .event_listeners
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect::<Vec<_>>();
        notifications.event_listeners.extend(global_listeners);
        notifications.event_values.push(event.clone());
        if let Some(session_id) = event_session_id(event)
            && let Some(listeners) = self.session_event_listeners.get(session_id)
        {
            let listeners = listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect::<Vec<_>>();
            notifications.session_event_listeners.extend(listeners);
            notifications.session_event_values.push(event.clone());
        }
    }

    fn apply_server_snapshot(
        &mut self,
        snapshot: &ServerSnapshot,
        notifications: &mut Notifications,
    ) {
        if let Some(current) = &self.snapshot
            && snapshot.revision < current.revision
        {
            return;
        }
        self.snapshot = Some(snapshot.clone());
        let listeners = self
            .snapshot_listeners
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect::<Vec<_>>();
        notifications.snapshot_listeners.extend(listeners);
        notifications.snapshot_values.push(snapshot.clone());
    }

    fn apply_session_snapshot(
        &mut self,
        snapshot: SessionSnapshot,
        force: bool,
        notifications: &mut Notifications,
    ) {
        if let Some(current) = self.session_snapshots.get(&snapshot.session_id)
            && !force
            && snapshot.revision < current.revision
        {
            return;
        }
        // Upstream derives attachment from the snapshot's `attached` field;
        // the landed schema has no such field, so attachment follows the
        // apply path: snapshots that arrive through attach/prompt results
        // or session_snapshot events mark the session attached.
        self.attached_sessions.insert(snapshot.session_id.clone());
        if let Some(listeners) = self.session_snapshot_listeners.get(&snapshot.session_id) {
            let listeners = listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect::<Vec<_>>();
            notifications.session_snapshot_listeners.extend(listeners);
            notifications.session_snapshot_values.push(snapshot.clone());
        }
        self.session_snapshots
            .insert(snapshot.session_id.clone(), snapshot);
    }
}

fn result_session(result: &CommandResult) -> Option<&SessionSnapshot> {
    match result {
        CommandResult::Create { session }
        | CommandResult::Attach { session }
        | CommandResult::Prompt { session }
        | CommandResult::Steer { session }
        | CommandResult::Abort { session }
        | CommandResult::SetModel { session }
        | CommandResult::SetThinking { session } => Some(session),
        CommandResult::List { .. } | CommandResult::Detach { .. } => None,
    }
}

fn event_session_id(event: &ServerEvent) -> Option<&str> {
    match event {
        ServerEvent::SessionSnapshot { snapshot } => Some(&snapshot.session_id),
        ServerEvent::SessionProgress { session_id, .. }
        | ServerEvent::SessionRemoved { session_id } => Some(session_id),
        ServerEvent::ServerSnapshot { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Command classification for correlation
// ---------------------------------------------------------------------------

/// Discriminant shared by a [`Command`] and its [`CommandResult`], used to
/// pin request/response correlation at the seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// `list`
    List,
    /// `create`
    Create,
    /// `attach`
    Attach,
    /// `detach`
    Detach,
    /// `prompt`
    Prompt,
    /// `steer`
    Steer,
    /// `abort`
    Abort,
    /// `set_model`
    SetModel,
    /// `set_thinking`
    SetThinking,
}

impl CommandKind {
    fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Create => "create",
            Self::Attach => "attach",
            Self::Detach => "detach",
            Self::Prompt => "prompt",
            Self::Steer => "steer",
            Self::Abort => "abort",
            Self::SetModel => "set_model",
            Self::SetThinking => "set_thinking",
        }
    }

    fn session_id_of(command: &Command) -> Option<String> {
        match command {
            Command::Attach { session_id }
            | Command::Detach { session_id }
            | Command::Prompt { session_id, .. }
            | Command::Steer { session_id, .. }
            | Command::Abort { session_id }
            | Command::SetModel { session_id, .. }
            | Command::SetThinking { session_id, .. } => Some(session_id.clone()),
            Command::List | Command::Create { .. } => None,
        }
    }
}

fn command_kind(command: &Command) -> CommandKind {
    match command {
        Command::List => CommandKind::List,
        Command::Create { .. } => CommandKind::Create,
        Command::Attach { .. } => CommandKind::Attach,
        Command::Detach { .. } => CommandKind::Detach,
        Command::Prompt { .. } => CommandKind::Prompt,
        Command::Steer { .. } => CommandKind::Steer,
        Command::Abort { .. } => CommandKind::Abort,
        Command::SetModel { .. } => CommandKind::SetModel,
        Command::SetThinking { .. } => CommandKind::SetThinking,
    }
}

fn result_kind(result: &CommandResult) -> CommandKind {
    match result {
        CommandResult::List { .. } => CommandKind::List,
        CommandResult::Create { .. } => CommandKind::Create,
        CommandResult::Attach { .. } => CommandKind::Attach,
        CommandResult::Detach { .. } => CommandKind::Detach,
        CommandResult::Prompt { .. } => CommandKind::Prompt,
        CommandResult::Steer { .. } => CommandKind::Steer,
        CommandResult::Abort { .. } => CommandKind::Abort,
        CommandResult::SetModel { .. } => CommandKind::SetModel,
        CommandResult::SetThinking { .. } => CommandKind::SetThinking,
    }
}

/// Maps a server-reported [`ProtocolError`] into the typed client taxonomy
/// at the seam: `session_locked` becomes the ownership class, everything
/// else the server class.
fn map_server_error(error: ProtocolError, pending: &PendingRequest) -> PiClientError {
    if error.code == ProtocolErrorCode::SessionLocked {
        PiClientError::Ownership(PiSessionOwnershipError {
            session_id: CommandKind::session_id_of(&pending.command),
            message: error.message,
        })
    } else {
        PiClientError::Server(PiServerError {
            code: error.code,
            message: error.message,
            details: error.details,
        })
    }
}

// ---------------------------------------------------------------------------
// Connection lifecycle (port of upstream connection.ts)
// ---------------------------------------------------------------------------

type HandshakeSender = oneshot::Sender<Result<ServerSnapshot, PiClientError>>;

enum ConnState {
    Disconnected,
    Connecting {
        id: u64,
        transport: Option<Arc<dyn ByteTransport>>,
        handshake: Option<HandshakeSender>,
        decoder: ServerMessageDecoder,
    },
    Connected {
        id: u64,
        transport: Arc<dyn ByteTransport>,
        decoder: ServerMessageDecoder,
    },
}

impl ConnState {
    fn name(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting { .. } => "connecting",
            Self::Connected { .. } => "connected",
        }
    }

    fn current_id(&self) -> Option<u64> {
        match self {
            Self::Disconnected => None,
            Self::Connecting { id, .. } | Self::Connected { id, .. } => Some(*id),
        }
    }
}

struct PendingRequest {
    kind: CommandKind,
    command: Command,
    resolve: oneshot::Sender<Result<CommandResult, PiClientError>>,
}

type SharedOutcome = Shared<BoxFuture<'static, Result<(), PiClientError>>>;

struct Inner {
    disposed: bool,
    request_sequence: u64,
    conn_sequence: u64,
    pending_requests: HashMap<String, PendingRequest>,
    session_lease_counts: HashMap<String, usize>,
    exclusive_session_leases: HashMap<String, u64>,
    session_lease_generations: HashMap<String, u64>,
    session_cleanup_required: HashSet<String>,
    session_attachments: HashMap<String, SharedOutcome>,
    session_detachments: HashMap<String, SharedOutcome>,
    session_reconciliations: HashMap<String, SharedOutcome>,
    state: ClientState,
    conn: ConnState,
    connection_state_listeners: Vec<(u64, ConnectionStateListener)>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            disposed: false,
            request_sequence: 0,
            conn_sequence: 0,
            pending_requests: HashMap::new(),
            session_lease_counts: HashMap::new(),
            exclusive_session_leases: HashMap::new(),
            session_lease_generations: HashMap::new(),
            session_cleanup_required: HashSet::new(),
            session_attachments: HashMap::new(),
            session_detachments: HashMap::new(),
            session_reconciliations: HashMap::new(),
            state: ClientState::default(),
            conn: ConnState::Disconnected,
            connection_state_listeners: Vec::new(),
        }
    }
}

impl Inner {
    fn is_connected(&self) -> bool {
        matches!(self.conn, ConnState::Connected { .. })
    }

    fn notify_connection_state(change: ConnectionStateChange, notifications: &mut Notifications) {
        notifications.connection_state.push(change);
    }

    fn reject_pending(&mut self, error: &PiClientError) {
        let pending = std::mem::take(&mut self.pending_requests);
        for (_, request) in pending {
            let _ = request.resolve.send(Err(error.clone()));
        }
    }

    fn take_handshake(&mut self) -> Option<HandshakeSender> {
        match &mut self.conn {
            ConnState::Connecting { handshake, .. } => handshake.take(),
            _ => None,
        }
    }

    /// Fails the current connection (upstream `#fail`): resolves the
    /// handshake with `error`, clears attachments, invalidates every lease,
    /// rejects every pending request, and queues the disconnected
    /// notification.
    fn fail(&mut self, error: PiClientError, notifications: &mut Notifications) {
        if matches!(self.conn, ConnState::Disconnected) {
            return;
        }
        if let Some(handshake) = self.take_handshake() {
            let failure = error.clone();
            notifications.handshake = Some(Box::new(move || {
                let _ = handshake.send(Err(failure));
            }));
        }
        self.conn = ConnState::Disconnected;
        self.state.clear_attachments();
        self.invalidate_all_session_leases();
        self.reject_pending(&error);
        Inner::notify_connection_state(
            ConnectionStateChange {
                state: ConnectionState::Disconnected,
                error: Some(error),
            },
            notifications,
        );
    }

    /// Fails the current connection and closes its transport (upstream
    /// `#failAndClose`).
    fn fail_and_close(&mut self, error: PiClientError, notifications: &mut Notifications) {
        let transport = match &self.conn {
            ConnState::Disconnected => None,
            ConnState::Connecting { transport, .. } => transport.clone(),
            ConnState::Connected { transport, .. } => Some(Arc::clone(transport)),
        };
        self.fail(error, notifications);
        if let Some(transport) = transport {
            transport.close();
        }
    }

    fn invalidate_session_leases(&mut self, session_id: &str) {
        self.session_lease_counts.remove(session_id);
        self.exclusive_session_leases.remove(session_id);
        self.session_cleanup_required.remove(session_id);
        let generation = self
            .session_lease_generations
            .entry(session_id.to_string())
            .or_insert(0);
        *generation += 1;
    }

    fn invalidate_all_session_leases(&mut self) {
        let session_ids: Vec<String> = self.session_lease_counts.keys().cloned().collect();
        for session_id in session_ids {
            self.invalidate_session_leases(&session_id);
        }
        self.session_cleanup_required.clear();
    }
}

// ---------------------------------------------------------------------------
// Lease machinery (port of upstream session-handle.ts + client leases)
// ---------------------------------------------------------------------------

/// Lease mode for acquiring a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLeaseMode {
    /// Multiple leases may coexist; the server detaches on final release.
    Shared,
    /// The sole lease; conflicts with any other lease.
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseState {
    Active,
    Releasing,
    Released,
    Invalidated,
}

struct LeaseEntry {
    session_id: String,
    token: u64,
    generation: u64,
    state: StdMutex<LeaseState>,
    release_future: StdMutex<Option<SharedOutcome>>,
}

/// Options for creating a session.
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    /// Working directory for the new session.
    pub cwd: Option<String>,
    /// Session name.
    pub name: Option<String>,
    /// Initial model.
    pub model: Option<ModelRef>,
    /// Initial thinking level.
    pub thinking_level: Option<ThinkingLevel>,
}

// ---------------------------------------------------------------------------
// Client core
// ---------------------------------------------------------------------------

struct ClientCore {
    factory: ByteTransportFactory,
    frame_options: FrameDecoderOptions,
    on_listener_error: Option<ListenerErrorHandler>,
    next_lease_token: AtomicU64,
    inner: StdMutex<Inner>,
}

/// Options for constructing a [`PiClient`].
#[derive(Clone)]
pub struct PiClientOptions {
    /// Creates one fresh transport per connection attempt.
    pub transport_factory: ByteTransportFactory,
    /// Maximum frame length in bytes (default 16 MiB).
    pub max_frame_length: Option<usize>,
    /// Reports isolated listener panics.
    pub on_listener_error: Option<ListenerErrorHandler>,
}

impl fmt::Debug for PiClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiClientOptions")
            .field("max_frame_length", &self.max_frame_length)
            .finish_non_exhaustive()
    }
}

/// A remote-session client over any [`ByteTransport`].
#[derive(Clone)]
pub struct PiClient {
    core: Arc<ClientCore>,
}

impl fmt::Debug for PiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiClient").finish_non_exhaustive()
    }
}

struct ConnHandlers {
    core: Arc<ClientCore>,
}

impl PiClient {
    /// Validates options and constructs a disconnected client.
    /// # Errors
    pub fn new(options: PiClientOptions) -> Result<Self, PiClientOptionsError> {
        let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        let value = u64::try_from(max_frame_length).unwrap_or(u64::MAX);
        if value == 0 || value > MAX_UINT32 {
            return Err(PiClientOptionsError::InvalidMaxFrameLength {
                value,
                max: MAX_UINT32,
            });
        }
        let core = Arc::new(ClientCore {
            factory: options.transport_factory,
            frame_options: FrameDecoderOptions { max_frame_length },
            on_listener_error: options.on_listener_error,
            next_lease_token: AtomicU64::new(1),
            inner: StdMutex::new(Inner::default()),
        });
        Ok(Self { core })
    }

    /// Opens a connection and completes the protocol handshake, returning
    /// the server snapshot.
    /// # Errors
    pub async fn connect(&self) -> Result<ServerSnapshot, PiClientError> {
        let (receiver, id) = {
            let mut inner = lock(&self.core.inner);
            if inner.disposed {
                return Err(PiClientError::Disposed(PiClientDisposedError));
            }
            if !matches!(inner.conn, ConnState::Disconnected) {
                return Err(PiClientError::disconnected(format!(
                    "Pi client is already {}",
                    inner.conn.name()
                )));
            }
            inner.state.reset();
            inner.conn_sequence += 1;
            let id = inner.conn_sequence;
            let decoder = ServerMessageDecoder::new(Some(self.core.frame_options))
                .map_err(|error| PiClientError::Protocol(ProtocolViolationError::from(error)))?;
            let (sender, receiver) = oneshot::channel();
            inner.conn = ConnState::Connecting {
                id,
                transport: None,
                handshake: Some(sender),
                decoder,
            };
            (receiver, id)
        };
        {
            let mut notifications = Notifications::default();
            {
                Inner::notify_connection_state(
                    ConnectionStateChange {
                        state: ConnectionState::Connecting,
                        error: None,
                    },
                    &mut notifications,
                );
            }
            deliver_all(&self.core, &mut notifications);
        }
        let core = Arc::clone(&self.core);
        let handlers: Arc<dyn ByteTransportHandlers> = Arc::new(ConnHandlers {
            core: Arc::clone(&self.core),
        });
        let factory = Arc::clone(&self.core.factory);
        tokio::spawn(async move {
            open_transport(core, id, handlers, factory).await;
        });
        receiver
            .await
            .unwrap_or_else(|_| Err(PiClientError::disconnected("connection attempt aborted")))
    }

    /// Reconnects after a disconnection.
    /// # Errors
    pub async fn reconnect(&self) -> Result<ServerSnapshot, PiClientError> {
        self.connect().await
    }

    /// Whether the handshake has completed on the current connection.
    #[must_use]
    pub fn connected(&self) -> bool {
        lock(&self.core.inner).is_connected()
    }

    /// Current connection lifecycle state.
    #[must_use]
    pub fn connection_state(&self) -> ConnectionState {
        match &lock(&self.core.inner).conn {
            ConnState::Disconnected => ConnectionState::Disconnected,
            ConnState::Connecting { .. } => ConnectionState::Connecting,
            ConnState::Connected { .. } => ConnectionState::Connected,
        }
    }

    /// Latest server snapshot, if connected.
    #[must_use]
    pub fn snapshot(&self) -> Option<ServerSnapshot> {
        lock(&self.core.inner).state.snapshot.clone()
    }

    /// Subscribes to server snapshots.
    /// # Errors
    pub fn subscribe(
        &self,
        listener: ServerSnapshotListener,
    ) -> Result<Subscription, PiClientError> {
        let mut inner = lock(&self.core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
        let id = inner.state.next_id();
        inner.state.snapshot_listeners.push((id, listener));
        let core = Arc::clone(&self.core);
        Ok(Subscription::new(move || {
            lock(&core.inner)
                .state
                .snapshot_listeners
                .retain(|(listener_id, _)| *listener_id != id);
        }))
    }

    /// Subscribes to unsolicited server events.
    /// # Errors
    pub fn on_event(&self, listener: ServerEventListener) -> Result<Subscription, PiClientError> {
        let mut inner = lock(&self.core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
        let id = inner.state.next_id();
        inner.state.event_listeners.push((id, listener));
        let core = Arc::clone(&self.core);
        Ok(Subscription::new(move || {
            lock(&core.inner)
                .state
                .event_listeners
                .retain(|(listener_id, _)| *listener_id != id);
        }))
    }

    /// Subscribes to connection-state transitions.
    /// # Errors
    pub fn on_connection_state_change(
        &self,
        listener: ConnectionStateListener,
    ) -> Result<Subscription, PiClientError> {
        let mut inner = lock(&self.core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
        let id = inner.state.next_id();
        inner.connection_state_listeners.push((id, listener));
        let core = Arc::clone(&self.core);
        Ok(Subscription::new(move || {
            lock(&core.inner)
                .connection_state_listeners
                .retain(|(listener_id, _)| *listener_id != id);
        }))
    }

    /// Lists sessions known to the server.
    /// # Errors
    pub async fn list_sessions(&self) -> Result<Vec<SessionMetadata>, PiClientError> {
        let result = request(&self.core, Command::List).await?;
        match result {
            CommandResult::List { sessions } => Ok(sessions),
            other => Err(unexpected_kind(CommandKind::List, &other)),
        }
    }

    /// Creates a session and returns an exclusive lease on it.
    /// # Errors
    pub async fn create_session(
        &self,
        options: CreateSessionOptions,
    ) -> Result<PiSessionHandle, PiClientError> {
        let result = request(
            &self.core,
            Command::Create {
                cwd: options.cwd,
                name: options.name,
                model: options.model,
                thinking_level: options.thinking_level,
            },
        )
        .await?;
        let session = match result {
            CommandResult::Create { session } => session,
            other => return Err(unexpected_kind(CommandKind::Create, &other)),
        };
        let token = reserve_lease(&self.core, &session.session_id, SessionLeaseMode::Exclusive)?;
        Ok(create_session_handle(&self.core, session.session_id, token))
    }

    /// Attaches to a session with a shared lease.
    /// # Errors
    pub async fn attach_session(&self, session_id: &str) -> Result<PiSessionHandle, PiClientError> {
        self.acquire_session(session_id, SessionLeaseMode::Shared)
            .await
    }

    /// Attaches to a session under the requested lease mode.
    /// # Errors
    pub async fn acquire_session(
        &self,
        session_id: &str,
        mode: SessionLeaseMode,
    ) -> Result<PiSessionHandle, PiClientError> {
        acquire_session(&self.core, session_id, mode).await
    }

    /// Disconnects the current connection; pending requests fail with the
    /// disconnected class.
    pub fn disconnect(&self, reason: &str) {
        let mut notifications = Notifications::default();
        {
            let mut inner = lock(&self.core.inner);
            if matches!(inner.conn, ConnState::Disconnected) {
                return;
            }
            inner.fail_and_close(PiClientError::disconnected(reason), &mut notifications);
        }
        deliver_all(&self.core, &mut notifications);
    }

    /// Disposes the client: idempotent, rejects pending requests with the
    /// disposed class, disconnects, and invalidates every lease.
    pub fn dispose(&self) {
        let mut notifications = Notifications::default();
        {
            let mut inner = lock(&self.core.inner);
            if inner.disposed {
                return;
            }
            inner.disposed = true;
            inner.reject_pending(&PiClientError::Disposed(PiClientDisposedError));
            inner.fail_and_close(
                PiClientError::Disposed(PiClientDisposedError),
                &mut notifications,
            );
            inner.state.dispose();
            inner.connection_state_listeners.clear();
        }
        deliver_all(&self.core, &mut notifications);
    }
}

impl Drop for PiClient {
    fn drop(&mut self) {
        self.dispose();
    }
}

// ---------------------------------------------------------------------------
// Connection open + handlers
// ---------------------------------------------------------------------------

async fn open_transport(
    core: Arc<ClientCore>,
    connect_id: u64,
    handlers: Arc<dyn ByteTransportHandlers>,
    factory: ByteTransportFactory,
) {
    let transport = match factory(handlers).await {
        Ok(transport) => transport,
        Err(error) => {
            fail_if_current(&core, connect_id, to_disconnected(&error)).await;
            return;
        }
    };
    let stored = {
        let mut inner = lock(&core.inner);
        match &mut inner.conn {
            ConnState::Connecting {
                id,
                transport: slot,
                ..
            } if *id == connect_id => {
                *slot = Some(Arc::clone(&transport));
                true
            }
            _ => false,
        }
    };
    if !stored {
        transport.close();
        return;
    }
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
    };
    let frame = match encode_client_message(&hello, Some(core.frame_options)) {
        Ok(frame) => frame,
        Err(error) => {
            let error = PiClientError::Protocol(ProtocolViolationError::from(error));
            let mut notifications = Notifications::default();
            {
                let mut inner = lock(&core.inner);
                inner.fail_and_close(error, &mut notifications);
            }
            deliver_all(&core, &mut notifications);
            return;
        }
    };
    if let Err(error) = transport.send(frame).await {
        fail_if_current(&core, connect_id, to_disconnected(&error)).await;
    }
}

#[expect(
    clippy::unused_async,
    reason = "async for API symmetry with the connection lifecycle; callers .await for consistent error propagation ordering"
)]
async fn fail_if_current(core: &Arc<ClientCore>, connect_id: u64, error: PiClientError) {
    let mut notifications = Notifications::default();
    {
        let mut inner = lock(&core.inner);
        if inner.conn.current_id() == Some(connect_id) {
            inner.fail_and_close(error, &mut notifications);
        }
    }
    deliver_all(core, &mut notifications);
}

impl ByteTransportHandlers for ConnHandlers {
    fn on_data(&self, chunk: Vec<u8>) {
        let mut notifications = Notifications::default();
        {
            let mut inner = lock(&self.core.inner);
            if inner.conn.current_id().is_none() {
                return;
            }
            if matches!(
                &inner.conn,
                ConnState::Connecting {
                    transport: None,
                    ..
                }
            ) {
                let error = PiClientError::protocol(
                    "Received server data before the client hello was sent",
                );
                inner.fail_and_close(error, &mut notifications);
                return;
            }
            let messages = {
                match &mut inner.conn {
                    ConnState::Connecting { decoder, .. }
                    | ConnState::Connected { decoder, .. } => decoder.push(&chunk),
                    ConnState::Disconnected => return,
                }
            };
            let messages = match messages {
                Ok(messages) => messages,
                Err(error) => {
                    let error = PiClientError::Protocol(ProtocolViolationError::from(error));
                    inner.fail_and_close(error, &mut notifications);
                    return;
                }
            };
            for message in messages {
                if matches!(inner.conn, ConnState::Disconnected) {
                    break;
                }
                handle_server_message(&mut inner, message, &mut notifications);
            }
        }
        deliver_all(&self.core, &mut notifications);
    }

    fn on_close(&self) {
        let mut notifications = Notifications::default();
        {
            let mut inner = lock(&self.core.inner);
            if matches!(inner.conn, ConnState::Disconnected) {
                return;
            }
            let end_result = match &mut inner.conn {
                ConnState::Connecting { decoder, .. } | ConnState::Connected { decoder, .. } => {
                    decoder.end()
                }
                ConnState::Disconnected => Ok(()),
            };
            let error = match end_result {
                Ok(()) => PiClientError::disconnected("Byte transport closed"),
                Err(error) => PiClientError::Protocol(ProtocolViolationError::from(error)),
            };
            inner.fail(error, &mut notifications);
        }
        deliver_all(&self.core, &mut notifications);
    }

    fn on_error(&self, error: TransportError) {
        let mut notifications = Notifications::default();
        {
            let mut inner = lock(&self.core.inner);
            if matches!(inner.conn, ConnState::Disconnected) {
                return;
            }
            inner.fail_and_close(to_disconnected(&error), &mut notifications);
        }
        deliver_all(&self.core, &mut notifications);
    }
}

fn handle_server_message(
    inner: &mut Inner,
    message: ServerMessage,
    notifications: &mut Notifications,
) {
    match &inner.conn {
        ConnState::Connecting { .. } => match message {
            ServerMessage::HelloError { error } => {
                let error = PiClientError::Server(PiServerError {
                    code: error.code,
                    message: error.message,
                    details: error.details,
                });
                inner.fail_and_close(error, notifications);
            }
            ServerMessage::Hello { snapshot, .. } => {
                let handshake = match std::mem::replace(&mut inner.conn, ConnState::Disconnected) {
                    ConnState::Connecting {
                        id,
                        transport: Some(transport),
                        handshake,
                        decoder,
                    } => {
                        inner.conn = ConnState::Connected {
                            id,
                            transport,
                            decoder,
                        };
                        handshake
                    }
                    other => {
                        inner.conn = other;
                        let error = PiClientError::protocol(
                            "Received server hello before the client hello was sent",
                        );
                        inner.fail_and_close(error, notifications);
                        return;
                    }
                };
                inner.state.apply_server_snapshot(&snapshot, notifications);
                Inner::notify_connection_state(
                    ConnectionStateChange {
                        state: ConnectionState::Connected,
                        error: None,
                    },
                    notifications,
                );
                if let Some(sender) = handshake {
                    notifications.handshake = Some(Box::new(move || {
                        let _ = sender.send(Ok(snapshot));
                    }));
                }
            }
            _ => {
                let error = PiClientError::protocol("Expected server hello as first message");
                inner.fail_and_close(error, notifications);
            }
        },
        ConnState::Connected { .. } => match message {
            ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
                let error = PiClientError::protocol("Unexpected handshake message");
                inner.fail_and_close(error, notifications);
            }
            ServerMessage::Response {
                id,
                ok,
                result,
                error,
            } => {
                handle_response(inner, &id, ok, result, error, notifications);
            }
            ServerMessage::Event { event } => {
                if let ServerEvent::SessionRemoved { ref session_id } = event {
                    inner.invalidate_session_leases(session_id);
                }
                inner.state.apply_event(&event, notifications);
            }
        },
        ConnState::Disconnected => {}
    }
}

fn handle_response(
    inner: &mut Inner,
    id: &str,
    ok: bool,
    result: Option<CommandResult>,
    error: Option<ProtocolError>,
    notifications: &mut Notifications,
) {
    let violation = PiClientError::protocol("Response has no matching request");
    let Some(pending) = inner.pending_requests.remove(id) else {
        inner.fail_and_close(violation, notifications);
        return;
    };
    if !ok {
        let mapped = match error {
            Some(error) => map_server_error(error, &pending),
            None => PiClientError::protocol("Failed response without an error"),
        };
        let _ = pending.resolve.send(Err(mapped));
        return;
    }
    let Some(result) = result else {
        let violation = PiClientError::protocol(format!(
            "Response has no result matching {}",
            pending.kind.name()
        ));
        let _ = pending.resolve.send(Err(violation.clone()));
        inner.fail_and_close(violation, notifications);
        return;
    };
    if result_kind(&result) != pending.kind {
        let violation = PiClientError::protocol(format!(
            "Response command {} does not match {}",
            result_kind(&result).name(),
            pending.kind.name()
        ));
        let _ = pending.resolve.send(Err(violation.clone()));
        inner.fail_and_close(violation, notifications);
        return;
    }
    inner.state.apply_result(&result, notifications);
    let _ = pending.resolve.send(Ok(result));
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

async fn send_frame(core: &Arc<ClientCore>, frame: Vec<u8>) -> Result<(), PiClientError> {
    let transport = {
        let inner = lock(&core.inner);
        match &inner.conn {
            ConnState::Connected { transport, .. } => Arc::clone(transport),
            _ => return Err(PiClientError::disconnected("Pi client is disconnected")),
        }
    };
    match transport.send(frame).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = to_disconnected(&error);
            fail_if_current_by_transport(core, &transport, error.clone()).await;
            Err(error)
        }
    }
}

#[expect(
    clippy::unused_async,
    reason = "async for API symmetry with the connection lifecycle; callers .await for consistent error propagation ordering"
)]
async fn fail_if_current_by_transport(
    core: &Arc<ClientCore>,
    transport: &Arc<dyn ByteTransport>,
    error: PiClientError,
) {
    let mut notifications = Notifications::default();
    {
        let mut inner = lock(&core.inner);
        let current = match &inner.conn {
            ConnState::Connected {
                transport: current, ..
            }
            | ConnState::Connecting {
                transport: Some(current),
                ..
            } => Arc::ptr_eq(current, transport),
            _ => false,
        };
        if current {
            inner.fail_and_close(error, &mut notifications);
        }
    }
    deliver_all(core, &mut notifications);
}

async fn request(core: &Arc<ClientCore>, command: Command) -> Result<CommandResult, PiClientError> {
    let (id, receiver) = {
        let mut inner = lock(&core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
        if !inner.is_connected() {
            return Err(PiClientError::disconnected("Pi client is disconnected"));
        }
        inner.request_sequence += 1;
        let id = format!("request-{}", inner.request_sequence);
        let (sender, receiver) = oneshot::channel();
        let kind = command_kind(&command);
        inner.pending_requests.insert(
            id.clone(),
            PendingRequest {
                kind,
                command: command.clone(),
                resolve: sender,
            },
        );
        (id, receiver)
    };
    let message = ClientMessage::Request {
        id,
        request: command.clone(),
    };
    let frame = match encode_client_message(&message, Some(core.frame_options)) {
        Ok(frame) => frame,
        Err(error) => {
            let error = PiClientError::Protocol(ProtocolViolationError::from(error));
            if let Some(pending) = lock(&core.inner)
                .pending_requests
                .remove(&message_id(&message))
            {
                let _ = pending.resolve.send(Err(error.clone()));
            }
            return Err(error);
        }
    };
    if let Err(error) = send_frame(core, frame).await {
        return match receiver.await {
            Ok(result) => result,
            Err(_) => Err(error),
        };
    }
    receiver
        .await
        .unwrap_or_else(|_| Err(PiClientError::disconnected("request dropped")))
}

fn message_id(message: &ClientMessage) -> String {
    match message {
        ClientMessage::Request { id, .. } => id.clone(),
        ClientMessage::Hello { .. } => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Deliveries
// ---------------------------------------------------------------------------

fn deliver_all(core: &Arc<ClientCore>, notifications: &mut Notifications) {
    let state_listeners: Vec<ConnectionStateListener> = lock(&core.inner)
        .connection_state_listeners
        .iter()
        .map(|(_, listener)| Arc::clone(listener))
        .collect();
    for change in std::mem::take(&mut notifications.connection_state) {
        for listener in &state_listeners {
            invoke_isolated(listener, &change, core.on_listener_error.as_ref());
        }
    }
    if let Some(send) = notifications.handshake.take() {
        send();
    }
    notifications.deliver(core.on_listener_error.as_ref());
}

// ---------------------------------------------------------------------------
// Leases and session handles
// ---------------------------------------------------------------------------

fn reserve_lease(
    core: &Arc<ClientCore>,
    session_id: &str,
    mode: SessionLeaseMode,
) -> Result<u64, PiClientError> {
    let mut inner = lock(&core.inner);
    let count = inner
        .session_lease_counts
        .get(session_id)
        .copied()
        .unwrap_or(0);
    if mode == SessionLeaseMode::Exclusive && count > 0 {
        return Err(PiClientError::Ownership(PiSessionOwnershipError {
            session_id: Some(session_id.to_string()),
            message: format!("Session {session_id} already has an active lease"),
        }));
    }
    if mode == SessionLeaseMode::Shared && inner.exclusive_session_leases.contains_key(session_id) {
        return Err(PiClientError::Ownership(PiSessionOwnershipError {
            session_id: Some(session_id.to_string()),
            message: format!("Session {session_id} has an exclusive lease"),
        }));
    }
    let token = core.next_lease_token.fetch_add(1, Ordering::SeqCst);
    inner
        .session_lease_counts
        .insert(session_id.to_string(), count + 1);
    if mode == SessionLeaseMode::Exclusive {
        inner
            .exclusive_session_leases
            .insert(session_id.to_string(), token);
    }
    Ok(token)
}

fn release_lease_by_token(core: &Arc<ClientCore>, session_id: &str, token: u64) {
    let mut inner = lock(&core.inner);
    let count = inner
        .session_lease_counts
        .get(session_id)
        .copied()
        .unwrap_or(0);
    if count <= 1 {
        inner.session_lease_counts.remove(session_id);
    } else {
        inner
            .session_lease_counts
            .insert(session_id.to_string(), count - 1);
    }
    if inner.exclusive_session_leases.get(session_id) == Some(&token) {
        inner.exclusive_session_leases.remove(session_id);
    }
}

fn create_session_handle(
    core: &Arc<ClientCore>,
    session_id: String,
    token: u64,
) -> PiSessionHandle {
    let generation = lock(&core.inner)
        .session_lease_generations
        .get(&session_id)
        .copied()
        .unwrap_or(0);
    PiSessionHandle {
        core: Arc::clone(core),
        lease: Arc::new(LeaseEntry {
            session_id,
            token,
            generation,
            state: StdMutex::new(LeaseState::Active),
            release_future: StdMutex::new(None),
        }),
    }
}

fn refresh_lease_state(core: &Arc<ClientCore>, lease: &Arc<LeaseEntry>) {
    let mut state = lock(&lease.state);
    if matches!(*state, LeaseState::Active | LeaseState::Releasing) {
        let current = lock(&core.inner)
            .session_lease_generations
            .get(&lease.session_id)
            .copied()
            .unwrap_or(0);
        if current != lease.generation {
            *state = LeaseState::Invalidated;
        }
    }
}

fn lease_is_active(core: &Arc<ClientCore>, lease: &Arc<LeaseEntry>) -> bool {
    refresh_lease_state(core, lease);
    *lock(&lease.state) == LeaseState::Active
        && lock(&core.inner)
            .state
            .is_session_attached(&lease.session_id)
}

fn assert_lease_active(
    core: &Arc<ClientCore>,
    lease: &Arc<LeaseEntry>,
) -> Result<(), PiClientError> {
    {
        let inner = lock(&core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
        if !inner.is_connected() {
            return Err(PiClientError::disconnected("Pi client is disconnected"));
        }
    }
    if !lease_is_active(core, lease) {
        return Err(PiClientError::SessionDetached(PiSessionDetachedError::new(
            lease.session_id.clone(),
        )));
    }
    Ok(())
}

async fn acquire_session(
    core: &Arc<ClientCore>,
    session_id: &str,
    mode: SessionLeaseMode,
) -> Result<PiSessionHandle, PiClientError> {
    {
        let inner = lock(&core.inner);
        if inner.disposed {
            return Err(PiClientError::Disposed(PiClientDisposedError));
        }
    }
    let token = reserve_lease(core, session_id, mode)?;
    match acquire_after_reserve(core, session_id, token).await {
        Ok(handle) => Ok(handle),
        Err(error) => {
            release_lease_by_token(core, session_id, token);
            Err(error)
        }
    }
}

async fn acquire_after_reserve(
    core: &Arc<ClientCore>,
    session_id: &str,
    token: u64,
) -> Result<PiSessionHandle, PiClientError> {
    let detachment = lock(&core.inner)
        .session_detachments
        .get(session_id)
        .cloned();
    if let Some(detachment) = detachment {
        let _ = detachment.await;
    }
    let needs_reconcile = lock(&core.inner)
        .session_cleanup_required
        .contains(session_id);
    if needs_reconcile {
        reconcile_cleanup(core, session_id).await?;
    }
    let attached = lock(&core.inner).state.is_session_attached(session_id);
    if needs_reconcile || !attached {
        let attachment = {
            let mut inner = lock(&core.inner);
            if let Some(existing) = inner.session_attachments.get(session_id) {
                existing.clone()
            } else {
                let future: SharedOutcome = attach_session_shared(core, session_id).shared();
                inner
                    .session_attachments
                    .insert(session_id.to_string(), future.clone());
                future
            }
        };
        let outcome = attachment.await;
        lock(&core.inner).session_attachments.remove(session_id);
        outcome?;
    }
    Ok(create_session_handle(core, session_id.to_string(), token))
}

fn attach_session_shared(
    core: &Arc<ClientCore>,
    session_id: &str,
) -> BoxFuture<'static, Result<(), PiClientError>> {
    let core = Arc::clone(core);
    let session_id = session_id.to_string();
    Box::pin(async move {
        let previous = lock(&core.inner).state.forget_session_snapshot(&session_id);
        let outcome = request(
            &core,
            Command::Attach {
                session_id: session_id.clone(),
            },
        )
        .await;
        match outcome {
            Ok(_) => Ok(()),
            Err(error) => {
                if let Some(previous) = previous {
                    lock(&core.inner).state.restore_session_snapshot(previous);
                }
                Err(error)
            }
        }
    })
}

fn reconcile_cleanup(
    core: &Arc<ClientCore>,
    session_id: &str,
) -> BoxFuture<'static, Result<(), PiClientError>> {
    let core = Arc::clone(core);
    let session_id = session_id.to_string();
    Box::pin(async move {
        if !lock(&core.inner)
            .session_cleanup_required
            .contains(&session_id)
        {
            return Ok(());
        }
        let future = {
            let mut inner = lock(&core.inner);
            if let Some(existing) = inner.session_reconciliations.get(&session_id) {
                existing.clone()
            } else {
                let core = Arc::clone(&core);
                let sid = session_id.clone();
                let future: SharedOutcome = async move {
                    let outcome = request(
                        &core,
                        Command::Detach {
                            session_id: sid.clone(),
                        },
                    )
                    .await;
                    let result = outcome.map(|_| ());
                    if result.is_ok() {
                        lock(&core.inner).session_cleanup_required.remove(&sid);
                    }
                    result
                }
                .boxed()
                .shared();
                inner
                    .session_reconciliations
                    .insert(session_id.clone(), future.clone());
                future
            }
        };
        let outcome = future.await;
        lock(&core.inner)
            .session_reconciliations
            .remove(&session_id);
        outcome
    })
}

fn release_lease(
    core: &Arc<ClientCore>,
    lease: &Arc<LeaseEntry>,
    relinquish_on_failure: bool,
) -> BoxFuture<'static, Result<(), PiClientError>> {
    let core = Arc::clone(core);
    let lease = Arc::clone(lease);
    Box::pin(async move {
        refresh_lease_state(&core, &lease);
        {
            let state = *lock(&lease.state);
            if matches!(state, LeaseState::Released | LeaseState::Invalidated) {
                return Ok(());
            }
        }
        let in_flight = lock(&lease.release_future).clone();
        if let Some(shared) = in_flight {
            return shared.await;
        }
        assert_lease_active(&core, &lease)?;
        *lock(&lease.state) = LeaseState::Releasing;
        let shared: SharedOutcome =
            release_outer(Arc::clone(&core), Arc::clone(&lease), relinquish_on_failure)
                .boxed()
                .shared();
        *lock(&lease.release_future) = Some(shared.clone());
        shared.await
    })
}

async fn release_outer(
    core: Arc<ClientCore>,
    lease: Arc<LeaseEntry>,
    relinquish_on_failure: bool,
) -> Result<(), PiClientError> {
    match release_inner(&core, &lease).await {
        Ok(()) => Ok(()),
        Err(error) => {
            refresh_lease_state(&core, &lease);
            if matches!(*lock(&lease.state), LeaseState::Invalidated) {
                return Ok(());
            }
            if relinquish_on_failure {
                release_lease_by_token(&core, &lease.session_id, lease.token);
                lock(&core.inner)
                    .session_cleanup_required
                    .insert(lease.session_id.clone());
                *lock(&lease.state) = LeaseState::Released;
            } else {
                *lock(&lease.state) = LeaseState::Active;
                *lock(&lease.release_future) = None;
            }
            Err(error)
        }
    }
}

async fn release_inner(
    core: &Arc<ClientCore>,
    lease: &Arc<LeaseEntry>,
) -> Result<(), PiClientError> {
    let session_id = lease.session_id.clone();
    let count = lock(&core.inner)
        .session_lease_counts
        .get(&session_id)
        .copied()
        .unwrap_or(0);
    if count <= 1 {
        let detachment: SharedOutcome = {
            let request_core = Arc::clone(core);
            let sid = session_id.clone();
            let future: SharedOutcome = async move {
                request(&request_core, Command::Detach { session_id: sid })
                    .await
                    .map(|_| ())
            }
            .boxed()
            .shared();
            lock(&core.inner)
                .session_detachments
                .insert(session_id.clone(), future.clone());
            future
        };
        let outcome = detachment.await;
        lock(&core.inner).session_detachments.remove(&session_id);
        outcome?;
        release_lease_by_token(core, &lease.session_id, lease.token);
    } else {
        release_lease_by_token(core, &lease.session_id, lease.token);
    }
    *lock(&lease.state) = LeaseState::Released;
    Ok(())
}

/// A lease on one attached remote session (port of upstream
/// `SessionHandle`). Dropping the handle does **not** release the lease —
/// call [`PiSessionHandle::detach`] or [`PiSessionHandle::dispose`].
#[derive(Clone)]
pub struct PiSessionHandle {
    core: Arc<ClientCore>,
    lease: Arc<LeaseEntry>,
}

impl fmt::Debug for PiSessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiSessionHandle")
            .field("id", &self.lease.session_id)
            .finish_non_exhaustive()
    }
}

impl PiSessionHandle {
    /// Session identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.lease.session_id
    }

    /// Whether the lease is active and the session attached.
    #[must_use]
    pub fn attached(&self) -> bool {
        lease_is_active(&self.core, &self.lease)
    }

    /// Alias of [`PiSessionHandle::attached`].
    #[must_use]
    pub fn active(&self) -> bool {
        self.attached()
    }

    /// Latest session snapshot while the lease is active.
    #[must_use]
    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        if lease_is_active(&self.core, &self.lease) {
            lock(&self.core.inner)
                .state
                .session_snapshots
                .get(&self.lease.session_id)
                .cloned()
        } else {
            None
        }
    }

    /// Subscribes to snapshots for this session while the lease is active.
    /// # Errors
    pub fn subscribe(
        &self,
        listener: SessionSnapshotListener,
    ) -> Result<Subscription, PiClientError> {
        assert_lease_active(&self.core, &self.lease)?;
        let core = Arc::clone(&self.core);
        let lease = Arc::clone(&self.lease);
        let wrapped: SessionSnapshotListener = Arc::new(move |snapshot| {
            if lease_is_active(&core, &lease) {
                listener(snapshot);
            }
        });
        let session_id = self.lease.session_id.clone();
        let subscription = {
            let mut inner = lock(&self.core.inner);
            let id = inner.state.next_id();
            inner
                .state
                .session_snapshot_listeners
                .entry(session_id.clone())
                .or_default()
                .push((id, wrapped));
            let core = Arc::clone(&self.core);
            Subscription::new(move || {
                let mut inner = lock(&core.inner);
                inner
                    .state
                    .unregister_session_snapshot_listener(&session_id, id);
            })
        };
        Ok(subscription)
    }

    /// Subscribes to events for this session while the lease is active;
    /// `session_removed` is always delivered.
    /// # Errors
    pub fn on_event(&self, listener: SessionEventListener) -> Result<Subscription, PiClientError> {
        assert_lease_active(&self.core, &self.lease)?;
        let core = Arc::clone(&self.core);
        let lease = Arc::clone(&self.lease);
        let wrapped: SessionEventListener = Arc::new(move |event| {
            let removed = matches!(event, ServerEvent::SessionRemoved { .. });
            if removed || lease_is_active(&core, &lease) {
                listener(event);
            }
        });
        let session_id = self.lease.session_id.clone();
        let subscription = {
            let mut inner = lock(&self.core.inner);
            let id = inner.state.next_id();
            inner
                .state
                .session_event_listeners
                .entry(session_id.clone())
                .or_default()
                .push((id, wrapped));
            let core = Arc::clone(&self.core);
            Subscription::new(move || {
                let mut inner = lock(&core.inner);
                inner
                    .state
                    .unregister_session_event_listener(&session_id, id);
            })
        };
        Ok(subscription)
    }

    /// Releases the lease; the server detaches once the final lease for the
    /// session is released. Detach is **not** disconnect: the client stays
    /// connected.
    /// # Errors
    #[must_use = "the lease is only released once the returned future is awaited"]
    pub fn detach(&self) -> BoxFuture<'static, Result<(), PiClientError>> {
        release_lease(&self.core, &self.lease, false)
    }

    /// Releases the lease, relinquishing it even when the protocol detach
    /// fails (marking the session for cleanup on reacquire).
    /// # Errors
    #[expect(
        clippy::must_use_candidate,
        reason = "returns a BoxFuture that must be awaited; not annotating with #[must_use] because Drop impls call dispose without awaiting"
    )]
    pub fn dispose(&self) -> BoxFuture<'static, Result<(), PiClientError>> {
        release_lease(&self.core, &self.lease, true)
    }

    /// Sends a prompt and returns the resulting snapshot.
    /// # Errors
    pub async fn prompt(&self, text: String) -> Result<SessionSnapshot, PiClientError> {
        let result = self
            .session_request(Command::Prompt {
                session_id: self.lease.session_id.clone(),
                text,
            })
            .await?;
        match result {
            CommandResult::Prompt { session } => Ok(session),
            other => Err(unexpected_kind(CommandKind::Prompt, &other)),
        }
    }

    /// Steers an in-flight prompt.
    /// # Errors
    pub async fn steer(&self, text: String) -> Result<SessionSnapshot, PiClientError> {
        let result = self
            .session_request(Command::Steer {
                session_id: self.lease.session_id.clone(),
                text,
            })
            .await?;
        match result {
            CommandResult::Steer { session } => Ok(session),
            other => Err(unexpected_kind(CommandKind::Steer, &other)),
        }
    }

    /// Aborts the in-flight prompt.
    /// # Errors
    pub async fn abort(&self) -> Result<SessionSnapshot, PiClientError> {
        let result = self
            .session_request(Command::Abort {
                session_id: self.lease.session_id.clone(),
            })
            .await?;
        match result {
            CommandResult::Abort { session } => Ok(session),
            other => Err(unexpected_kind(CommandKind::Abort, &other)),
        }
    }

    /// Switches the session model.
    /// # Errors
    pub async fn set_model(&self, model: ModelRef) -> Result<SessionSnapshot, PiClientError> {
        let result = self
            .session_request(Command::SetModel {
                session_id: self.lease.session_id.clone(),
                model,
            })
            .await?;
        match result {
            CommandResult::SetModel { session } => Ok(session),
            other => Err(unexpected_kind(CommandKind::SetModel, &other)),
        }
    }

    /// Switches the session thinking level.
    /// # Errors
    pub async fn set_thinking(
        &self,
        thinking_level: ThinkingLevel,
    ) -> Result<SessionSnapshot, PiClientError> {
        let result = self
            .session_request(Command::SetThinking {
                session_id: self.lease.session_id.clone(),
                thinking_level,
            })
            .await?;
        match result {
            CommandResult::SetThinking { session } => Ok(session),
            other => Err(unexpected_kind(CommandKind::SetThinking, &other)),
        }
    }

    async fn session_request(&self, command: Command) -> Result<CommandResult, PiClientError> {
        assert_lease_active(&self.core, &self.lease)?;
        request(&self.core, command).await
    }
}

impl ClientState {
    fn unregister_session_snapshot_listener(&mut self, session_id: &str, id: u64) {
        if let Some(listeners) = self.session_snapshot_listeners.get_mut(session_id) {
            listeners.retain(|(listener_id, _)| *listener_id != id);
            if listeners.is_empty() {
                self.session_snapshot_listeners.remove(session_id);
            }
        }
    }

    fn unregister_session_event_listener(&mut self, session_id: &str, id: u64) {
        if let Some(listeners) = self.session_event_listeners.get_mut(session_id) {
            listeners.retain(|(listener_id, _)| *listener_id != id);
            if listeners.is_empty() {
                self.session_event_listeners.remove(session_id);
            }
        }
    }
}

fn unexpected_kind(expected: CommandKind, actual: &CommandResult) -> PiClientError {
    PiClientError::protocol(format!(
        "Response command {} does not match {}",
        result_kind(actual).name(),
        expected.name()
    ))
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Tests: scripted in-memory server harness + acceptance suite
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use crate::remote::codec::{ClientMessageDecoder, encode_server_message};
    use crate::remote::transport::{
        EndpointSpec, InMemoryListener, InMemoryTransport, build_transport,
    };
    use tokio::sync::Notify;

    /// Decodes client chunks and records decoded messages.
    struct ServerCore {
        messages: StdMutex<Vec<ClientMessage>>,
        decoder: StdMutex<ClientMessageDecoder>,
        notify: Notify,
    }

    impl ServerCore {
        #[expect(clippy::expect_used, reason = "test helper: new must succeed")]
        fn new() -> Self {
            Self {
                messages: StdMutex::new(Vec::new()),
                decoder: StdMutex::new(
                    ClientMessageDecoder::new(None).expect("client message decoder"),
                ),
                notify: Notify::new(),
            }
        }

        fn push_chunk(&self, chunk: &[u8]) {
            let mut decoder = lock(&self.decoder);
            if let Ok(messages) = decoder.push(chunk) {
                lock(&self.messages).extend(messages);
                self.notify.notify_one();
            }
        }

        async fn next_message(&self) -> ClientMessage {
            loop {
                {
                    let mut messages = lock(&self.messages);
                    if let Some(first) = messages.first().cloned() {
                        messages.remove(0);
                        return first;
                    }
                }
                self.notify.notified().await;
            }
        }

        fn request_count(&self) -> usize {
            lock(&self.messages).len()
        }

        fn next_request(self: &Arc<Self>) -> BoxFuture<'static, (String, Command)> {
            let core = Arc::clone(self);
            Box::pin(async move {
                loop {
                    match core.next_message().await {
                        ClientMessage::Request { id, request } => return (id, request),
                        ClientMessage::Hello { .. } => {}
                    }
                }
            })
        }
    }

    struct ServerHandlers {
        core: Arc<ServerCore>,
    }

    impl ByteTransportHandlers for ServerHandlers {
        fn on_data(&self, chunk: Vec<u8>) {
            self.core.push_chunk(&chunk);
        }
        fn on_close(&self) {}
        fn on_error(&self, _error: TransportError) {}
    }

    /// Scripted server side of an in-memory connection.
    struct TestServer {
        transport: InMemoryTransport,
        core: Arc<ServerCore>,
    }

    impl TestServer {
        #[expect(clippy::expect_used, reason = "test helper: send must succeed")]
        async fn send(&self, message: ServerMessage) {
            let frame = encode_server_message(&message, None).expect("encode server message");
            self.transport.send(frame).await.expect("server send");
        }

        async fn respond(&self, id: &str, result: CommandResult) {
            self.send(ServerMessage::Response {
                id: id.to_string(),
                ok: true,
                result: Some(result),
                error: None,
            })
            .await;
        }

        async fn respond_error(&self, id: &str, code: ProtocolErrorCode, message: &str) {
            self.send(ServerMessage::Response {
                id: id.to_string(),
                ok: false,
                result: None,
                error: Some(ProtocolError {
                    code,
                    message: message.to_string(),
                    details: None,
                }),
            })
            .await;
        }

        #[expect(clippy::expect_used, reason = "test helper: send_raw must succeed")]
        async fn send_raw(&self, bytes: Vec<u8>) {
            self.transport.send(bytes).await.expect("server raw send");
        }

        fn close(&self) {
            self.transport.close();
        }

        fn fail(&self, message: &str) {
            self.transport
                .fail_peer(TransportError::Message(message.to_string()));
        }
    }

    fn base_server_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            server_id: "server-1".to_string(),
            protocol_version: PROTOCOL_VERSION,
            revision: 1,
            sessions: vec![],
            models: vec![],
        }
    }

    fn session_snapshot(session_id: &str, revision: u64) -> SessionSnapshot {
        SessionMetadata {
            session_id: session_id.to_string(),
            model: ModelRef {
                provider: "faux".to_string(),
                id: "model".to_string(),
            },
            thinking_level: ThinkingLevel::Off,
            locked: true,
            revision,
            transcript: vec![],
            queued_steer: vec![],
            queued_steer_count: 0,
        }
    }

    fn attach_result(session_id: &str, revision: u64) -> CommandResult {
        CommandResult::Attach {
            session: session_snapshot(session_id, revision),
        }
    }

    fn detach_result(session_id: &str) -> CommandResult {
        CommandResult::Detach {
            session_id: session_id.to_string(),
        }
    }

    #[expect(clippy::expect_used, reason = "test helper: make_client must succeed")]
    fn make_client(factory: ByteTransportFactory) -> PiClient {
        PiClient::new(PiClientOptions {
            transport_factory: factory,
            max_frame_length: None,
            on_listener_error: None,
        })
        .expect("client options")
    }

    #[expect(
        clippy::expect_used,
        reason = "test helper: connect_scripted must succeed"
    )]
    async fn connect_scripted() -> (PiClient, TestServer) {
        let (listener, endpoint) = InMemoryListener::new();
        let core = Arc::new(ServerCore::new());
        let client = make_client(
            build_transport(&EndpointSpec::InMemory { endpoint }).expect("in-memory factory"),
        );
        let connect = client.connect();
        let accept_core = Arc::clone(&core);
        let setup = async move {
            let transport = listener
                .accept(Arc::new(ServerHandlers { core: accept_core }))
                .await
                .expect("accept");
            let server = TestServer { transport, core };
            let hello = server.core.next_message().await;
            assert!(
                matches!(hello, ClientMessage::Hello { .. }),
                "first client message must be the hello"
            );
            server
                .send(ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    connection_id: "connection-1".to_string(),
                    snapshot: base_server_snapshot(),
                })
                .await;
            server
        };
        let (connected, server) = tokio::join!(connect, setup);
        connected.expect("client handshake");
        (client, server)
    }

    /// Accepts one connection, answers the client hello, and returns the
    /// scripted server.
    #[expect(dead_code, reason = "test utility: retained for future tests")]
    #[expect(clippy::expect_used, reason = "test helper: accept_hello must succeed")]
    async fn accept_hello(listener: Arc<InMemoryListener>, connection_id: &str) -> TestServer {
        let core = Arc::new(ServerCore::new());
        let accept_core = Arc::clone(&core);
        let transport = listener
            .accept(Arc::new(ServerHandlers { core: accept_core }))
            .await
            .expect("accept");
        let server = TestServer { transport, core };
        let _hello = server.core.next_message().await;
        server
            .send(ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                connection_id: connection_id.to_string(),
                snapshot: base_server_snapshot(),
            })
            .await;
        server
    }

    /// Attaches with a scripted attach round trip. Rust futures are lazy, so
    /// the client future and the server message wait must be polled
    /// concurrently.
    #[expect(clippy::expect_used, reason = "test helper: attach must succeed")]
    async fn attach(client: &PiClient, server: &TestServer, session_id: &str) -> PiSessionHandle {
        // Rust futures are lazy and join! only completes when every branch
        // does, so the scripted response must be sent from a branch inside
        // the join — answering after the join would deadlock the client
        // action that waits for the very response being sent.
        let attaching = client.attach_session(session_id);
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(
                matches!(command, Command::Attach { .. }),
                "expected attach request"
            );
            server.respond(&id, attach_result(session_id, 1)).await;
        };
        let (handle, ()) = tokio::join!(attaching, scripted);
        handle.expect("attach")
    }

    // -----------------------------------------------------------------------
    // Acceptance: lease conflict, detach-reattach, typed mid-request
    // disconnect
    // -----------------------------------------------------------------------

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn lease_conflicts_are_typed_ownership_errors() {
        let (client, server) = connect_scripted().await;
        let shared = attach(&client, &server, "session-1").await;
        assert!(shared.attached());

        let exclusive_conflict = client
            .acquire_session("session-1", SessionLeaseMode::Exclusive)
            .await
            .expect_err("expected error");
        match &exclusive_conflict {
            PiClientError::Ownership(error) => {
                assert_eq!(error.session_id.as_deref(), Some("session-1"));
                assert_eq!(
                    error.message,
                    "Session session-1 already has an active lease"
                );
            }
            other => panic!("expected ownership error, got {other:?}"),
        }

        detach_leases(&[shared], &server, &[("session-1", "first")]).await;

        let acquiring = client.acquire_session("session-1", SessionLeaseMode::Exclusive);
        let scripted = async {
            let (attach_id, attach_command) = server.core.next_request().await;
            assert!(matches!(attach_command, Command::Attach { .. }));
            server
                .respond(&attach_id, attach_result("session-1", 2))
                .await;
        };
        let (exclusive, ()) = tokio::join!(acquiring, scripted);
        let exclusive = exclusive.expect("exclusive acquire");

        let shared_conflict = client
            .attach_session("session-1")
            .await
            .expect_err("expected error");
        match &shared_conflict {
            PiClientError::Ownership(error) => {
                assert_eq!(error.message, "Session session-1 has an exclusive lease");
            }
            other => panic!("expected ownership error, got {other:?}"),
        }
        detach_leases(&[exclusive], &server, &[("session-1", "second")]).await;
    }

    /// Detaches every handle, answering each protocol detach request.
    #[expect(
        clippy::expect_used,
        reason = "test helper: detach_leases must succeed"
    )]
    async fn detach_leases(
        handles: &[PiSessionHandle],
        server: &TestServer,
        labels: &[(&str, &str)],
    ) {
        for (index, handle) in handles.iter().enumerate() {
            let detaching = handle.detach();
            let scripted = async {
                let (id, command) = server.core.next_request().await;
                assert!(
                    matches!(command, Command::Detach { .. }),
                    "expected detach for {}",
                    labels[index].1
                );
                server
                    .respond(&id, detach_result(handles[index].id()))
                    .await;
            };
            let (outcome, ()) = tokio::join!(detaching, scripted);
            outcome.expect("detach");
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn shared_lease_detaches_only_after_final_release() {
        let (client, server) = connect_scripted().await;
        let first = attach(&client, &server, "session-1").await;
        // A second shared lease on an already-attached session completes
        // locally without a protocol round trip, so it must not script a
        // server response (the script would wait forever).
        let second = client
            .attach_session("session-1")
            .await
            .expect("second shared attach");
        assert!(second.attached());

        // First release must NOT send a protocol detach.
        let (outcome, ()) = tokio::join!(first.detach(), async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        });
        outcome.expect("first detach");
        assert!(!first.attached());
        assert!(second.attached());
        assert_eq!(
            server.core.request_count(),
            0,
            "no protocol detach before the final lease"
        );

        let detaching = second.detach();
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(matches!(command, Command::Detach { .. }));
            server.respond(&id, detach_result("session-1")).await;
        };
        let (outcome, ()) = tokio::join!(detaching, scripted);
        outcome.expect("final detach");
        assert!(!second.attached());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn detach_is_not_disconnect_and_reattach_works() {
        let (client, server) = connect_scripted().await;
        let handle = attach(&client, &server, "session-1").await;
        let _other = attach(&client, &server, "session-2").await;

        let detaching = handle.detach();
        let scripted = async {
            let (id, _command) = server.core.next_request().await;
            server.respond(&id, detach_result("session-1")).await;
        };
        let (outcome, ()) = tokio::join!(detaching, scripted);
        outcome.expect("detach");

        assert_eq!(client.connection_state(), ConnectionState::Connected);
        assert!(!handle.attached());
        let error = handle.abort().await.expect_err("expected error");
        match &error {
            PiClientError::SessionDetached(e) => assert_eq!(e.session_id, "session-1"),
            other => panic!("expected session detached, got {other:?}"),
        }

        let reattached = attach(&client, &server, "session-1").await;
        assert!(reattached.attached());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn typed_mid_request_disconnect_on_orderly_close() {
        let (client, server) = connect_scripted().await;
        let pending = client.list_sessions();
        server.close();
        let error = pending.await.expect_err("expected error");
        match &error {
            PiClientError::Disconnected(e) => assert_eq!(e.message, "Byte transport closed"),
            other => panic!("expected disconnected error, got {other:?}"),
        }
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn typed_mid_request_disconnect_on_transport_error() {
        let (client, server) = connect_scripted().await;
        let pending = client.list_sessions();
        server.fail("read failed");
        let error = pending.await.expect_err("expected error");
        match &error {
            PiClientError::Disconnected(e) => assert_eq!(e.message, "read failed"),
            other => panic!("expected disconnected error, got {other:?}"),
        }
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    }

    // -----------------------------------------------------------------------
    // Acceptance: pinned transport-failure mapping table
    // -----------------------------------------------------------------------

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn transport_failure_table_maps_to_exactly_one_variant() {
        // Row 1: transport open failure → Disconnected.
        let (listener, endpoint) = InMemoryListener::new();
        let client =
            make_client(build_transport(&EndpointSpec::InMemory { endpoint }).expect("factory"));
        drop(listener);
        let error = client.connect().await.expect_err("expected error");
        assert!(
            matches!(error, PiClientError::Disconnected(_)),
            "open failure must map to Disconnected, got {error:?}"
        );

        let (client, server) = connect_scripted().await;

        // Row 2: server ok:false session_locked on a session-scoped request
        // → Ownership.
        let attaching = client.attach_session("locked-session");
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(matches!(command, Command::Attach { .. }));
            server
                .respond_error(&id, ProtocolErrorCode::SessionLocked, "Already attached")
                .await;
        };
        let (attaching, ()) = tokio::join!(attaching, scripted);
        let error = attaching.expect_err("expected error");
        match &error {
            PiClientError::Ownership(e) => {
                assert_eq!(e.session_id.as_deref(), Some("locked-session"));
                assert_eq!(e.message, "Already attached");
            }
            other => panic!("expected ownership error, got {other:?}"),
        }

        // Row 3: server ok:false with any other code → Server.
        let listing = client.list_sessions();
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(matches!(command, Command::List));
            server
                .respond_error(&id, ProtocolErrorCode::InvalidRequest, "retry")
                .await;
        };
        let (listing, ()) = tokio::join!(listing, scripted);
        let error = listing.expect_err("expected error");
        match &error {
            PiClientError::Server(e) => {
                assert_eq!(e.code, ProtocolErrorCode::InvalidRequest);
                assert_eq!(e.message, "retry");
            }
            other => panic!("expected server error, got {other:?}"),
        }

        // Row 4: oversized outbound encode → Protocol, nothing sent.
        let handle = attach(&client, &server, "session-1").await;
        let sent_before = server.core.request_count();
        let oversized = handle
            .prompt("x".repeat(DEFAULT_MAX_FRAME_LENGTH + 1))
            .await
            .expect_err("expected error");
        assert!(
            matches!(oversized, PiClientError::Protocol(_)),
            "encode failure must map to Protocol, got {oversized:?}"
        );
        assert_eq!(
            server.core.request_count(),
            sent_before,
            "no frame may be sent"
        );

        // Row 5: inbound undecodable frame → Protocol and disconnected.
        // A complete frame whose payload is the CBOR break byte (invalid
        // outside an indefinite-length item) fails decode immediately; a
        // bare prefix would just wait for more bytes.
        let pending = client.list_sessions();
        server.send_raw(vec![0, 0, 0, 1, 0xff]).await;
        let error = pending.await.expect_err("expected error");
        assert!(
            matches!(error, PiClientError::Protocol(_)),
            "decode failure must map to Protocol, got {error:?}"
        );
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn hello_error_maps_to_server_class() {
        let (listener, endpoint) = InMemoryListener::new();
        let core = Arc::new(ServerCore::new());
        let client =
            make_client(build_transport(&EndpointSpec::InMemory { endpoint }).expect("factory"));
        let connect = client.connect();
        let accept_core = Arc::clone(&core);
        let setup = async move {
            let transport = listener
                .accept(Arc::new(ServerHandlers { core: accept_core }))
                .await
                .expect("accept");
            let server = TestServer { transport, core };
            let _hello = server.core.next_message().await;
            server
                .send(ServerMessage::HelloError {
                    error: ProtocolError {
                        code: ProtocolErrorCode::Version,
                        message: "Unsupported protocol version".to_string(),
                        details: None,
                    },
                })
                .await;
            server
        };
        let (connected, _server) = tokio::join!(connect, setup);
        let error = connected.expect_err("expected error");
        match &error {
            PiClientError::Server(e) => {
                assert_eq!(e.code, ProtocolErrorCode::Version);
                assert_eq!(e.message, "Unsupported protocol version");
            }
            other => panic!("expected server error, got {other:?}"),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn request_on_closed_transport_maps_to_disconnected() {
        let (client, server) = connect_scripted().await;
        server.close();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let error = client.list_sessions().await.expect_err("expected error");
        assert!(
            matches!(error, PiClientError::Disconnected(_)),
            "request on a closed transport must map to Disconnected, got {error:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Correlation and protocol invariants
    // -----------------------------------------------------------------------

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn request_ids_are_monotonic() {
        let (client, server) = connect_scripted().await;
        let listing = client.list_sessions();
        let attaching = client.attach_session("session-1");
        let scripted = async {
            let (first_id, first_command) = server.core.next_request().await;
            let (second_id, second_command) = server.core.next_request().await;
            assert!(matches!(first_command, Command::List));
            assert!(matches!(second_command, Command::Attach { .. }));
            assert_eq!(first_id, "request-1");
            assert_eq!(second_id, "request-2");
            server
                .respond(&first_id, CommandResult::List { sessions: vec![] })
                .await;
            server
                .respond(&second_id, attach_result("session-1", 1))
                .await;
        };
        let (listed, attached, ()) = futures::join!(listing, attaching, scripted);
        assert!(listed.expect("list").is_empty());
        assert!(attached.expect("attach").attached());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn coalesced_out_of_order_responses_correlate() {
        let (client, server) = connect_scripted().await;
        let listing = client.list_sessions();
        let attaching = client.attach_session("session-1");
        let scripted = async {
            // join! polls futures in creation order, so the list request
            // (created first) is sent before the attach request.
            let (list_id, list_command) = server.core.next_request().await;
            let (attach_id, attach_command) = server.core.next_request().await;
            assert!(matches!(list_command, Command::List));
            assert!(matches!(attach_command, Command::Attach { .. }));
            // Respond in reverse completion order inside one chunk.
            let attach_frame = encode_server_message(
                &ServerMessage::Response {
                    id: attach_id,
                    ok: true,
                    result: Some(attach_result("session-1", 1)),
                    error: None,
                },
                None,
            )
            .expect("encode attach");
            let list_frame = encode_server_message(
                &ServerMessage::Response {
                    id: list_id,
                    ok: true,
                    result: Some(CommandResult::List { sessions: vec![] }),
                    error: None,
                },
                None,
            )
            .expect("encode list");
            let mut chunk = attach_frame;
            chunk.extend_from_slice(&list_frame);
            server.send_raw(chunk).await;
        };
        let (listed, attached, ()) = futures::join!(listing, attaching, scripted);

        assert!(listed.expect("list").is_empty());
        let handle = attached.expect("attach");
        assert!(handle.attached());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn mismatched_response_command_fails_the_connection() {
        let (client, server) = connect_scripted().await;
        let listing = client.list_sessions();
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(matches!(command, Command::List));
            server.respond(&id, attach_result("session-1", 1)).await;
        };
        let (listed, ()) = tokio::join!(listing, scripted);
        let error = listed.expect_err("expected error");
        match &error {
            PiClientError::Protocol(e) => {
                assert_eq!(e.message, "Response command attach does not match list");
            }
            other => panic!("expected protocol violation, got {other:?}"),
        }
        assert_eq!(client.connection_state(), ConnectionState::Disconnected);
    }

    // -----------------------------------------------------------------------
    // Listener subscription
    // -----------------------------------------------------------------------

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn listener_subscription_delivers_while_active_and_gates_after_detach() {
        let (client, server) = connect_scripted().await;
        let handle = attach(&client, &server, "session-1").await;

        let snapshots: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let events: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        // Subscriptions are RAII: the guard must outlive the events it
        // should observe, so it is bound instead of dropped on the spot.
        let snapshot_sink = Arc::clone(&snapshots);
        let _snapshot_guard = handle
            .subscribe(Arc::new(move |snapshot: &SessionSnapshot| {
                lock(&snapshot_sink).push(snapshot.revision);
            }))
            .expect("subscribe");
        let event_sink = Arc::clone(&events);
        let _event_guard = handle
            .on_event(Arc::new(move |_: &ServerEvent| {
                event_sink.fetch_add(1, AtomicOrdering::SeqCst);
            }))
            .expect("on_event");

        server
            .send(ServerMessage::Event {
                event: ServerEvent::SessionSnapshot {
                    snapshot: session_snapshot("session-1", 2),
                },
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(*lock(&snapshots), vec![2u64]);

        let detaching = handle.detach();
        let scripted = async {
            let (id, _command) = server.core.next_request().await;
            server.respond(&id, detach_result("session-1")).await;
        };
        let (outcome, ()) = tokio::join!(detaching, scripted);
        outcome.expect("detach");

        server
            .send(ServerMessage::Event {
                event: ServerEvent::SessionSnapshot {
                    snapshot: session_snapshot("session-1", 3),
                },
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(
            *lock(&snapshots),
            vec![2u64],
            "snapshots must stop after detach"
        );
        assert_eq!(events.load(AtomicOrdering::SeqCst), 1);

        // session_removed is delivered even to a detached lease.
        server
            .send(ServerMessage::Event {
                event: ServerEvent::SessionRemoved {
                    session_id: "session-1".to_string(),
                },
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(events.load(AtomicOrdering::SeqCst), 2);
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn session_removed_invalidates_leases() {
        let (client, server) = connect_scripted().await;
        let handle = attach(&client, &server, "session-1").await;
        assert!(handle.attached());
        server
            .send(ServerMessage::Event {
                event: ServerEvent::SessionRemoved {
                    session_id: "session-1".to_string(),
                },
            })
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!handle.attached());
        handle
            .dispose()
            .await
            .expect("invalidated dispose needs no protocol cleanup");
    }

    // -----------------------------------------------------------------------
    // Lifecycle: disposal, invalidation, detach retry
    // -----------------------------------------------------------------------

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn dispose_rejects_pending_and_invalidates_handles() {
        let (client, server) = connect_scripted().await;
        let handle = attach(&client, &server, "session-1").await;
        let pending = client.list_sessions();

        client.dispose();
        client.dispose();

        let error = pending.await.expect_err("expected error");
        assert!(matches!(error, PiClientError::Disposed(_)));
        let error = handle
            .prompt("after disposal".to_string())
            .await
            .expect_err("expected error");
        assert!(matches!(error, PiClientError::Disposed(_)));
        handle.dispose().await.expect("dispose after invalidation");
        assert!(!client.connected());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn invalidated_leases_dispose_without_protocol_cleanup() {
        let (client, server) = connect_scripted().await;
        let handle = attach(&client, &server, "session-1").await;
        client.disconnect("Client disconnected");
        handle.dispose().await.expect("dispose after disconnect");
        assert!(!handle.active());
        let _ = server;
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[expect(clippy::panic, reason = "test assertion: unexpected protocol state")]
    #[tokio::test]
    async fn detach_failure_restores_active_lease_and_retry_succeeds() {
        let (client, server) = connect_scripted().await;
        let acquiring = client.acquire_session("session-1", SessionLeaseMode::Exclusive);
        let scripted = async {
            let (attach_id, attach_command) = server.core.next_request().await;
            assert!(matches!(attach_command, Command::Attach { .. }));
            server
                .respond(&attach_id, attach_result("session-1", 1))
                .await;
        };
        let (acquired, ()) = tokio::join!(acquiring, scripted);
        let handle = acquired.expect("exclusive acquire");
        assert!(handle.attached());

        let detaching = handle.detach();
        let scripted = async {
            let (detach_id, detach_command) = server.core.next_request().await;
            assert!(matches!(detach_command, Command::Detach { .. }));
            // While releasing, session-scoped commands are detached-typed.
            let mid = handle.abort().await.expect_err("expected error");
            assert!(matches!(mid, PiClientError::SessionDetached(_)));
            server
                .respond_error(&detach_id, ProtocolErrorCode::InvalidRequest, "retry")
                .await;
        };
        let (first_detach, ()) = tokio::join!(detaching, scripted);
        let error = first_detach.expect_err("expected error");
        match &error {
            PiClientError::Server(e) => assert_eq!(e.message, "retry"),
            other => panic!("expected server error, got {other:?}"),
        }
        assert!(handle.active());

        let detaching = handle.detach();
        let scripted = async {
            let (retry_id, retry_command) = server.core.next_request().await;
            assert!(matches!(retry_command, Command::Detach { .. }));
            server.respond(&retry_id, detach_result("session-1")).await;
        };
        let (second_detach, ()) = tokio::join!(detaching, scripted);
        second_detach.expect("retry detach");
        assert!(!handle.active());
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn reacquisition_serializes_behind_final_detachment() {
        let (client, server) = connect_scripted().await;
        let first = attach(&client, &server, "session-1").await;

        let reacquiring = client.attach_session("session-1");
        let detaching = first.detach();
        let scripted = async {
            let (detach_id, detach_command) = server.core.next_request().await;
            assert!(matches!(detach_command, Command::Detach { .. }));
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(
                server.core.request_count(),
                0,
                "reacquire must wait for the in-flight detachment"
            );
            server.respond(&detach_id, detach_result("session-1")).await;
        };
        let (detached, ()) = tokio::join!(detaching, scripted);
        detached.expect("detach");

        let scripted = async {
            let (second_attach_id, second_attach_command) = server.core.next_request().await;
            assert!(matches!(second_attach_command, Command::Attach { .. }));
            server
                .respond(&second_attach_id, attach_result("session-1", 2))
                .await;
        };
        let (second, ()) = tokio::join!(reacquiring, scripted);
        let second = second.expect("reacquire");
        assert!(second.attached());
        assert_eq!(second.snapshot().map(|s| s.revision), Some(2));
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn create_session_takes_exclusive_lease() {
        let (client, server) = connect_scripted().await;
        let creating = client.create_session(CreateSessionOptions::default());
        let scripted = async {
            let (id, command) = server.core.next_request().await;
            assert!(matches!(command, Command::Create { .. }));
            server
                .respond(
                    &id,
                    CommandResult::Create {
                        session: session_snapshot("fresh", 1),
                    },
                )
                .await;
        };
        let (creating, ()) = tokio::join!(creating, scripted);
        let handle = creating.expect("create");
        assert_eq!(handle.id(), "fresh");
        let conflict = client
            .attach_session("fresh")
            .await
            .expect_err("expected error");
        assert!(matches!(conflict, PiClientError::Ownership(_)));
        detach_leases(&[handle], &server, &[("fresh", "created")]).await;
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn lower_revision_is_accepted_after_reacquire() {
        let (client, server) = connect_scripted().await;
        let first = attach(&client, &server, "session-1").await;
        assert_eq!(first.snapshot().map(|s| s.revision), Some(1));

        let detaching = first.detach();
        let scripted = async {
            let (id, _command) = server.core.next_request().await;
            server.respond(&id, detach_result("session-1")).await;
        };
        let (outcome, ()) = tokio::join!(detaching, scripted);
        outcome.expect("detach");

        // Upstream pins that a fresh attach accepts a lower revision than
        // the cached snapshot (the snapshot was forgotten on re-attach).
        let reacquiring = client.attach_session("session-1");
        let scripted = async {
            let (attach_id, attach_command) = server.core.next_request().await;
            assert!(matches!(attach_command, Command::Attach { .. }));
            server
                .respond(&attach_id, attach_result("session-1", 0))
                .await;
        };
        let (reacquiring, ()) = tokio::join!(reacquiring, scripted);
        let reopened = reacquiring.expect("reacquire");
        assert_eq!(reopened.snapshot().map(|s| s.revision), Some(0));
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: client operations must succeed or fail as expected"
    )]
    #[tokio::test]
    async fn max_frame_length_is_validated_at_construction() {
        let (_listener, endpoint) = InMemoryListener::new();
        let factory = build_transport(&EndpointSpec::InMemory { endpoint }).expect("factory");
        for bad in [0usize, usize::MAX] {
            let error = PiClient::new(PiClientOptions {
                transport_factory: factory.clone(),
                max_frame_length: Some(bad),
                on_listener_error: None,
            })
            .map(|_| ())
            .expect_err("expected error");
            match error {
                PiClientOptionsError::InvalidMaxFrameLength { value, max } => {
                    assert_eq!(max, MAX_UINT32);
                    if bad == 0 {
                        assert_eq!(value, 0);
                    } else {
                        assert_eq!(value, u64::MAX);
                    }
                }
            }
        }
    }
}
