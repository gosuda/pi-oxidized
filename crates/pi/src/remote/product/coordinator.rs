//! Native coordinator transport for the experimental product host.
//!
//! The coordinator is deliberately a small JSON-lines router.  It does not
//! interpret session or service payloads: those remain canonical
//! [`pi_agent::service::value::JsonValue`] trees all the way to the socket
//! boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use pi_agent::service::value::{JsObject, JsString, JsonValue, parse_json, stringify_json};

use super::process::{
    InternalProcessRole, InternalProcessSpawnOptions, MAX_CONTROL_LINE_BYTES,
    encode_control_line, spawn_internal_process,
};

/// Version of the coordinator control protocol.
pub const COORDINATOR_PROTOCOL_VERSION: u32 = 3;

const COORDINATOR_START_TIMEOUT: Duration = Duration::from_secs(10);
const COORDINATOR_RETRY: Duration = Duration::from_millis(10);
const EMPTY_STARTUP_GRACE: Duration = Duration::from_secs(30);
const EMPTY_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Messages delivered by the coordinator to a registered server.
#[derive(Clone, Debug, PartialEq)]
pub enum CoordinatorMessage {
    /// Registration acknowledgement for the current server generation.
    ServerRegistered {
        /// Generation identifier echoed from the registration request.
        server_connection_id: String,
        /// Peer ids already known to the coordinator.
        peers: Vec<String>,
    },
    /// A newer server generation replaced this one.
    ServerReplaced,
    /// A peer connected to the coordinator.
    PeerConnected {
        /// Connected peer id.
        peer_id: String,
    },
    /// A peer disconnected from the coordinator.
    PeerDisconnected {
        /// Disconnected peer id.
        peer_id: String,
    },
    /// Opaque payload routed from another endpoint.
    Message {
        /// Sender endpoint (`server` or a peer id).
        from: String,
        /// Canonical arbitrary JSON payload.
        payload: JsonValue,
    },
}

/// Serializes a canonical value as raw JSON inside a serde serializer.
///
/// `JsonValue` intentionally owns UTF-16 code units and therefore cannot
/// implement serde's string serializer without losing lone surrogates.  The
/// local wrapper keeps that boundary explicit and lets the process foundation
/// enforce the control-line byte limit.
struct JsonValueWire<'a>(&'a JsonValue);

impl Serialize for JsonValueWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(stringify_json(self.0))
            .map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

impl Serialize for CoordinatorMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::ServerRegistered { server_connection_id, peers } => {
                map.serialize_entry("type", "server_registered")?;
                map.serialize_entry("serverConnectionId", server_connection_id)?;
                map.serialize_entry("peers", peers)?;
            }
            Self::ServerReplaced => map.serialize_entry("type", "server_replaced")?,
            Self::PeerConnected { peer_id } => {
                map.serialize_entry("type", "peer_connected")?;
                map.serialize_entry("peerId", peer_id)?;
            }
            Self::PeerDisconnected { peer_id } => {
                map.serialize_entry("type", "peer_disconnected")?;
                map.serialize_entry("peerId", peer_id)?;
            }
            Self::Message { from, payload } => {
                map.serialize_entry("type", "message")?;
                map.serialize_entry("from", from)?;
                map.serialize_entry("payload", &JsonValueWire(payload))?;
            }
        }
        map.end()
    }
}
impl<'de> Deserialize<'de> for CoordinatorMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let value = parse_json(raw.get()).map_err(serde::de::Error::custom)?;
        parse_coordinator_message(&value).map_err(serde::de::Error::custom)
    }
}


/// Events emitted by a connected server-side [`CoordinatorConnection`].
#[derive(Clone, Debug, PartialEq)]
pub enum CoordinatorConnectionEvent {
    /// A peer is now known to the current server.
    PeerConnected { peer_id: String },
    /// A peer is no longer connected.
    PeerDisconnected { peer_id: String },
    /// An opaque message arrived from a peer.
    Message { from: String, payload: JsonValue },
}

/// Options for a server-side coordinator connection.
#[derive(Clone, Debug)]
pub struct CoordinatorConnectionOptions {
    /// Coordinator control socket.
    pub control_path: PathBuf,
    /// Generation endpoint advertised to the coordinator's public proxy.
    pub endpoint: PathBuf,
    /// Stable generation id, or a fresh UUID when omitted.
    pub server_connection_id: Option<String>,
}

/// Removes a listener previously registered with [`CoordinatorConnection::on_event`].
pub type CoordinatorUnsubscribe = Box<dyn FnOnce() + Send + 'static>;

/// Short alias retained for consumers that model listener removal as an unsubscribe.
pub type Unsubscribe = CoordinatorUnsubscribe;

/// Coordinator lifecycle and transport failure.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// Native socket or filesystem failure.
    #[error("coordinator I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The peer sent a malformed or unsupported control message.
    #[error("coordinator protocol error: {0}")]
    Protocol(String),
    /// A control line could not be encoded.
    #[error("coordinator control encoding failed: {0}")]
    Encoding(String),
    /// The coordinator process exited while it was starting.
    #[error("coordinator exited during startup")]
    ProcessExited,
    /// The coordinator did not become reachable before the startup deadline.
    #[error("timed out waiting for coordinator startup")]
    StartupTimeout,
    /// The coordinator transport is not available on this platform.
    #[error("coordinator unix sockets are unsupported on this platform")]
    Unsupported,
    /// The connection has already been closed.
    #[error("coordinator connection is closed")]
    Closed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn json_key(key: &str) -> JsString {
    JsString::from_utf8(key)
}


fn json_string(value: impl AsRef<str>) -> JsonValue {
    JsonValue::String(JsString::from_utf8(value.as_ref()))
}

fn json_object(entries: impl IntoIterator<Item = (&'static str, JsonValue)>) -> JsonValue {
    let mut object = JsObject::new();
    for (key, value) in entries {
        object.insert(json_key(key), value);
    }
    JsonValue::Object(object)
}

fn object_field<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    value.as_object()?.get(&json_key(key))
}

fn string_field(value: &JsonValue, key: &str) -> Option<String> {
    object_field(value, key)?.as_str()?.try_to_utf8().ok()
}

fn payload_field(value: &JsonValue) -> Option<JsonValue> {
    object_field(value, "payload").cloned()
}

fn protocol_is_supported(value: &JsonValue) -> bool {
    object_field(value, "protocol")
        .and_then(JsonValue::as_f64)
        .is_some_and(|protocol| protocol == f64::from(COORDINATOR_PROTOCOL_VERSION))
}

fn encode_json_line(value: &JsonValue) -> Result<Vec<u8>, CoordinatorError> {
    encode_control_line(&JsonValueWire(value)).map_err(|error| CoordinatorError::Encoding(error.to_string()))
}

fn parse_coordinator_message(value: &JsonValue) -> Result<CoordinatorMessage, String> {
    let message_type = string_field(value, "type").ok_or_else(|| "Coordinator message must have a type".to_owned())?;
    match message_type.as_str() {
        "server_registered" => {
            let server_connection_id = string_field(value, "serverConnectionId")
                .ok_or_else(|| "Coordinator server registration is missing serverConnectionId".to_owned())?;
            let peers = object_field(value, "peers")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| "Coordinator server registration is missing peers".to_owned())?
                .iter()
                .map(|peer| {
                    peer.as_str()
                        .and_then(|value| value.try_to_utf8().ok())
                        .ok_or_else(|| "Coordinator server registration has an invalid peer id".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CoordinatorMessage::ServerRegistered { server_connection_id, peers })
        }
        "server_replaced" => Ok(CoordinatorMessage::ServerReplaced),
        "peer_connected" => Ok(CoordinatorMessage::PeerConnected {
            peer_id: string_field(value, "peerId")
                .ok_or_else(|| "Coordinator peer_connected is missing peerId".to_owned())?,
        }),
        "peer_disconnected" => Ok(CoordinatorMessage::PeerDisconnected {
            peer_id: string_field(value, "peerId")
                .ok_or_else(|| "Coordinator peer_disconnected is missing peerId".to_owned())?,
        }),
        "message" => Ok(CoordinatorMessage::Message {
            from: string_field(value, "from")
                .ok_or_else(|| "Coordinator message is missing from".to_owned())?,
            payload: payload_field(value)
                .ok_or_else(|| "Coordinator message is missing payload".to_owned())?,
        }),
        other => Err(format!("Coordinator sent an unsupported message: {other}")),
    }
}

struct WriteRequest {
    bytes: Vec<u8>,
    result: oneshot::Sender<Result<(), String>>,
}

struct ConnectionState {
    sender: Option<mpsc::UnboundedSender<WriteRequest>>,
    reader_task: Option<JoinHandle<()>>,
    registration: Option<oneshot::Sender<Result<(), String>>>,
    listeners: BTreeMap<u64, Arc<dyn Fn(CoordinatorConnectionEvent) + Send + Sync>>,
    next_listener_id: u64,
    peer_ids: BTreeSet<String>,
    registered: bool,
    connected: bool,
    closed: bool,
    replaced: bool,
}

/// The server-side endpoint of the coordinator's intentionally opaque router.
pub struct CoordinatorConnection {
    inner: Arc<CoordinatorConnectionInner>,
    /// Identifier for this server generation.
    pub server_connection_id: String,
}

struct CoordinatorConnectionInner {
    control_path: PathBuf,
    endpoint: PathBuf,
    server_connection_id: String,
    state: Mutex<ConnectionState>,
    replaced: Arc<Notify>,
    cancel: CancellationToken,
}

impl CoordinatorConnection {
    /// Creates a disconnected connection object.
    #[must_use]
    pub fn new(options: CoordinatorConnectionOptions) -> Self {
        let server_connection_id = options
            .server_connection_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        Self {
            inner: Arc::new(CoordinatorConnectionInner {
                control_path: options.control_path,
                endpoint: options.endpoint,
                server_connection_id: server_connection_id.clone(),
                state: Mutex::new(ConnectionState {
                    sender: None,
                    reader_task: None,
                    registration: None,
                    listeners: BTreeMap::new(),
                    next_listener_id: 0,
                    peer_ids: BTreeSet::new(),
                    registered: false,
                    connected: false,
                    closed: false,
                    replaced: false,
                }),
                replaced: Arc::new(Notify::new()),
                cancel: CancellationToken::new(),
            }),
            server_connection_id,
        }
    }

    /// Returns the coordinator control path.
    #[must_use]
    pub fn control_path(&self) -> &Path {
        &self.inner.control_path
    }

    /// Returns whether this generation was replaced or disconnected.
    #[must_use]
    pub fn was_replaced(&self) -> bool {
        lock(&self.inner.state).replaced
    }

    /// Returns a future that resolves when this generation is replaced or disconnected.
    #[must_use]
    pub fn replaced(&self) -> BoxFuture<'static, ()> {
        let notified = Arc::clone(&self.inner.replaced).notified_owned();
        let already_replaced = self.was_replaced();
        Box::pin(async move {
            if !already_replaced {
                notified.await;
            }
        })
    }

    /// Returns the peer ids known from the latest registration and notifications.
    #[must_use]
    pub fn peer_ids(&self) -> BTreeSet<String> {
        lock(&self.inner.state).peer_ids.clone()
    }

    /// Registers an event callback and returns its removal function.
    pub fn on_event<F>(&self, listener: F) -> CoordinatorUnsubscribe
    where
        F: Fn(CoordinatorConnectionEvent) + Send + Sync + 'static,
    {
        let listener: Arc<dyn Fn(CoordinatorConnectionEvent) + Send + Sync> = Arc::new(listener);
        let (listener_id, inner) = {
            let mut state = lock(&self.inner.state);
            let listener_id = state.next_listener_id;
            state.next_listener_id = state.next_listener_id.wrapping_add(1);
            state.listeners.insert(listener_id, listener);
            (listener_id, Arc::clone(&self.inner))
        };
        Box::new(move || {
            lock(&inner.state).listeners.remove(&listener_id);
        })
    }

    /// Connects and waits for the server registration acknowledgement.
    pub async fn connect(&self) -> Result<(), CoordinatorError> {
        #[cfg(unix)]
        {
            self.connect_unix().await
        }
        #[cfg(not(unix))]
        {
            Err(CoordinatorError::Unsupported)
        }
    }

    /// Sends one opaque payload to a peer.
    pub async fn send(&self, peer_id: impl AsRef<str>, payload: JsonValue) -> Result<(), CoordinatorError> {
        let value = json_object([
            ("type", json_string("send")),
            ("to", json_string(peer_id.as_ref())),
            ("payload", payload),
        ]);
        self.write_value(value, true).await
    }

    /// Broadcasts one opaque payload to every peer through the current server.
    pub async fn broadcast(&self, payload: JsonValue) -> Result<(), CoordinatorError> {
        let value = json_object([
            ("type", json_string("broadcast")),
            ("payload", payload),
        ]);
        self.write_value(value, true).await
    }

    /// Closes the connection and removes all listeners' peer state.
    pub fn close(&self) {
        let (registration, reader_task) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state.connected = false;
            state.registered = false;
            state.sender = None;
            state.peer_ids.clear();
            (state.registration.take(), state.reader_task.take())
        };
        self.inner.cancel.cancel();
        if let Some(registration) = registration {
            let _ = registration.send(Err("Coordinator server closed".to_owned()));
        }
        if let Some(reader_task) = reader_task {
            reader_task.abort();
        }
    }

    async fn write_value(&self, value: JsonValue, require_registration: bool) -> Result<(), CoordinatorError> {
        let bytes = encode_json_line(&value)?;
        let sender = {
            let state = lock(&self.inner.state);
            if state.closed {
                return Err(CoordinatorError::Closed);
            }
            if require_registration && !state.registered {
                return Err(CoordinatorError::Protocol("Coordinator server is not connected".to_owned()));
            }
            state
                .sender
                .clone()
                .ok_or_else(|| CoordinatorError::Protocol("Coordinator server is not connected".to_owned()))?
        };
        let (result, receiver) = oneshot::channel();
        sender
            .send(WriteRequest { bytes, result })
            .map_err(|_| CoordinatorError::Closed)?;
        receiver
            .await
            .map_err(|_| CoordinatorError::Closed)?
            .map_err(CoordinatorError::Protocol)
    }

    fn emit(&self, event: CoordinatorConnectionEvent) {
        let listeners = lock(&self.inner.state).listeners.values().cloned().collect::<Vec<_>>();
        for listener in listeners {
            listener(event.clone());
        }
    }

    fn mark_replaced(&self) {
        let should_notify = {
            let mut state = lock(&self.inner.state);
            if state.replaced {
                false
            } else {
                state.replaced = true;
                true
            }
        };
        if should_notify {
            self.inner.replaced.notify_waiters();
        }
    }

    fn disconnected(&self, error: impl Into<String>) {
        let registration = {
            let mut state = lock(&self.inner.state);
            state.sender = None;
            state.connected = false;
            state.registration.take()
        };
        if let Some(registration) = registration {
            let _ = registration.send(Err(error.into()));
        }
        self.inner.cancel.cancel();
        if !lock(&self.inner.state).closed {
            self.mark_replaced();
        }
    }

    fn handle_message(&self, value: JsonValue) {
        let message = match parse_coordinator_message(&value) {
            Ok(message) => message,
            Err(error) => {
                self.disconnected(error);
                return;
            }
        };
        match message {
            CoordinatorMessage::ServerRegistered { server_connection_id, peers } => {
                if server_connection_id != self.server_connection_id {
                    self.disconnected("Coordinator returned an invalid server registration");
                    return;
                }
                let registration = {
                    let mut state = lock(&self.inner.state);
                    state.peer_ids.extend(peers);
                    state.registered = true;
                    state.registration.take()
                };
                if let Some(registration) = registration {
                    let _ = registration.send(Ok(()));
                }
            }
            CoordinatorMessage::ServerReplaced => self.mark_replaced(),
            CoordinatorMessage::PeerConnected { peer_id } => {
                lock(&self.inner.state).peer_ids.insert(peer_id.clone());
                self.emit(CoordinatorConnectionEvent::PeerConnected { peer_id });
            }
            CoordinatorMessage::PeerDisconnected { peer_id } => {
                lock(&self.inner.state).peer_ids.remove(&peer_id);
                self.emit(CoordinatorConnectionEvent::PeerDisconnected { peer_id });
            }
            CoordinatorMessage::Message { from, payload } => {
                self.emit(CoordinatorConnectionEvent::Message { from, payload });
            }
        }
    }

    #[cfg(unix)]
    async fn connect_unix(&self) -> Result<(), CoordinatorError> {
        use tokio::net::UnixStream;

        {
            let state = lock(&self.inner.state);
            if state.closed {
                return Err(CoordinatorError::Closed);
            }
            if state.connected || state.sender.is_some() {
                return Err(CoordinatorError::Protocol("Coordinator server is already connected".to_owned()));
            }
        }
        let endpoint = self
            .inner
            .endpoint
            .to_str()
            .ok_or_else(|| CoordinatorError::Protocol("Coordinator endpoint is not valid UTF-8".to_owned()))?
            .to_owned();
        let socket = UnixStream::connect(&self.inner.control_path).await?;
        let (reader, writer) = socket.into_split();
        let (sender, receiver) = mpsc::unbounded_channel();
        let (registration, registered) = oneshot::channel();
        {
            let mut state = lock(&self.inner.state);
            state.sender = Some(sender.clone());
            state.registration = Some(registration);
            state.connected = true;
        }
        let writer_inner = Arc::clone(&self.inner);
        let writer_cancel = self.inner.cancel.clone();
        tokio::spawn(async move {
            writer_loop(writer, receiver, writer_inner, writer_cancel).await;
        });
        let reader_inner = Arc::clone(&self.inner);
        let reader_connection = Self { inner: Arc::clone(&self.inner), server_connection_id: self.server_connection_id.clone() };
        let reader_cancel = self.inner.cancel.clone();
        let reader_task = tokio::spawn(async move {
            reader_loop(reader, reader_connection, reader_inner, reader_cancel).await;
        });
        lock(&self.inner.state).reader_task = Some(reader_task);

        let register = json_object([
            ("type", json_string("register_server")),
            ("protocol", JsonValue::Number(f64::from(COORDINATOR_PROTOCOL_VERSION))),
            ("serverConnectionId", json_string(&self.server_connection_id)),
            ("endpoint", json_string(endpoint)),
        ]);
        if let Err(error) = self.write_value(register, false).await {
            self.disconnected(error.to_string());
            return Err(error);
        }
        match registered.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(CoordinatorError::Protocol(error)),
            Err(_) => Err(CoordinatorError::Closed),
        }
    }
}

#[cfg(unix)]
async fn writer_loop(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut receiver: mpsc::UnboundedReceiver<WriteRequest>,
    connection: Arc<CoordinatorConnectionInner>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            request = receiver.recv() => {
                let Some(request) = request else { break };
                match writer.write_all(&request.bytes).await {
                    Ok(()) => { let _ = request.result.send(Ok(())); }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = request.result.send(Err(message.clone()));
                        let connection_view = CoordinatorConnection {
                            inner: Arc::clone(&connection),
                            server_connection_id: connection.server_connection_id.clone(),
                        };
                        connection_view.disconnected(message);
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
async fn read_control_line<R>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<Option<bool>>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if line.is_empty() { None } else { Some(false) });
        }
        if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
            let required = line.len().saturating_add(newline.saturating_add(1));
            if required > MAX_CONTROL_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Coordinator message is too large",
                ));
            }
            line.extend_from_slice(&chunk[..=newline]);
            reader.consume(newline + 1);
            return Ok(Some(true));
        }
        let required = line.len().saturating_add(chunk.len());
        if required > MAX_CONTROL_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Coordinator message is too large",
            ));
        }
        let consumed = chunk.len();
        line.extend_from_slice(chunk);
        reader.consume(consumed);
    }
}

#[cfg(unix)]
async fn reader_loop(
    reader: tokio::net::unix::OwnedReadHalf,
    connection: CoordinatorConnection,
    _inner: Arc<CoordinatorConnectionInner>,
    cancel: CancellationToken,
) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let result = tokio::select! {
            _ = cancel.cancelled() => break,
            result = read_control_line(&mut reader, &mut line) => result,
        };
        let complete = match result {
            Ok(Some(complete)) => complete,
            Ok(None) => {
                connection.disconnected("Coordinator connection closed");
                break;
            }
            Err(error) => {
                connection.disconnected(error.to_string());
                break;
            }
        };
        if !complete {
            connection.disconnected("Coordinator connection closed");
            break;
        }
        let mut end = line.len().saturating_sub(1);
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
        let text = match std::str::from_utf8(&line[..end]) {
            Ok(text) => text,
            Err(error) => {
                connection.disconnected(format!("Coordinator sent invalid UTF-8: {error}"));
                break;
            }
        };
        match parse_json(text) {
            Ok(value) => connection.handle_message(value),
            Err(error) => {
                connection.disconnected(format!("Coordinator sent invalid JSON: {error}"));
                break;
            }
        }
    }
}


/// A lease keeping a startup probe socket open until the caller is ready.
pub struct CoordinatorStartupLease {
    #[cfg(unix)]
    socket: Option<tokio::net::UnixStream>,
}

impl CoordinatorStartupLease {
    /// Closes the probe connection.
    pub fn close(self) {}
}

/// Ensures that a coordinator is reachable, starting one when necessary.
pub async fn ensure_coordinator(
    public_path: impl AsRef<Path>,
    control_path: impl AsRef<Path>,
) -> Result<CoordinatorStartupLease, CoordinatorError> {
    #[cfg(unix)]
    {
        let public_path = public_path.as_ref().to_path_buf();
        let control_path = control_path.as_ref().to_path_buf();
        if let Some(socket) = try_connect(&control_path).await? {
            return Ok(CoordinatorStartupLease { socket: Some(socket) });
        }
        let args = vec![
            public_path.to_string_lossy().into_owned(),
            control_path.to_string_lossy().into_owned(),
        ];
        let mut child = spawn_internal_process(
            InternalProcessRole::Coordinator,
            &args,
            &InternalProcessSpawnOptions { env: Vec::new() },
        )?;
        let deadline = tokio::time::Instant::now() + COORDINATOR_START_TIMEOUT;
        loop {
            if let Some(socket) = try_connect(&control_path).await? {
                return Ok(CoordinatorStartupLease { socket: Some(socket) });
            }
            if child.try_wait()?.is_some() {
                return Err(CoordinatorError::ProcessExited);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CoordinatorError::StartupTimeout);
            }
            tokio::time::sleep(COORDINATOR_RETRY).await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (public_path, control_path);
        Err(CoordinatorError::Unsupported)
    }
}

#[cfg(unix)]
async fn try_connect(path: &Path) -> Result<Option<tokio::net::UnixStream>, CoordinatorError> {
    match tokio::net::UnixStream::connect(path).await {
        Ok(socket) => Ok(Some(socket)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => Ok(None),
        Err(error) => Err(CoordinatorError::Io(error)),
    }
}

#[cfg(unix)]
static COORDINATOR_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Runs the standalone coordinator process until it becomes empty or receives
/// SIGINT/SIGTERM.
pub async fn run_coordinator_process(args: &[String]) -> Result<(), CoordinatorError> {
    #[cfg(unix)]
    {
        run_coordinator_process_unix(args).await
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err(CoordinatorError::Unsupported)
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct ControlState {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
    role: ControlRole,
}

#[cfg(unix)]
#[derive(Clone)]
enum ControlRole {
    Unknown,
    Server,
    Peer(String),
}

#[cfg(unix)]
#[derive(Clone)]
struct ServerState {
    connection_id: u64,
    server_connection_id: String,
    endpoint: PathBuf,
    writer: mpsc::UnboundedSender<Vec<u8>>,
}

#[cfg(unix)]
#[derive(Clone)]
struct PeerState {
    connection_id: u64,
    writer: mpsc::UnboundedSender<Vec<u8>>,
}

#[cfg(unix)]
enum ProcessEvent {
    ControlAccepted {
        connection_id: u64,
        writer: mpsc::UnboundedSender<Vec<u8>>,
        cancel: CancellationToken,
    },
    ControlLine { connection_id: u64, value: JsonValue },
    ControlClosed { connection_id: u64 },
    PublicClosed { connection_id: u64 },
    EmptyTimer,
}

#[cfg(unix)]
struct CoordinatorProcess {
    event_tx: mpsc::UnboundedSender<ProcessEvent>,
    control_connections: BTreeMap<u64, ControlState>,
    peers: BTreeMap<String, PeerState>,
    current_server: Option<ServerState>,
    public_connections: BTreeMap<u64, CancellationToken>,
    empty_timer: Option<CancellationToken>,
    shutting_down: bool,
}

#[cfg(unix)]
impl CoordinatorProcess {
    fn new(event_tx: mpsc::UnboundedSender<ProcessEvent>) -> Self {
        Self {
            event_tx,
            control_connections: BTreeMap::new(),
            peers: BTreeMap::new(),
            current_server: None,
            public_connections: BTreeMap::new(),
            empty_timer: None,
            shutting_down: false,
        }
    }

    fn schedule_empty_shutdown(&mut self, delay: Duration) {
        if self.empty_timer.is_some() || self.shutting_down {
            return;
        }
        let cancel = CancellationToken::new();
        let timer_cancel = cancel.clone();
        let event_tx = self.event_tx.clone();
        self.empty_timer = Some(cancel);
        tokio::spawn(async move {
            tokio::select! {
                _ = timer_cancel.cancelled() => {}
                _ = tokio::time::sleep(delay) => { let _ = event_tx.send(ProcessEvent::EmptyTimer); }
            }
        });
    }

    fn cancel_empty_shutdown(&mut self) {
        if let Some(cancel) = self.empty_timer.take() {
            cancel.cancel();
        }
    }

    fn check_empty(&mut self) {
        if self.shutting_down
            || self.current_server.is_some()
            || !self.peers.is_empty()
            || !self.public_connections.is_empty()
            || !self.control_connections.is_empty()
        {
            self.cancel_empty_shutdown();
        } else {
            self.schedule_empty_shutdown(EMPTY_SHUTDOWN_GRACE);
        }
    }

    fn accept_control(
        &mut self,
        connection_id: u64,
        writer: mpsc::UnboundedSender<Vec<u8>>,
        cancel: CancellationToken,
    ) {
        if self.shutting_down {
            cancel.cancel();
            return;
        }
        self.control_connections.insert(
            connection_id,
            ControlState { writer, cancel, role: ControlRole::Unknown },
        );
    }

    fn accept_public(&mut self, stream: tokio::net::UnixStream, connection_id: u64) {
        if self.shutting_down {
            drop(stream);
            return;
        }
        let endpoint = match self.current_server.as_ref() {
            Some(server) => server.endpoint.clone(),
            None => {
                drop(stream);
                return;
            }
        };
        self.cancel_empty_shutdown();
        let cancel = CancellationToken::new();
        self.public_connections.insert(connection_id, cancel.clone());
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            proxy_public(stream, endpoint, connection_id, cancel, event_tx).await;
        });
    }

    fn handle_line(&mut self, connection_id: u64, value: JsonValue) {
        let Some(connection) = self.control_connections.get(&connection_id).cloned() else {
            return;
        };
        match connection.role {
            ControlRole::Unknown => {
                let Some(message_type) = string_field(&value, "type") else {
                    self.disconnect(connection_id);
                    return;
                };
                let result = match message_type.as_str() {
                    "register_server" => self.register_server(connection_id, value),
                    "register_peer" => self.register_peer(connection_id, value),
                    _ => Err("Coordinator connection did not register a role".to_owned()),
                };
                if result.is_err() {
                    self.disconnect(connection_id);
                }
            }
            ControlRole::Server => {
                if self.current_server.as_ref().is_some_and(|server| server.connection_id == connection_id) {
                    self.handle_routed_message("server", &value, connection_id);
                }
            }
            ControlRole::Peer(peer_id) => {
                self.handle_routed_message(&peer_id, &value, connection_id);
            }
        }
    }

    fn register_server(&mut self, connection_id: u64, value: JsonValue) -> Result<(), String> {
        if !protocol_is_supported(&value) {
            return Err("Unsupported coordinator protocol".to_owned());
        }
        let server_connection_id = string_field(&value, "serverConnectionId")
            .ok_or_else(|| "Coordinator serverConnectionId must be a string".to_owned())?;
        if server_connection_id.is_empty() {
            return Err("Coordinator serverConnectionId must be a string".to_owned());
        }
        let endpoint = string_field(&value, "endpoint")
            .ok_or_else(|| "Coordinator endpoint must be a string".to_owned())?;
        if endpoint.is_empty() {
            return Err("Coordinator endpoint must be a string".to_owned());
        }
        let writer = self
            .control_connections
            .get(&connection_id)
            .ok_or_else(|| "Coordinator connection is gone".to_owned())?
            .writer
            .clone();
        let previous = self.current_server.take();
        self.current_server = Some(ServerState {
            connection_id,
            server_connection_id: server_connection_id.clone(),
            endpoint: PathBuf::from(endpoint),
            writer: writer.clone(),
        });
        if let Some(connection) = self.control_connections.get_mut(&connection_id) {
            connection.role = ControlRole::Server;
        }
        self.cancel_empty_shutdown();
        let peers = JsonValue::Array(
            self.peers
                .keys()
                .map(|peer_id| json_string(peer_id))
                .collect(),
        );
        self.send_value(
            &writer,
            json_object([
                ("type", json_string("server_registered")),
                ("serverConnectionId", json_string(&server_connection_id)),
                ("peers", peers),
            ]),
        );
        if let Some(previous) = previous {
            self.close_public_connections();
            self.notify_peers(json_object([
                ("type", json_string("server_disconnected")),
                ("serverConnectionId", json_string(previous.server_connection_id)),
            ]));
            self.send_value(&previous.writer, json_object([("type", json_string("server_replaced"))]));
            self.disconnect(previous.connection_id);
        }
        self.notify_peers(json_object([
            ("type", json_string("server_connected")),
            ("serverConnectionId", json_string(&server_connection_id)),
        ]));
        Ok(())
    }

    fn register_peer(&mut self, connection_id: u64, value: JsonValue) -> Result<(), String> {
        if !protocol_is_supported(&value) {
            return Err("Unsupported coordinator protocol".to_owned());
        }
        let peer_id = string_field(&value, "peerId")
            .ok_or_else(|| "Coordinator peerId must be a string".to_owned())?;
        if peer_id.is_empty() {
            return Err("Coordinator peerId must be a string".to_owned());
        }
        if peer_id == "server" || self.peers.contains_key(&peer_id) {
            return Err(format!("Coordinator peer is already connected: {peer_id}"));
        }
        let writer = self
            .control_connections
            .get(&connection_id)
            .ok_or_else(|| "Coordinator connection is gone".to_owned())?
            .writer
            .clone();
        self.peers.insert(
            peer_id.clone(),
            PeerState { connection_id, writer: writer.clone() },
        );
        if let Some(connection) = self.control_connections.get_mut(&connection_id) {
            connection.role = ControlRole::Peer(peer_id.clone());
        }
        self.cancel_empty_shutdown();
        let mut registration = vec![("type", json_string("peer_registered")), ("peerId", json_string(&peer_id))];
        if let Some(server) = &self.current_server {
            registration.push(("serverConnectionId", json_string(&server.server_connection_id)));
        }
        self.send_value(&writer, json_object(registration));
        if let Some(server) = &self.current_server {
            self.send_value(
                &server.writer,
                json_object([
                    ("type", json_string("peer_connected")),
                    ("peerId", json_string(&peer_id)),
                ]),
            );
        }
        Ok(())
    }

    fn handle_routed_message(&mut self, from: &str, value: &JsonValue, connection_id: u64) {
        let Some(message_type) = string_field(value, "type") else {
            self.disconnect(connection_id);
            return;
        };
        match message_type.as_str() {
            "send" => {
                let Some(target) = string_field(value, "to") else {
                    self.disconnect(connection_id);
                    return;
                };
                if target.is_empty() {
                    self.disconnect(connection_id);
                    return;
                }
                let writer = if target == "server" {
                    self.current_server.as_ref().map(|server| server.writer.clone())
                } else {
                    self.peers.get(&target).map(|peer| peer.writer.clone())
                };
                if let Some(writer) = writer {
                    let payload = payload_field(value).unwrap_or(JsonValue::Null);
                    self.send_value(
                        &writer,
                        json_object([
                            ("type", json_string("message")),
                            ("from", json_string(from)),
                            ("payload", payload),
                        ]),
                    );
                }
            }
            "broadcast" => {
                if from != "server"
                    || !self
                        .current_server
                        .as_ref()
                        .is_some_and(|server| server.connection_id == connection_id)
                {
                    self.disconnect(connection_id);
                    return;
                }
                let payload = payload_field(value).unwrap_or(JsonValue::Null);
                for peer in self.peers.values() {
                    self.send_value(
                        &peer.writer,
                        json_object([
                            ("type", json_string("message")),
                            ("from", json_string(from)),
                            ("payload", payload.clone()),
                        ]),
                    );
                }
            }
            _ => self.disconnect(connection_id),
        }
    }

    fn send_value(&self, writer: &mpsc::UnboundedSender<Vec<u8>>, value: JsonValue) {
        if let Ok(bytes) = encode_json_line(&value) {
            let _ = writer.send(bytes);
        }
    }

    fn notify_peers(&self, value: JsonValue) {
        for peer in self.peers.values() {
            self.send_value(&peer.writer, value.clone());
        }
    }

    fn close_public_connections(&mut self) {
        for cancel in self.public_connections.values() {
            cancel.cancel();
        }
        self.public_connections.clear();
    }

    fn disconnect(&mut self, connection_id: u64) {
        let Some(connection) = self.control_connections.remove(&connection_id) else {
            return;
        };
        connection.cancel.cancel();
        match connection.role {
            ControlRole::Server => {
                if self
                    .current_server
                    .as_ref()
                    .is_some_and(|server| server.connection_id == connection_id)
                {
                    let server_connection_id = self.current_server.take().map(|server| server.server_connection_id);
                    if let Some(server_connection_id) = server_connection_id {
                        self.notify_peers(json_object([
                            ("type", json_string("server_disconnected")),
                            ("serverConnectionId", json_string(server_connection_id)),
                        ]));
                    }
                }
            }
            ControlRole::Peer(peer_id) => {
                if self
                    .peers
                    .get(&peer_id)
                    .is_some_and(|peer| peer.connection_id == connection_id)
                {
                    self.peers.remove(&peer_id);
                    if let Some(server) = &self.current_server {
                        self.send_value(
                            &server.writer,
                            json_object([
                                ("type", json_string("peer_disconnected")),
                                ("peerId", json_string(peer_id)),
                            ]),
                        );
                    }
                }
            }
            ControlRole::Unknown => {}
        }
        self.check_empty();
    }

    fn handle_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::ControlAccepted { connection_id, writer, cancel } => {
                self.accept_control(connection_id, writer, cancel);
            }
            ProcessEvent::ControlLine { connection_id, value } => self.handle_line(connection_id, value),
            ProcessEvent::ControlClosed { connection_id } => self.disconnect(connection_id),
            ProcessEvent::PublicClosed { connection_id } => {
                self.public_connections.remove(&connection_id);
                self.check_empty();
            }
            ProcessEvent::EmptyTimer => {
                self.empty_timer = None;
                if self.current_server.is_none()
                    && self.peers.is_empty()
                    && self.public_connections.is_empty()
                    && self.control_connections.is_empty()
                {
                    self.shutting_down = true;
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.shutting_down = true;
        self.cancel_empty_shutdown();
        self.close_public_connections();
        for connection in self.control_connections.values() {
            connection.cancel.cancel();
        }
        self.control_connections.clear();
        self.peers.clear();
        self.current_server = None;
    }
}

#[cfg(unix)]
async fn control_connection(
    stream: tokio::net::UnixStream,
    connection_id: u64,
    event_tx: mpsc::UnboundedSender<ProcessEvent>,
) {
    let (reader, writer) = stream.into_split();
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let cancel = CancellationToken::new();
    let writer_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut writer = writer;
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                message = writer_rx.recv() => {
                    let Some(message) = message else { break };
                    if writer.write_all(&message).await.is_err() {
                        writer_cancel.cancel();
                        break;
                    }
                }
            }
        }
    });
    if event_tx
        .send(ProcessEvent::ControlAccepted { connection_id, writer: writer_tx, cancel: cancel.clone() })
        .is_err()
    {
        cancel.cancel();
        return;
    }
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let result = tokio::select! {
            _ = cancel.cancelled() => break,
            result = read_control_line(&mut reader, &mut line) => result,
        };
        let complete = match result {
            Ok(Some(complete)) => complete,
            _ => false,
        };
        if !complete {
            break;
        }
        let mut end = line.len().saturating_sub(1);
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
        let text = match std::str::from_utf8(&line[..end]) {
            Ok(text) => text,
            Err(_) => break,
        };
        let value = match parse_json(text) {
            Ok(value) => value,
            Err(_) => break,
        };
        if event_tx.send(ProcessEvent::ControlLine { connection_id, value }).is_err() {
            break;
        }
    }
    cancel.cancel();
    let _ = event_tx.send(ProcessEvent::ControlClosed { connection_id });
}

#[cfg(unix)]
async fn proxy_public(
    mut client: tokio::net::UnixStream,
    endpoint: PathBuf,
    connection_id: u64,
    cancel: CancellationToken,
    event_tx: mpsc::UnboundedSender<ProcessEvent>,
) {
    let upstream = tokio::select! {
        _ = cancel.cancelled() => None,
        result = tokio::net::UnixStream::connect(endpoint) => result.ok(),
    };
    if let Some(mut upstream) = upstream {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
        }
    }
    cancel.cancel();
    let _ = event_tx.send(ProcessEvent::PublicClosed { connection_id });
}

#[cfg(unix)]
async fn run_coordinator_process_unix(args: &[String]) -> Result<(), CoordinatorError> {
    if COORDINATOR_RUNNING.load(std::sync::atomic::Ordering::Acquire) {
        return Err(CoordinatorError::Protocol("Coordinator process is already running".to_owned()));
    }
    let Some(public_arg) = args.first() else {
        return Err(CoordinatorError::Protocol("Coordinator requires public and control socket paths".to_owned()));
    };
    let Some(control_arg) = args.get(1) else {
        return Err(CoordinatorError::Protocol("Coordinator requires public and control socket paths".to_owned()));
    };
    if public_arg.is_empty() || control_arg.is_empty() {
        return Err(CoordinatorError::Protocol("Coordinator requires public and control socket paths".to_owned()));
    }
    if COORDINATOR_RUNNING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return Err(CoordinatorError::Protocol("Coordinator process is already running".to_owned()));
    }
    let public_path = PathBuf::from(public_arg);
    let control_path = PathBuf::from(control_arg);
    remove_stale_socket(&control_path).await?;
    remove_stale_socket(&public_path).await?;
    let control_listener = match tokio::net::UnixListener::bind(&control_path) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = cleanup_socket(&control_path).await;
            let _ = cleanup_socket(&public_path).await;
            return Err(CoordinatorError::Io(error));
        }
    };
    if let Err(error) = restrict_socket(&control_path).await {
        drop(control_listener);
        let _ = cleanup_socket(&control_path).await;
        let _ = cleanup_socket(&public_path).await;
        return Err(error);
    }
    let public_listener = match tokio::net::UnixListener::bind(&public_path) {
        Ok(listener) => listener,
        Err(error) => {
            drop(control_listener);
            let _ = cleanup_socket(&control_path).await;
            let _ = cleanup_socket(&public_path).await;
            return Err(CoordinatorError::Io(error));
        }
    };
    if let Err(error) = restrict_socket(&public_path).await {
        drop(control_listener);
        drop(public_listener);
        let _ = cleanup_socket(&control_path).await;
        let _ = cleanup_socket(&public_path).await;
        return Err(error);
    }

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut process = CoordinatorProcess::new(event_tx.clone());
    process.schedule_empty_shutdown(EMPTY_STARTUP_GRACE);
    let mut next_connection_id = 1_u64;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(CoordinatorError::Io)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(CoordinatorError::Io)?;
    loop {
        tokio::select! {
            biased;
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                process.handle_event(event);
            }
            accepted = control_listener.accept() => {
                let (stream, _) = accepted.map_err(CoordinatorError::Io)?;
                let connection_id = next_connection_id;
                next_connection_id = next_connection_id.wrapping_add(1);
                let event_tx = event_tx.clone();
                tokio::spawn(async move { control_connection(stream, connection_id, event_tx).await; });
            }
            accepted = public_listener.accept() => {
                let (stream, _) = accepted.map_err(CoordinatorError::Io)?;
                let connection_id = next_connection_id;
                next_connection_id = next_connection_id.wrapping_add(1);
                process.accept_public(stream, connection_id);
            }
        }
        if process.shutting_down {
            break;
        }
    }
    process.shutdown();
    drop(control_listener);
    drop(public_listener);
    cleanup_socket(&public_path).await?;
    cleanup_socket(&control_path).await?;
    Ok(())
}

#[cfg(unix)]
async fn restrict_socket(path: &Path) -> Result<(), CoordinatorError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = tokio::fs::metadata(path).await?.permissions();
    permissions.set_mode(0o600);
    tokio::fs::set_permissions(path, permissions).await?;
    Ok(())
}

#[cfg(unix)]
async fn remove_stale_socket(path: &Path) -> Result<(), CoordinatorError> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CoordinatorError::Io(error)),
    };
    if !metadata.file_type().is_socket() {
        return Err(CoordinatorError::Protocol(format!(
            "Coordinator path is not a socket: {}",
            path.display()
        )));
    }
    match tokio::net::UnixStream::connect(path).await {
        Ok(socket) => {
            drop(socket);
            Err(CoordinatorError::Protocol(format!(
                "Coordinator socket is already active: {}",
                path.display()
            )))
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {
                cleanup_socket(path).await
            }
    }
}

#[cfg(unix)]
async fn cleanup_socket(path: &Path) -> Result<(), CoordinatorError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CoordinatorError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_message_preserves_canonical_payload() {
        let payload = parse_json(r#"{"surrogate":"\ud800","number":-0}"#)
            .expect("canonical JSON fixture");
        let line = encode_json_line(&json_object([
            ("type", json_string("message")),
            ("from", json_string("server")),
            ("payload", payload),
        ]))
        .expect("control line");
        let text = String::from_utf8(line).expect("UTF-8 control line");
        assert!(text.contains(r#"\ud800"#));
        assert!(text.ends_with('\n'));
    }
}
