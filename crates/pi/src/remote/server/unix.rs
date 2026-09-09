//! Unix-domain listener preset (R4, AR2) — the server-side sibling of
//! the Unix client transport, `#[cfg(unix)]`-gated per the platform
//! contract.
//!
//! [`create_server`] composes that listener with the generic [`Server`]
//! (upstream `createUnixServer`).
//!
//! Bind discipline mirrors upstream: the parent directory is created
//! with mode `0o700`, a stale socket at the path is probed for
//! liveness before removal (a live listener is an error, never
//! silently unlinked), the socket is `chmod`ed to the requested mode,
//! and cleanup unlinks only while the file identity (`dev`/`ino`)
//! still matches the bound socket.
//!
//! # Divergences from upstream (recorded)
//!
//! - Upstream binds to a private `.p-<hash>` path and hard-links it to
//!   the published path for atomic startup; this port binds directly
//!   at the requested path and guards cleanup by file identity
//!   instead (no multi-tenant symlink threat model on the Rust tier).
//! - The graceful-close deadline waits bounded on the writer task
//!   finishing; on expiry the connection is already marked closed and
//!   the failure is treated as best-effort (upstream destroys the
//!   socket with a timer).

use futures::future::{BoxFuture, FutureExt};
use std::collections::HashSet;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::remote::framing::DEFAULT_MAX_FRAME_LENGTH;
use crate::remote::schemas::ServerId;
use crate::remote::transport::TransportError;
use super::{
    ByteConnection, ConnectionAcceptor, ConnectionHandler, ListenSpec, ListenerError, Server,
    ServerErrorHandler, ServerHost, ServerListener, ServerOptions, ServerOptionsError,
    build_listener,
};

/// Default socket mode (owner read/write only).
pub const DEFAULT_SOCKET_MODE: u32 = 0o600;
/// Default grace period for draining writes on close.
pub const DEFAULT_GRACEFUL_CLOSE_TIMEOUT_MS: u64 = 5_000;
/// Liveness probe deadline for stale-socket detection.
const SOCKET_PROBE_TIMEOUT_MS: u64 = 1_000;
/// Queued outbound write chunks before writers feel backpressure; the
/// pending-byte budget bounds the bytes inside it.
const WRITE_QUEUE_CAPACITY: usize = 128;
/// Read buffer for inbound chunks.
const READ_CHUNK_BYTES: usize = 16 * 1024;
/// Maximum Unix socket path length in bytes, mirroring
/// `build_transport`'s eager validation.
const MAX_UNIX_SOCKET_PATH_BYTES: usize = if cfg!(target_os = "linux") { 107 } else { 103 };
/// Bound on how long `close` waits for the accept task to settle.
const CLOSE_SETTLE_TIMEOUT_MS: u64 = 5_000;
/// Derives the public Unix socket path for one canonical server identity.
///
/// `ServerId` has already enforced the lowercase UUIDv4 wire spelling, so the
/// helper can append the source-compatible `.sock` suffix without accepting a
/// second, unchecked string identity.
#[must_use]
pub fn unix_socket_path(server_id: &ServerId, directory: &Path) -> PathBuf {
    directory.join(format!("{server_id}.sock"))
}
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Options for [`create_listener`].
#[derive(Clone)]
pub struct UnixListenerOptions {
    /// Socket path.
    pub path: PathBuf,
    /// Socket file mode (default `0o600`).
    pub mode: Option<u32>,
    /// Outbound backpressure budget in bytes per connection.
    pub max_pending_bytes: Option<usize>,
    /// Frame length used to derive the default pending-byte budget.
    pub max_frame_length: Option<usize>,
    /// Grace period for draining writes on close (default 5 s).
    pub graceful_close_timeout_ms: Option<u64>,
    /// Reports listener failures.
    pub on_error: Option<ServerErrorHandler>,
}

impl std::fmt::Debug for UnixListenerOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixListenerOptions")
            .field("path", &self.path)
            .field("mode", &self.mode)
            .field("max_pending_bytes", &self.max_pending_bytes)
            .field("max_frame_length", &self.max_frame_length)
            .field("graceful_close_timeout_ms", &self.graceful_close_timeout_ms)
            .field("on_error", &self.on_error.as_ref().map(|_| "<handler>"))
            .finish()
    }
}

/// Construction-options failure for the Unix listener — distinct from
/// runtime operation errors, mirroring the client's options-error
/// precedent. Surfaced eagerly by [`create_server`]; raw
/// [`create_listener`] use surfaces them as [`ListenerError::Io`] at
/// `start` because the [`ServerListener`] surface is infallible at
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnixListenerOptionsError {
    /// The socket path is the empty string.
    EmptyPath,
    /// The socket path exceeds the platform's `sun_path` budget.
    PathTooLong { max: usize },
    /// The socket mode is outside `0o000..=0o777`.
    InvalidMode,
    /// The frame bound is outside the protocol range.
    InvalidMaxFrameLength,
    /// The pending-byte budget is too small.
    InvalidMaxPendingBytes,
    /// The graceful-close timeout is zero.
    InvalidGracefulTimeout,
}

impl std::fmt::Display for UnixListenerOptionsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => formatter.write_str("unix listener path must not be empty"),
            Self::PathTooLong { max } => {
                write!(formatter, "unix listener path is too long; maximum is {max} UTF-8 bytes")
            }
            Self::InvalidMode => formatter.write_str("unix listener mode must be between 0o000 and 0o777"),
            Self::InvalidMaxFrameLength => formatter.write_str("unix listener maxFrameLength is invalid"),
            Self::InvalidMaxPendingBytes => formatter.write_str("unix listener maxPendingBytes must be at least maxFrameLength + 4"),
            Self::InvalidGracefulTimeout => formatter.write_str("unix listener gracefulCloseTimeoutMs must be positive"),
        }
    }
}

impl std::error::Error for UnixListenerOptionsError {}

#[derive(Clone)]
pub struct UnixServerOptions {
    /// Socket path.
    pub path: PathBuf,
    /// Socket file mode.
    pub mode: Option<u32>,
    /// Outbound backpressure budget in bytes per connection.
    pub max_pending_bytes: Option<usize>,
    /// Grace period for draining writes on close.
    pub graceful_close_timeout_ms: Option<u64>,
    /// Maximum frame length for the composed server.
    pub max_frame_length: Option<usize>,
    /// Handshake deadline for the composed server.
    pub handshake_timeout_ms: Option<u64>,
    /// Stable server id.
    pub server_id: ServerId,
    /// Called after the accepted-connection count changes.
    pub on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    /// Reports isolated server errors.
    pub on_error: Option<ServerErrorHandler>,
}

impl Default for UnixServerOptions {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            mode: None,
            max_pending_bytes: None,
            graceful_close_timeout_ms: None,
            max_frame_length: None,
            handshake_timeout_ms: None,
            server_id: ServerId::new("00000000-0000-4000-8000-000000000000")
                .expect("literal server id is canonical"),
            on_connection_count_changed: None,
            on_error: None,
        }
    }
}

impl std::fmt::Debug for UnixServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixServerOptions")
            .field("path", &self.path)
            .field("mode", &self.mode)
            .field("max_pending_bytes", &self.max_pending_bytes)
            .field("graceful_close_timeout_ms", &self.graceful_close_timeout_ms)
            .field("max_frame_length", &self.max_frame_length)
            .field("handshake_timeout_ms", &self.handshake_timeout_ms)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum UnixServerError {
    /// The listen spec was rejected (typed shared owner).
    Spec(crate::remote::transport::EndpointSpecError),
    /// The preset options were rejected.
    Options(UnixListenerOptionsError),
    /// The composed server options were rejected.
    Server(ServerOptionsError),
}
impl std::fmt::Display for UnixServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spec(error) => write!(formatter, "unix listen spec rejected: {error}"),
            Self::Options(error) => write!(formatter, "unix listener options rejected: {error}"),
            Self::Server(error) => write!(formatter, "server options rejected: {error}"),
        }
    }
}

impl From<UnixListenerOptionsError> for UnixServerError {
    fn from(error: UnixListenerOptionsError) -> Self {
        Self::Options(error)
    }
}

impl std::error::Error for UnixServerError {}

// ---------------------------------------------------------------------------
// Preset (port of upstream transports/unix/preset.ts)
// ---------------------------------------------------------------------------

/// Composes [`Server`] with one Unix-domain socket listener.
pub fn create_server<H: ServerHost>(
    host: Arc<H>,
    options: UnixServerOptions,
) -> Result<Server<H>, UnixServerError> {
    build_listener(&ListenSpec::Unix {
        path: options.path.clone(),
        max_pending_bytes: options.max_pending_bytes,
    })
    .map_err(UnixServerError::Spec)?;
    validate_options(
        &options.path,
        options.mode,
        options.graceful_close_timeout_ms,
        options.max_frame_length,
        options.max_pending_bytes,
    )?;
    let listener = create_listener(UnixListenerOptions {
        path: options.path.clone(),
        mode: options.mode,
        max_pending_bytes: options.max_pending_bytes,
        max_frame_length: options.max_frame_length,
        graceful_close_timeout_ms: options.graceful_close_timeout_ms,
        on_error: options.on_error.clone(),
    });
    Server::new(
        host,
        ServerOptions {
            listeners: vec![listener],
            server_id: options.server_id,
            max_frame_length: options.max_frame_length,
            handshake_timeout_ms: options.handshake_timeout_ms,
            on_connection_count_changed: options.on_connection_count_changed,
            on_error: options.on_error,
        },
    )
    .map_err(UnixServerError::Server)
}

/// Eager validation shared by the preset and the raw listener.
fn validate_options(
    path: &Path,
    mode: Option<u32>,
    graceful_close_timeout_ms: Option<u64>,
    max_frame_length: Option<usize>,
    max_pending_bytes: Option<usize>,
) -> Result<(), UnixListenerOptionsError> {
    if path.as_os_str().is_empty() {
        return Err(UnixListenerOptionsError::EmptyPath);
    }
    if path.as_os_str().len() > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(UnixListenerOptionsError::PathTooLong {
            max: MAX_UNIX_SOCKET_PATH_BYTES,
        });
    }
    if mode.is_some_and(|mode| mode > 0o777) {
        return Err(UnixListenerOptionsError::InvalidMode);
    }
    if graceful_close_timeout_ms == Some(0) {
        return Err(UnixListenerOptionsError::InvalidGracefulTimeout);
    }
    let max_frame_length = max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    if max_frame_length == 0 || (max_frame_length as u64) > 0xffff_ffff {
        return Err(UnixListenerOptionsError::InvalidMaxFrameLength);
    }
    let default_pending = max_frame_length
        .checked_mul(4)
        .ok_or(UnixListenerOptionsError::InvalidMaxPendingBytes)?;
    let max_pending_bytes = max_pending_bytes.unwrap_or(default_pending);
    if max_pending_bytes < max_frame_length.saturating_add(4) {
        return Err(UnixListenerOptionsError::InvalidMaxPendingBytes);
    }
    Ok(())
}

pub fn create_listener(options: UnixListenerOptions) -> Arc<dyn ServerListener> {
    let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    let max_pending_bytes = options
        .max_pending_bytes
        .unwrap_or_else(|| max_frame_length.saturating_mul(4));
    Arc::new(UnixServerListener {
        options: ResolvedOptions {
            path: options.path,
            mode: options.mode.unwrap_or(DEFAULT_SOCKET_MODE),
            max_frame_length,
            max_pending_bytes,
            graceful_close_timeout_ms: options
                .graceful_close_timeout_ms
                .unwrap_or(DEFAULT_GRACEFUL_CLOSE_TIMEOUT_MS),
            on_error: options.on_error,
        },
        state: StdMutex::new(ListenerState::Idle),
        stop: Arc::new(tokio::sync::Notify::new()),
        connections: Arc::new(StdMutex::new(HashSet::new())),
        identity: Arc::new(StdMutex::new(None)),
        closed: Arc::new(StdMutex::new(false)),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerState {
    Idle,
    Started,
    Closing,
}

struct ResolvedOptions {
    path: PathBuf,
    mode: u32,
    max_frame_length: usize,
    max_pending_bytes: usize,
    graceful_close_timeout_ms: u64,
    on_error: Option<ServerErrorHandler>,
}

// ---------------------------------------------------------------------------
// Listener (port of upstream UnixListener)
// ---------------------------------------------------------------------------

struct UnixServerListener {
    options: ResolvedOptions,
    state: StdMutex<ListenerState>,
    stop: Arc<tokio::sync::Notify>,
    connections: Arc<StdMutex<HashSet<Arc<UnixServerConnection>>>>,
    identity: Arc<StdMutex<Option<(u64, u64)>>>,
    closed: Arc<StdMutex<bool>>,
}

impl ServerListener for UnixServerListener {
    fn address(&self) -> Option<String> {
        Some(self.options.path.to_string_lossy().into_owned())
    }

    fn start(&self, accept: ConnectionAcceptor) -> BoxFuture<'static, Result<(), ListenerError>> {
        {
            let mut state = lock(&self.state);
            match *state {
                ListenerState::Started => {
                    return futures::future::ready(Err(ListenerError::AlreadyStarted)).boxed();
                }
                ListenerState::Closing => {
                    return futures::future::ready(Err(ListenerError::Closing)).boxed();
                }
                ListenerState::Idle => *state = ListenerState::Started,
            }
        }
        if let Err(error) = validate_options(
            &self.options.path,
            Some(self.options.mode),
            Some(self.options.graceful_close_timeout_ms),
            Some(self.options.max_frame_length),
            Some(self.options.max_pending_bytes),
        ) {
            *lock(&self.state) = ListenerState::Closing;
            return futures::future::ready(Err(ListenerError::Io(error.to_string()))).boxed();
        }
        let runner = ListenerRunner {
            options: clone_options(&self.options),
            stop: Arc::clone(&self.stop),
            connections: Arc::clone(&self.connections),
            identity_tx: IdentitySink(Arc::clone(&self.identity)),
            closed: Arc::clone(&self.closed),
            accept,
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            match runner.open().await {
                Ok(listener) => {
                    let _ = ready_tx.send(Ok(()));
                    runner.run(listener).await;
                }
                Err(error) => {
                    *lock(&runner.closed) = true;
                    if let Some(callback) = &runner.options.on_error {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            callback(&error);
                        }));
                    }
                    let _ = ready_tx.send(Err(error));
                }
            }
        });
        async move {
            match ready_rx.await {
                Ok(result) => result,
                Err(_) => Err(ListenerError::Io("listener task aborted".to_string())),
            }
        }
        .boxed()
    }
    fn close(&self) -> BoxFuture<'static, ()> {
        let idle = {
            let mut state = lock(&self.state);
            let idle = matches!(*state, ListenerState::Idle);
            *state = ListenerState::Closing;
            idle
        };
        if idle {
            *lock(&self.closed) = true;
            return futures::future::ready(()).boxed();
        }
        self.stop.notify_one();
        let path = self.options.path.clone();
        let closed = Arc::clone(&self.closed);
        async move {
            // Dial the socket to wake up `listener.accept()` from its park
            let _ = UnixStream::connect(&path).await;
            let deadline =
                tokio::time::timeout(Duration::from_millis(CLOSE_SETTLE_TIMEOUT_MS), async {
                    let mut interval = tokio::time::interval(Duration::from_millis(5));
                    loop {
                        interval.tick().await;
                        if *lock(&closed) {
                            break;
                        }
                    }
                })
                .await;
            let _ = deadline;
        }
        .boxed()
    }
}
fn clone_options(options: &ResolvedOptions) -> ResolvedOptions {
    ResolvedOptions {
        path: options.path.clone(),
        mode: options.mode,
        max_frame_length: options.max_frame_length,
        max_pending_bytes: options.max_pending_bytes,
        graceful_close_timeout_ms: options.graceful_close_timeout_ms,
        on_error: options.on_error.clone(),
    }
}

/// Forwards the bound socket identity into the listener handle.
struct IdentitySink(Arc<StdMutex<Option<(u64, u64)>>>);

/// A bound, running listener owned by its spawned task.
struct ListenerRunner {
    options: ResolvedOptions,
    stop: Arc<tokio::sync::Notify>,
    connections: Arc<StdMutex<HashSet<Arc<UnixServerConnection>>>>,
    identity_tx: IdentitySink,
    closed: Arc<StdMutex<bool>>,
    accept: ConnectionAcceptor,
}

impl ListenerRunner {
    /// Binds the socket and records its identity (the fallible half
    /// of startup).
    async fn open(&self) -> Result<TokioUnixListener, ListenerError> {
        let path = &self.options.path;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| ListenerError::Io(error.to_string()))?;
            let _ = tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
        }
        remove_stale_socket(path).await?;
        let listener =
            TokioUnixListener::bind(path).map_err(|error| ListenerError::Io(error.to_string()))?;
        let metadata = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|error| ListenerError::Io(error.to_string()))?;
        if !metadata.file_type().is_socket() {
            return Err(ListenerError::Io(format!(
                "Unix listener path is not a socket after binding: {}",
                path.display()
            )));
        }
        *lock(&self.identity_tx.0) = Some((metadata.dev(), metadata.ino()));
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(self.options.mode)).await;
        Ok(listener)
    }

    /// Accepts connections until stopped, then closes them and unlinks
    /// the socket while its identity still matches.
    async fn run(&self, listener: TokioUnixListener) {
        loop {
            let accepted = tokio::select! {
                () = self.stop.notified() => break,
                accepted = listener.accept() => accepted,
            };
            let (stream, _) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    if let Some(callback) = &self.options.on_error {
                        let failure = ListenerError::Io(error.to_string());
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            callback(&failure);
                        }));
                    }
                    break;
                }
            };
            let connection = spawn_connection(stream, &self.options, &self.accept);
            lock(&self.connections).insert(Arc::clone(&connection));
        }
        drop(listener);
        let connections: Vec<Arc<UnixServerConnection>> = lock(&self.connections).drain().collect();
        for connection in connections {
            connection.close(None).await.ok();
        }
        self.cleanup_owned_socket();
        *lock(&self.closed) = true;

    }
    /// Unlinks the socket only while its identity still matches the
    /// bound one (port of `cleanupOwnedSocket`, simplified to a
    /// guarded unlink).
    fn cleanup_owned_socket(&self) {
        let identity = *lock(&self.identity_tx.0);
        let Some((dev, ino)) = identity else { return };
        let path = &self.options.path;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) => return,
        };
        if !metadata.file_type().is_socket() || metadata.dev() != dev || metadata.ino() != ino {
            return;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// Removes a stale socket at `path`, refusing live or non-socket
/// entries (port of `removeStaleSocket`).
async fn remove_stale_socket(path: &Path) -> Result<(), ListenerError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ListenerError::Io(error.to_string())),
    };
    if !metadata.file_type().is_socket() {
        return Err(ListenerError::Io(format!(
            "Refusing to remove non-socket Unix listener path: {}",
            path.display()
        )));
    }
    if socket_is_live(path).await {
        return Err(ListenerError::Io(format!(
            "Unix listener is already running: {}",
            path.display()
        )));
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ListenerError::Io(error.to_string())),
    }
}

/// Probes whether a socket path still accepts connections (port of
/// `isSocketLive`; a probe timeout counts as live).
async fn socket_is_live(path: &Path) -> bool {
    match tokio::time::timeout(
        Duration::from_millis(SOCKET_PROBE_TIMEOUT_MS),
        UnixStream::connect(path),
    )
    .await
    {
        Ok(Ok(_stream)) => true,
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            false
        }
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Connection (port of upstream UnixByteConnection)
// ---------------------------------------------------------------------------

struct UnixServerConnection {
    write_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    pending_bytes: Arc<AtomicUsize>,
    max_pending_bytes: usize,
    closed: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    send_tail: StdMutex<futures::future::Shared<BoxFuture<'static, ()>>>,
    writer_finished: Arc<tokio::sync::Notify>,
    graceful_close_timeout_ms: u64,
}

/// Spawns the reader/writer tasks for one accepted stream and returns
/// the connection handle.
fn spawn_connection(
    stream: UnixStream,
    options: &ResolvedOptions,
    accept: &ConnectionAcceptor,
) -> Arc<UnixServerConnection> {
    let (write_tx, write_rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
    let connection = Arc::new(UnixServerConnection {
        write_tx: StdMutex::new(Some(write_tx)),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        max_pending_bytes: options.max_pending_bytes,
        closed: Arc::new(AtomicBool::new(false)),
        closing: Arc::new(AtomicBool::new(false)),
        send_tail: StdMutex::new(futures::future::ready(()).boxed().shared()),
        writer_finished: Arc::new(tokio::sync::Notify::new()),
        graceful_close_timeout_ms: options.graceful_close_timeout_ms,
    });

    let (read_half, write_half) = stream.into_split();

    // Writer task: drains the bounded queue to the stream, then shuts
    // the write side down and signals completion.
    // Captures individual Arcs (NOT the connection Arc) so `write_tx`
    // inside `connection` is dropped when all outside holders drop,
    // allowing `write_rx` to close and the writer task to exit.
    {
        let pending_bytes = Arc::clone(&connection.pending_bytes);
        let closed = Arc::clone(&connection.closed);
        let writer_finished = Arc::clone(&connection.writer_finished);
        tokio::spawn(async move {
            let mut write_half = write_half;
            let mut write_rx = write_rx;
            while let Some(chunk) = write_rx.recv().await {
                let written = write_half.write_all(&chunk).await;
                pending_bytes.fetch_sub(chunk.len(), Ordering::SeqCst);
                if written.is_err() {
                    break;
                }
            }
            let _ = write_half.shutdown().await;
            closed.store(true, Ordering::SeqCst);
            writer_finished.notify_one();
        });
    }

    // Reader task: delivers chunks and exactly one terminal event
    // (`on_close` or `on_error`).
    {
        let closed = Arc::clone(&connection.closed);
        let handler: Arc<dyn ConnectionHandler> =
            accept(Arc::clone(&connection) as Arc<dyn ByteConnection>);
        tokio::spawn(async move {
            let mut read_half = read_half;
            let mut buffer = vec![0u8; READ_CHUNK_BYTES];
            loop {
                match read_half.read(&mut buffer).await {
                    Ok(0) => {
                        closed.store(true, Ordering::SeqCst);
                        handler.on_close();
                        break;
                    }
                    Ok(read) => handler.on_data(buffer[..read].to_vec()),
                    Err(error) => {
                        closed.store(true, Ordering::SeqCst);
                        handler.on_error(TransportError::Io(error));
                        break;
                    }
                }
            }
        });
    }

    connection
}

impl ByteConnection for UnixServerConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed.load(Ordering::Acquire) {
            return futures::future::ready(Err(TransportError::Closed)).boxed();
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let pending_bytes = Arc::clone(&self.pending_bytes);
        let max_pending_bytes = self.max_pending_bytes;
        let task = {
            let mut tail = lock(&self.send_tail);
            if self.closed.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
                return futures::future::ready(Err(TransportError::Closed)).boxed();
            }
            let Some(write_tx) = lock(&self.write_tx).clone() else {
                return futures::future::ready(Err(TransportError::Closed)).boxed();
            };
            let previous = tail.clone();
            let task = async move {
                previous.await;
                let bytes = chunk.len();
                let budget = pending_bytes
                    .fetch_add(bytes, Ordering::SeqCst)
                    .saturating_add(bytes);
                let result = if budget > max_pending_bytes {
                    pending_bytes.fetch_sub(bytes, Ordering::SeqCst);
                    Err(TransportError::PendingBytesExceeded)
                } else if write_tx.send(chunk).await.is_err() {
                    pending_bytes.fetch_sub(bytes, Ordering::SeqCst);
                    Err(TransportError::Closed)
                } else {
                    Ok(())
                };
                let _ = sender.send(result);
            }
            .boxed()
            .shared();
            *tail = task.clone();
            task
        };
        tokio::spawn(task);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or(Err(TransportError::Closed))
        })
    }

    fn close(
        &self,
        final_chunk: Option<Vec<u8>>,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return futures::future::ready(Ok(())).boxed();
        }
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let closed = Arc::clone(&self.closed);
        let writer_finished = Arc::clone(&self.writer_finished);
        let timeout = Duration::from_millis(self.graceful_close_timeout_ms);
        let task = {
            let mut tail = lock(&self.send_tail);
            let previous = tail.clone();
            let write_tx = lock(&self.write_tx).take();
            let task = async move {
                previous.await;
                let result = async {
                    if let Some(write_tx) = write_tx {
                        if let Some(chunk) = final_chunk {
                            write_tx.send(chunk).await.ok();
                        }
                        drop(write_tx);
                        let _ = tokio::time::timeout(timeout, writer_finished.notified()).await;
                    }
                    Ok(())
                }
                .await;
                closed.store(true, Ordering::Release);
                let _ = sender.send(result);
            }
            .boxed()
            .shared();
            *tail = task.clone();
            task
        };
        tokio::spawn(task);
        Box::pin(async move {
            receiver
                .await
                .unwrap_or(Err(TransportError::Closed))
        })
    }
}
impl PartialEq for UnixServerConnection {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for UnixServerConnection {}

impl std::hash::Hash for UnixServerConnection {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self, state);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

