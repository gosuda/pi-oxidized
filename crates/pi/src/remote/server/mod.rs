//! Reusable protocol-v8 server and session router.
//!
//! The server is transport-neutral: listeners only adapt ordered byte I/O,
//! while this module owns handshake, request cancellation, attachment fences,
//! per-client ordering, and shutdown.  A [`ServerHost`] supplies the concrete
//! repository metadata and service capabilities; the server never truncates a
//! backend metadata record to the base `SessionMetadata` shape.
//!
//! Opaque service calls, results, and updates are the canonical
//! `pi_agent::service::value::JsonValue`.  The private remote CBOR adapters
//! (`CborValue`/`OpaqueJson`) exist only at the envelope boundary: in-process
//! values retain UTF-16 lone surrogates, while CBOR encoding rejects those
//! surrogates with a scalar-value error.  Byte strings never enter opaque JSON.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared, join_all};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

use pi_agent::context::Context;
use pi_agent::service::delta::DeltaOp;
use pi_agent::service::state_codec::ServiceStateEncoder;
use pi_agent::service::value::JsonValue;
use pi_agent::service::wire::{
    ServiceControlCall, ServiceProviderUpdate, decode_service_control_call, parse_service_call,
    parse_service_subscription_snapshot,
};

use crate::remote::codec::{
    ClientMessageDecoder, CodecError, encode_server_message, is_supported_protocol_version,
};
use crate::remote::framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoderOptions};
use crate::remote::schemas::{
    ClientMessage, ProtocolError, PROTOCOL_VERSION, RpcTarget, ServerId, ServerMessage,
    SessionTarget,
};
use crate::remote::transport::TransportError;

mod connection;
mod errors;
mod host;
mod router;

#[cfg(unix)]
pub mod unix;

pub use connection::{
    ByteConnection, ConnectionAcceptor, ConnectionHandler, InMemoryServerListener, ListenSpec,
    ListenerError, ServerListener, build_listener,
};
pub use errors::{
    HostError, INTERNAL_SERVER_ERROR_MESSAGE, ServerError, ServerErrorCode, to_protocol_error,
};
pub use host::{
    PublishUpdate, RoutedServerPresentation, RoutedServerServiceAttachment,
    RoutedServerServiceHost, RoutedSessionAttachment, RoutedSessionHandle, ServerHost,
};

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

const MAX_UINT32: u64 = 0xffff_ffff;
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;
const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

/// Reports an isolated server error.  Error observers cannot affect state.
pub type ServerErrorHandler = Arc<dyn Fn(&(dyn Error + 'static)) + Send + Sync>;

/// Construction-time option failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerOptionsError {
    /// The frame bound is outside the unsigned 32-bit protocol range.
    InvalidMaxFrameLength { value: u64, max: u64 },
    /// The handshake deadline is outside the signed 32-bit timer range.
    InvalidHandshakeTimeout { value: u64, max: u64 },
}

impl fmt::Display for ServerOptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaxFrameLength { value, max } => write!(
                formatter,
                "Server maxFrameLength must be an integer between 1 and {max} (got {value})"
            ),
            Self::InvalidHandshakeTimeout { value, max } => write!(
                formatter,
                "Server handshakeTimeoutMs must be an integer between 1 and {max} (got {value})"
            ),
        }
    }
}

impl Error for ServerOptionsError {}

/// Options for constructing a [`Server`].
#[derive(Clone)]
pub struct ServerOptions {
    /// Listeners supplying established connections.
    pub listeners: Vec<Arc<dyn ServerListener>>,
    /// Stable logical server identity.
    pub server_id: ServerId,
    /// Maximum framed payload length.
    pub max_frame_length: Option<usize>,
    /// Handshake deadline in milliseconds.
    pub handshake_timeout_ms: Option<u64>,
    /// Called after the accepted-connection count changes.
    pub on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    /// Receives local failures that cannot be sent to a peer.
    pub on_error: Option<ServerErrorHandler>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            listeners: Vec::new(),
            server_id: ServerId::new("00000000-0000-4000-8000-000000000000")
                .expect("literal server id is canonical"),
            max_frame_length: None,
            handshake_timeout_ms: None,
            on_connection_count_changed: None,
            on_error: None,
        }
    }
}

impl fmt::Debug for ServerOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerOptions")
            .field("listeners", &self.listeners.len())
            .field("server_id", &self.server_id)
            .field("max_frame_length", &self.max_frame_length)
            .field("handshake_timeout_ms", &self.handshake_timeout_ms)
            .finish_non_exhaustive()
    }
}

/// Startup failure, including failures observed while cleaning up listeners.
#[derive(Debug)]
pub enum ServerStartError {
    /// The server is already listening.
    AlreadyStarted,
    /// Another startup is in progress.
    AlreadyStarting,
    /// The server is closing or closed.
    Closing,
    /// A listener rejected startup.
    Listener(ListenerError),
    /// Startup failed and cleanup also reported failures.
    Cleanup {
        /// Original startup failure.
        start: Box<ServerStartError>,
        /// Cleanup failures retained in order.
        cleanup: Vec<ServerCloseError>,
    },
}

impl fmt::Display for ServerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("server is already started"),
            Self::AlreadyStarting => formatter.write_str("server is already starting"),
            Self::Closing => formatter.write_str("server is closing or closed"),
            Self::Listener(error) => write!(formatter, "listener startup failed: {error}"),
            Self::Cleanup { start, cleanup } => {
                write!(formatter, "server startup failed ({start}); cleanup failures: {}", cleanup.len())
            }
        }
    }
}

impl Error for ServerStartError {}

/// Shutdown failure retaining all routed-session causes.
#[derive(Debug)]
pub enum ServerCloseError {
    /// A listener lifecycle operation failed.
    Listener(ListenerError),
    /// One or more connection/router/host operations failed.
    Sessions(Vec<HostError>),
}

impl Clone for ServerCloseError {
    fn clone(&self) -> Self {
        match self {
            Self::Listener(error) => Self::Listener(error.clone()),
            Self::Sessions(errors) => Self::Sessions(
                errors
                    .iter()
                    .map(crate::remote::server::errors::duplicate_host_error)
                    .collect(),
            ),
        }
    }
}

impl fmt::Display for ServerCloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => write!(formatter, "listener shutdown failed: {error}"),
            Self::Sessions(errors) => write!(formatter, "{} routed session cleanup failures", errors.len()),
        }
    }
}

impl Error for ServerCloseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Idle,
    Starting,
    Started,
    Closing,
    Closed,
}

struct ClosedSignal {
    notify: Notify,
    result: StdMutex<Option<Result<(), ServerCloseError>>>,
}

impl ClosedSignal {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            result: StdMutex::new(None),
        }
    }
}

struct ActiveRequest {
    token: CancellationToken,
    target: RpcTarget,
}

struct ConnInner {
    stage: Stage,
    disconnected: bool,
    watchdog: Option<tokio::task::AbortHandle>,
    server_services: Option<Arc<dyn RoutedServerServiceAttachment>>,
    active_requests: HashMap<String, ActiveRequest>,
    state_encoders: HashMap<String, ServiceStateEncoder>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    AwaitingHello,
    Handshaking,
    Ready,
    Closing,
    Closed,
}

struct ServerConnection {
    key: router::ClientKey,
    connection: Arc<dyn ByteConnection>,
    decoder: StdMutex<ClientMessageDecoder>,
    inner: StdMutex<ConnInner>,
    handshake_done: Notify,
    events: StdMutex<Shared<BoxFuture<'static, ()>>>,
    sends: StdMutex<Shared<BoxFuture<'static, ()>>>,
}

impl ServerConnection {
    fn new(
        key: router::ClientKey,
        connection: Arc<dyn ByteConnection>,
        max_frame_length: usize,
    ) -> Result<Self, CodecError> {
        Ok(Self {
            key,
            connection,
            decoder: StdMutex::new(ClientMessageDecoder::new(Some(FrameDecoderOptions {
                max_frame_length,
            }))?),
            inner: StdMutex::new(ConnInner {
                stage: Stage::AwaitingHello,
                disconnected: false,
                watchdog: None,
                server_services: None,
                active_requests: HashMap::new(),
                state_encoders: HashMap::new(),
            }),
            handshake_done: Notify::new(),
            events: StdMutex::new(futures::future::ready(()).boxed().shared()),
            sends: StdMutex::new(futures::future::ready(()).boxed().shared()),
        })
    }

    fn terminal(&self) -> bool {
        let inner = lock(&self.inner);
        inner.disconnected || matches!(inner.stage, Stage::Closing | Stage::Closed)
    }

    fn enqueue_event(&self, event: BoxFuture<'static, ()>) {
        let task = {
            let mut tail = lock(&self.events);
            let previous = tail.clone();
            let task = async move {
                previous.await;
                event.await;
            }
            .boxed()
            .shared();
            *tail = task.clone();
            task
        };
        tokio::spawn(task);
    }

    fn send_frame(&self, frame: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>> {
        let connection = Arc::clone(&self.connection);
        let (sender, receiver) = oneshot::channel();
        let task = {
            let mut tail = lock(&self.sends);
            let previous = tail.clone();
            let task = async move {
                previous.await;
                let result = connection.send(frame).await;
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

    fn close_frame(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<'static, Result<(), TransportError>> {
        let connection = Arc::clone(&self.connection);
        let (sender, receiver) = oneshot::channel();
        let task = {
            let mut tail = lock(&self.sends);
            let previous = tail.clone();
            let task = async move {
                previous.await;
                let result = connection.close(final_chunk).await;
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

struct ServerCore<H: ServerHost> {
    host: Arc<H>,
    server_id: ServerId,
    listeners: Vec<Arc<dyn ServerListener>>,
    max_frame_length: usize,
    handshake_timeout_ms: u64,
    on_connection_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    on_error: Option<ServerErrorHandler>,
    closing: AtomicBool,
    lifecycle: StdMutex<Lifecycle>,
    connections: StdMutex<HashMap<router::ClientKey, Arc<ServerConnection>>>,
    next_connection: AtomicU64,
    start_done: Notify,
    closed: Arc<ClosedSignal>,
    close_future: StdMutex<Option<Shared<BoxFuture<'static, Result<(), ServerCloseError>>>>>,
    router: Arc<router::SessionRouter<H>>,
}

/// A transport-neutral routed remote server.
pub struct Server<H: ServerHost> {
    core: Arc<ServerCore<H>>,
}

impl<H: ServerHost> fmt::Debug for Server<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Server")
            .field("server_id", &self.core.server_id)
            .finish_non_exhaustive()
    }
}

impl<H: ServerHost> Server<H> {
    /// Validates options and constructs an unstarted server.
    pub fn new(host: Arc<H>, options: ServerOptions) -> Result<Self, ServerOptionsError> {
        let max_frame_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
        if max_frame_length == 0 || (max_frame_length as u64) > MAX_UINT32 {
            return Err(ServerOptionsError::InvalidMaxFrameLength {
                value: max_frame_length as u64,
                max: MAX_UINT32,
            });
        }
        let handshake_timeout_ms = options
            .handshake_timeout_ms
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        if handshake_timeout_ms == 0 || handshake_timeout_ms > MAX_TIMER_DELAY_MS {
            return Err(ServerOptionsError::InvalidHandshakeTimeout {
                value: handshake_timeout_ms,
                max: MAX_TIMER_DELAY_MS,
            });
        }
        let closed = Arc::new(ClosedSignal::new());
        let listeners = options.listeners;
        let server_id = options.server_id;
        let on_connection_count_changed = options.on_connection_count_changed;
        let on_error = options.on_error;
        let core = Arc::new_cyclic(|weak: &Weak<ServerCore<H>>| {
            let is_closing: Arc<dyn Fn() -> bool + Send + Sync> = {
                let weak = weak.clone();
                Arc::new(move || weak.upgrade().is_none_or(|core| core.closing.load(Ordering::Acquire)))
            };
            let publish_attachment = {
                let weak = weak.clone();
                Arc::new(move |client, attachment, _context| {
                    let weak = weak.clone();
                    Box::pin(async move {
                        let Some(core) = weak.upgrade() else { return; };
                        let Some(connection) = core.connection(client) else { return; };
                        let _ = core
                            .send_message(
                                &connection,
                                ServerMessage::Attachment { attachment },
                            )
                            .await;
                    }) as BoxFuture<'static, ()>
                })
                    as Arc<dyn Fn(router::ClientKey, Option<SessionTarget>, Context) -> BoxFuture<'static, ()> + Send + Sync>
            };
            let report_error = {
                let weak = weak.clone();
                Arc::new(move |error: &(dyn Error + 'static)| {
                    if let Some(core) = weak.upgrade() {
                        core.report_error(error);
                    }
                }) as ServerErrorHandler
            };
            let router = Arc::new(router::SessionRouter::new(router::SessionRouterOptions {
                host: Arc::clone(&host),
                server_id: server_id.clone(),
                is_closing,
                publish_attachment,
                report_error,
            }));
            ServerCore {
                host: Arc::clone(&host),
                server_id: server_id.clone(),
                listeners: listeners.clone(),
                max_frame_length,
                handshake_timeout_ms,
                on_connection_count_changed: on_connection_count_changed.clone(),
                on_error: on_error.clone(),
                closing: AtomicBool::new(false),
                lifecycle: StdMutex::new(Lifecycle::Idle),
                connections: StdMutex::new(HashMap::new()),
                next_connection: AtomicU64::new(1),
                start_done: Notify::new(),
                closed: Arc::clone(&closed),
                close_future: StdMutex::new(None),
                router,
            }
        });
        Ok(Self { core })
    }

    /// Returns the stable server identity used by every route fence.
    #[must_use]
    pub fn server_id(&self) -> &ServerId {
        &self.core.server_id
    }

    /// Starts every configured listener.
    pub async fn start(&self) -> Result<(), ServerStartError> {
        {
            let mut lifecycle = lock(&self.core.lifecycle);
            match *lifecycle {
                Lifecycle::Idle => *lifecycle = Lifecycle::Starting,
                Lifecycle::Starting => return Err(ServerStartError::AlreadyStarting),
                Lifecycle::Started => return Err(ServerStartError::AlreadyStarted),
                Lifecycle::Closing | Lifecycle::Closed => return Err(ServerStartError::Closing),
            }
        }
        let mut started = Vec::new();
        let mut start_error = None;
        for listener in &self.core.listeners {
            if self.core.closing.load(Ordering::Acquire) {
                start_error = Some(ServerStartError::Closing);
                break;
            }
            let acceptor = self.acceptor();
            match listener.start(acceptor).await {
                Ok(()) => started.push(Arc::clone(listener)),
                Err(error) => {
                    start_error = Some(ServerStartError::Listener(error));
                    break;
                }
            }
        }
        if let Some(start_error) = start_error {
            self.core.closing.store(true, Ordering::Release);
            for listener in started {
                listener.close().await;
            }
            let cleanup_errors = self.core.close_server_state().await;
            let cleanup = if cleanup_errors.is_empty() {
                Vec::new()
            } else {
                vec![ServerCloseError::Sessions(cleanup_errors)]
            };
            *lock(&self.core.lifecycle) = Lifecycle::Closed;
            self.core.start_done.notify_waiters();
            let result = if cleanup.is_empty() {
                start_error
            } else {
                ServerStartError::Cleanup {
                    start: Box::new(start_error),
                    cleanup,
                }
            };
            let close_error = match &result {
                ServerStartError::Cleanup { cleanup, .. } => cleanup[0].clone(),
                ServerStartError::Listener(error) => ServerCloseError::Listener(error.clone()),
                _ => ServerCloseError::Sessions(Vec::new()),
            };
            self.core.settle_closed(Err(close_error));
            return Err(result);
        }
        *lock(&self.core.lifecycle) = Lifecycle::Started;
        self.core.start_done.notify_waiters();
        Ok(())
    }

    /// Accepts one already-authorized byte connection.
    pub fn accept(&self, connection: Arc<dyn ByteConnection>) -> Arc<dyn ConnectionHandler> {
        if self.core.closing.load(Ordering::Acquire) {
            let core = Arc::clone(&self.core);
            tokio::spawn(async move {
                let _ = connection.close(None).await;
                drop(core);
            });
            return Arc::new(ClosedHandler {
                on_error: self.core.on_error.clone(),
            });
        }
        let key = router::ClientKey(self.core.next_connection.fetch_add(1, Ordering::Relaxed));
        let connection_state = match ServerConnection::new(key, Arc::clone(&connection), self.core.max_frame_length) {
            Ok(connection_state) => Arc::new(connection_state),
            Err(error) => {
                self.core.report_error(&error);
                return Arc::new(ClosedHandler {
                    on_error: self.core.on_error.clone(),
                });
            }
        };
        lock(&self.core.connections).insert(key, Arc::clone(&connection_state));
        self.core.notify_connection_count_changed();
        self.install_watchdog(&connection_state);
        Arc::new(ServerHandler {
            core: Arc::downgrade(&self.core),
            connection: Arc::downgrade(&connection_state),
        })
    }

    /// Begins an idempotent graceful shutdown.
    pub async fn close(&self) -> Result<(), ServerCloseError> {
        let future = {
            let mut slot = lock(&self.core.close_future);
            if let Some(future) = slot.as_ref() {
                future.clone()
            } else {
                self.core.closing.store(true, Ordering::Release);
                let core = Arc::clone(&self.core);
                let future = async move { core.close_internal().await }.boxed().shared();
                *slot = Some(future.clone());
                future
            }
        };
        future.await
    }

    /// Returns a future that settles when shutdown has completed.
    pub fn closed(&self) -> BoxFuture<'static, Result<(), ServerCloseError>> {
        let signal = Arc::clone(&self.core.closed);
        Box::pin(async move {
            loop {
                let notified = signal.notify.notified();
                if let Some(result) = lock(&signal.result).as_ref() {
                    return result.clone();
                }
                notified.await;
            }
        })
    }

    fn acceptor(&self) -> ConnectionAcceptor {
        let core = Arc::clone(&self.core);
        Arc::new(move |connection| {
            let server = Server { core: Arc::clone(&core) };
            server.accept(connection)
        })
    }

    fn install_watchdog(&self, connection: &Arc<ServerConnection>) {
        let weak_core = Arc::downgrade(&self.core);
        let weak_connection = Arc::downgrade(connection);
        let timeout = self.core.handshake_timeout_ms;
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout)).await;
            let (Some(core), Some(connection)) = (weak_core.upgrade(), weak_connection.upgrade()) else {
                return;
            };
            if matches!(lock(&connection.inner).stage, Stage::AwaitingHello | Stage::Handshaking) {
                core.fail_protocol(
                    &connection,
                    ProtocolError {
                        code: "invalid_request".to_owned(),
                        message: "Handshake timeout".to_owned(),
                    },
                )
                .await;
            }
        });
        lock(&connection.inner).watchdog = Some(task.abort_handle());
    }
}

impl<H: ServerHost> ServerCore<H> {
    fn connection(&self, client: router::ClientKey) -> Option<Arc<ServerConnection>> {
        lock(&self.connections).get(&client).cloned()
    }

    fn notify_connection_count_changed(&self) {
        let Some(callback) = self.on_connection_count_changed.as_ref() else { return; };
        let count = lock(&self.connections).len();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(count)));
        if result.is_err() {
            // A callback panic is intentionally isolated from server state.
        }
    }

    fn report_error(&self, error: &(dyn Error + 'static)) {
        let Some(callback) = self.on_error.as_ref() else { return; };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(error)));
    }

    fn settle_closed(&self, result: Result<(), ServerCloseError>) {
        let mut slot = lock(&self.closed.result);
        if slot.is_none() {
            *slot = Some(result);
            self.closed.notify.notify_waiters();
        }
    }

    async fn close_internal(self: Arc<Self>) -> Result<(), ServerCloseError> {
        if matches!(*lock(&self.lifecycle), Lifecycle::Starting) {
            let notified = self.start_done.notified();
            if matches!(*lock(&self.lifecycle), Lifecycle::Starting) {
                notified.await;
            }
        }
        *lock(&self.lifecycle) = Lifecycle::Closing;
        for listener in &self.listeners {
            listener.close().await;
        }
        let errors = self.close_server_state().await;
        *lock(&self.lifecycle) = Lifecycle::Closed;
        self.start_done.notify_waiters();
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(ServerCloseError::Sessions(errors))
        };
        self.settle_closed(result.clone());
        result
    }

    async fn close_server_state(&self) -> Vec<HostError> {
        let connections = lock(&self.connections).values().cloned().collect::<Vec<_>>();
        for connection in &connections {
            let mut inner = lock(&connection.inner);
            inner.stage = Stage::Closing;
            if let Some(watchdog) = inner.watchdog.take() {
                watchdog.abort();
            }
            connection.handshake_done.notify_waiters();
        }
        let close_results = join_all(connections.iter().map(|connection| connection.close_frame(None))).await;
        let mut errors = Vec::new();
        for result in close_results {
            if let Err(error) = result {
                self.report_error(&error);
                errors.push(HostError::Other(Box::new(error)));
            }
        }
        for connection in connections {
            self.disconnect(&connection).await;
        }
        if let Err(error) = Arc::clone(&self.router).close(Context::background()).await {
            errors.push(error);
        }
        errors
    }

    async fn receive(self: Arc<Self>, connection: Arc<ServerConnection>, chunk: Vec<u8>) {
        if connection.terminal() {
            return;
        }
        let messages = {
            let mut decoder = lock(&connection.decoder);
            decoder.push(&chunk)
        };
        let messages = match messages {
            Ok(messages) => messages,
            Err(error) => {
                self.fail_protocol(&connection, self.to_protocol_error(&error)).await;
                return;
            }
        };
        for message in messages {
            if connection.terminal() {
                return;
            }
            Arc::clone(&self).dispatch_message(&connection, message).await;
        }
    }

    async fn dispatch_message(
        self: Arc<Self>,
        connection: &Arc<ServerConnection>,
        message: ClientMessage,
    ) {
        let stage = lock(&connection.inner).stage;
        match stage {
            Stage::AwaitingHello => match message {
                ClientMessage::Hello { version } => {
                    lock(&connection.inner).stage = Stage::Handshaking;
                    self.finish_handshake(connection, version).await;
                }
                _ => {
                    self.fail_protocol(
                        connection,
                        ProtocolError {
                            code: "invalid_request".to_owned(),
                            message: "The first client message must be hello".to_owned(),
                        },
                    )
                    .await;
                }
            },
            Stage::Handshaking => {
                if matches!(message, ClientMessage::Hello { .. }) {
                    self.fail_protocol(
                        connection,
                        ProtocolError {
                            code: "invalid_request".to_owned(),
                            message: "hello may only be sent as the first message".to_owned(),
                        },
                    )
                    .await;
                }
            }
            Stage::Ready => match message {
                ClientMessage::Cancel { id, target } => self.handle_cancel(connection, id, target),
                ClientMessage::Request { id, target, call } => {
                    let core = Arc::clone(&self);
                    let connection = Arc::clone(connection);
                    tokio::spawn(async move {
                        core.handle_request(&connection, id, target, call).await;
                    });
                }
                ClientMessage::Hello { .. } => {
                    self.fail_protocol(
                        connection,
                        ProtocolError {
                            code: "invalid_request".to_owned(),
                            message: "hello may only be sent as the first message".to_owned(),
                        },
                    )
                    .await;
                }
            },
            Stage::Closing | Stage::Closed => {}
        }
    }


    async fn finish_handshake(&self, connection: &Arc<ServerConnection>, version: u64) {
        if !is_supported_protocol_version(version) {
            self.fail_protocol(
                connection,
                ProtocolError {
                    code: "version".to_owned(),
                    message: format!("Unsupported protocol version {version}; expected {PROTOCOL_VERSION}"),
                },
            )
            .await;
            return;
        }
        if self.closing.load(Ordering::Acquire) || connection.terminal() {
            return;
        }
        let presentation: Arc<dyn RoutedServerPresentation> = Arc::new(router::ServerPresentation::new(
            Arc::clone(&self.router),
            connection.key,
        ));
        let services = match self
            .host
            .server_services()
            .attach_client(presentation, Context::background())
            .await
        {
            Ok(services) => services,
            Err(error) => {
                self.fail_protocol(connection, to_protocol_error(&error)).await;
                return;
            }
        };
        if self.closing.load(Ordering::Acquire) || connection.terminal() {
            let _ = services.release(Context::background()).await;
            return;
        }
        lock(&connection.inner).server_services = Some(services);
        if self
            .send_message(
                connection,
                ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    server_id: self.server_id.clone(),
                },
            )
            .await
        {
            let mut inner = lock(&connection.inner);
            if matches!(inner.stage, Stage::Handshaking) && !inner.disconnected {
                inner.stage = Stage::Ready;
                if let Some(watchdog) = inner.watchdog.take() {
                    watchdog.abort();
                }
                connection.handshake_done.notify_waiters();
            }
        }
    }

    fn handle_cancel(&self, connection: &Arc<ServerConnection>, id: String, target: RpcTarget) {
        if target.server_id() != &self.server_id {
            return;
        }
        let token = lock(&connection.inner)
            .active_requests
            .get(&id)
            .filter(|active| same_target(&active.target, &target))
            .map(|active| active.token.clone());
        if let Some(token) = token {
            token.cancel();
        }
    }

    async fn handle_request(
        self: Arc<Self>,
        connection: &Arc<ServerConnection>,
        id: String,
        target: RpcTarget,
        raw_call: JsonValue,
    ) {
        if lock(&connection.inner).active_requests.contains_key(&id) {
            let _ = self
                .send_message(
                    connection,
                    ServerMessage::ResponseError {
                        id,
                        error: ProtocolError {
                            code: "invalid_request".to_owned(),
                            message: "Request ID is already active".to_owned(),
                        },
                    },
                )
                .await;
            return;
        }
        let call = match parse_service_call(&raw_call) {
            Ok(call) => call,
            Err(_) => {
                let _ = self
                    .send_message(
                        connection,
                        ServerMessage::ResponseError {
                            id,
                            error: ProtocolError {
                                code: "invalid_request".to_owned(),
                                message: "Invalid service call".to_owned(),
                            },
                        },
                    )
                    .await;
                return;
            }
        };
        let (context, token) = Context::background().with_cancel();
        let active = ActiveRequest {
            token,
            target: target.clone(),
        };
        lock(&connection.inner)
            .active_requests
            .insert(id.clone(), active);

        let control = decode_service_control_call(&call);
        let subscription_id = match control.as_ref() {
            Some(ServiceControlCall::Subscribe { subscription_id, .. }) => {
                match subscription_id.try_to_utf8() {
                    Ok(value) => Some(value),
                    Err(_) => {
                        lock(&connection.inner).active_requests.remove(&id);
                        let _ = self
                            .send_message(
                                connection,
                                ServerMessage::ResponseError {
                                    id,
                                    error: ProtocolError {
                                        code: "invalid_request".to_owned(),
                                        message: "Invalid service call".to_owned(),
                                    },
                                },
                            )
                            .await;
                        return;
                    }
                }
            }
            _ => None,
        };
        if let Some(subscription_id) = subscription_id.as_ref() {
            if lock(&connection.inner)
                .state_encoders
                .contains_key(subscription_id)
            {
                let error = HostError::Protocol(format!(
                    "Duplicate service subscription {subscription_id}"
                ));
                let _ = self
                    .send_message(
                        connection,
                        ServerMessage::ResponseError {
                            id: id.clone(),
                            error: to_protocol_error(&error),
                        },
                    )
                    .await;
                lock(&connection.inner).active_requests.remove(&id);
                return;
            }
        }

        let pending = Arc::new(PendingUpdates::new(subscription_id.clone()));
        let pending_for_publish = Arc::clone(&pending);
        let core = Arc::clone(&self);
        let connection_for_publish = Arc::clone(connection);
        let publish: PublishUpdate = Arc::new(move |subscription, update, _context| {
            let pending = Arc::clone(&pending_for_publish);
            let core = Arc::clone(&core);
            let connection = Arc::clone(&connection_for_publish);
            Box::pin(async move {
                if pending.buffer(&subscription, update.clone()) {
                    return;
                }
                let _ = core
                    .send_service_update(&connection, subscription, update)
                    .await;
            })
        });

        let result = if target.server_id() != &self.server_id {
            Err(ServerError::wrong_server().into())
        } else {
            match &target {
                RpcTarget::Session(_) => self
                    .router
                    .clone()
                    .execute_service_call(call, target.clone(), connection.key, publish.clone(), context.clone())
                    .await,
                RpcTarget::Server(_) => {
                    let services = lock(&connection.inner).server_services.clone();
                    match services {
                        Some(services) => services
                            .invoke_service(call, publish, context.clone())
                            .await,
                        None => Err(HostError::Protocol(
                            "Unknown service member".to_owned(),
                        )),
                    }
                }
            }
        };

        let mut installed = false;
        let mut result = result;
        if let Some(subscription_id) = subscription_id.as_ref() {
            let original = result;
            result = match original {
                Ok(Some(snapshot)) => match parse_service_subscription_snapshot(&snapshot) {
                    Ok(snapshot) => {
                        let mut encoder = ServiceStateEncoder::new();
                        match encoder.encode_snapshot(&snapshot) {
                            Ok(snapshot) => {
                                lock(&connection.inner)
                                    .state_encoders
                                    .insert(subscription_id.clone(), encoder);
                                installed = true;
                                Ok(Some(snapshot.into_json()))
                            }
                            Err(error) => Err(HostError::Protocol(error.to_string())),
                        }
                    }
                    Err(error) => Err(HostError::Protocol(error.to_string())),
                },
                Ok(None) => Err(HostError::Protocol(
                    "Service subscription did not return a snapshot".to_owned(),
                )),
                Err(error) => Err(error),
            };
        } else if let Some(ServiceControlCall::Unsubscribe { subscription_id }) = control.as_ref() {
            if let Ok(subscription_id) = subscription_id.try_to_utf8() {
                lock(&connection.inner).state_encoders.remove(&subscription_id);
            }
        }

        match result {
            Ok(result) => {
                let sent = self
                    .send_message(
                        connection,
                        ServerMessage::Response {
                            id: id.clone(),
                            result,
                        },
                    )
                    .await;
                if sent && subscription_id.is_some() {
                    let updates = pending.activate();
                    for update in updates {
                        let Some(subscription_id) = subscription_id.as_ref() else { break; };
                        let _ = self
                            .send_service_update(connection, subscription_id.clone(), update)
                            .await;
                    }
                }
            }
            Err(error) => {
                if installed {
                    if let Some(subscription_id) = subscription_id.as_ref() {
                        lock(&connection.inner).state_encoders.remove(subscription_id);
                    }
                }
                let error = if context.is_cancelled() {
                    ProtocolError {
                        code: "cancelled".to_owned(),
                        message: "RPC request cancelled".to_owned(),
                    }
                } else {
                    to_protocol_error(&error)
                };
                let _ = self
                    .send_message(
                        connection,
                        ServerMessage::ResponseError {
                            id: id.clone(),
                            error,
                        },
                    )
                    .await;
            }
        }
        lock(&connection.inner).active_requests.remove(&id);
    }

    async fn send_service_update(
        &self,
        connection: &Arc<ServerConnection>,
        subscription_id: String,
        update: ServiceProviderUpdate<DeltaOp>,
    ) -> bool {
        let encoded = {
            let mut inner = lock(&connection.inner);
            let Some(encoder) = inner.state_encoders.get_mut(&subscription_id) else {
                return true;
            };
            encoder.encode_update(&update)
        };
        let update = match encoded {
            Ok(update) => update.into_json(),
            Err(error) => {
                self.report_error(&error);
                let _ = self.close_connection(connection, None).await;
                self.disconnect(connection).await;
                return false;
            }
        };
        self.send_message(
            connection,
            ServerMessage::ServiceUpdate {
                subscription_id,
                update,
            },
        )
        .await
    }

    async fn send_message(&self, connection: &Arc<ServerConnection>, message: ServerMessage) -> bool {
        if connection.terminal() || connection.connection.closed() {
            return false;
        }
        let frame = match encode_server_message(
            &message,
            Some(FrameDecoderOptions {
                max_frame_length: self.max_frame_length,
            }),
        ) {
            Ok(frame) => frame,
            Err(error) => {
                self.report_error(&error);
                let _ = self.close_connection(connection, None).await;
                self.disconnect(connection).await;
                return false;
            }
        };
        match connection.send_frame(frame).await {
            Ok(()) => true,
            Err(error) => {
                self.report_error(&error);
                let _ = self.close_connection(connection, None).await;
                self.disconnect(connection).await;
                false
            }
        }
    }

    async fn fail_protocol(&self, connection: &Arc<ServerConnection>, error: ProtocolError) {
        {
            let mut inner = lock(&connection.inner);
            if inner.disconnected || matches!(inner.stage, Stage::Closing | Stage::Closed) {
                return;
            }
            inner.stage = Stage::Closing;
            if let Some(watchdog) = inner.watchdog.take() {
                watchdog.abort();
            }
            connection.handshake_done.notify_waiters();
        }
        let frame = encode_server_message(
            &ServerMessage::HelloError { error },
            Some(FrameDecoderOptions {
                max_frame_length: self.max_frame_length,
            }),
        )
        .ok();
        if let Err(error) = self.close_connection(connection, frame).await {
            self.report_error(&error);
        }
        self.disconnect(connection).await;
    }

    async fn transport_closed(&self, connection: &Arc<ServerConnection>) {
        if !connection.terminal() {
            let result = lock(&connection.decoder).end();
            if let Err(error) = result {
                self.report_error(&error);
            }
        }
        self.disconnect(connection).await;
    }

    async fn disconnect(&self, connection: &Arc<ServerConnection>) {
        let (services, key, was_disconnected) = {
            let mut inner = lock(&connection.inner);
            if inner.disconnected {
                (None, connection.key, true)
            } else {
                inner.disconnected = true;
                inner.stage = Stage::Closed;
                if let Some(watchdog) = inner.watchdog.take() {
                    watchdog.abort();
                }
                for active in inner.active_requests.values() {
                    active.token.cancel();
                }
                inner.active_requests.clear();
                inner.state_encoders.clear();
                (inner.server_services.take(), connection.key, false)
            }
        };
        connection.handshake_done.notify_waiters();
        if was_disconnected {
            return;
        }
        let removed = lock(&self.connections).remove(&key).is_some();
        if removed {
            self.notify_connection_count_changed();
        }
        let router = Arc::clone(&self.router).disconnect(key, Context::background());
        let service = services.as_ref().map(|service| service.release(Context::background()));
        let router_result = router.await;
        if let Err(error) = router_result {
            self.report_error(&error);
        }
        if let Some(service) = service {
            if let Err(error) = service.await {
                self.report_error(&error);
            }
        }
    }

    async fn close_connection(
        &self,
        connection: &Arc<ServerConnection>,
        final_chunk: Option<Vec<u8>>,
    ) -> Result<(), TransportError> {
        connection.close_frame(final_chunk).await
    }

    fn to_protocol_error(&self, error: &CodecError) -> ProtocolError {
        ProtocolError {
            code: "invalid_request".to_owned(),
            message: error.to_string(),
        }
    }
}

struct ServerHandler<H: ServerHost> {
    core: Weak<ServerCore<H>>,
    connection: Weak<ServerConnection>,
}

impl<H: ServerHost> ConnectionHandler for ServerHandler<H> {
    fn on_data(&self, chunk: Vec<u8>) {
        let (Some(core), Some(connection)) = (self.core.upgrade(), self.connection.upgrade()) else {
            return;
        };
        let event_connection = Arc::clone(&connection);
        connection.enqueue_event(Box::pin(async move {
            core.receive(event_connection, chunk).await;
        }));
    }

    fn on_close(&self) {
        let (Some(core), Some(connection)) = (self.core.upgrade(), self.connection.upgrade()) else {
            return;
        };
        let event_connection = Arc::clone(&connection);
        connection.enqueue_event(Box::pin(async move {
            core.transport_closed(&event_connection).await;
        }));
    }

    fn on_error(&self, error: TransportError) {
        let (Some(core), Some(connection)) = (self.core.upgrade(), self.connection.upgrade()) else {
            return;
        };
        let event_connection = Arc::clone(&connection);
        connection.enqueue_event(Box::pin(async move {
            core.report_error(&error);
            let _ = core.close_connection(&event_connection, None).await;
            core.disconnect(&event_connection).await;
        }));
    }
}

struct ClosedHandler {
    on_error: Option<ServerErrorHandler>,
}

impl ConnectionHandler for ClosedHandler {
    fn on_data(&self, _chunk: Vec<u8>) {}
    fn on_close(&self) {}
    fn on_error(&self, error: TransportError) {
        if let Some(callback) = &self.on_error {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(&error)));
        }
    }
}

struct PendingUpdates {
    subscription: Option<String>,
    state: StdMutex<(bool, Vec<ServiceProviderUpdate<DeltaOp>>)>,
}

impl PendingUpdates {
    fn new(subscription: Option<String>) -> Self {
        Self {
            subscription,
            state: StdMutex::new((false, Vec::new())),
        }
    }

    fn buffer(&self, subscription_id: &str, update: ServiceProviderUpdate<DeltaOp>) -> bool {
        if self.subscription.as_deref() != Some(subscription_id) {
            return false;
        }
        let mut state = lock(&self.state);
        if state.0 {
            false
        } else {
            state.1.push(update);
            true
        }
    }

    fn activate(&self) -> Vec<ServiceProviderUpdate<DeltaOp>> {
        let mut state = lock(&self.state);
        state.0 = true;
        std::mem::take(&mut state.1)
    }
}

fn same_target(left: &RpcTarget, right: &RpcTarget) -> bool {
    match (left, right) {
        (RpcTarget::Server(left), RpcTarget::Server(right)) => left == right,
        (RpcTarget::Session(left), RpcTarget::Session(right)) => left == right,
        _ => false,
    }
}


trait RpcTargetServerId {
    fn server_id(&self) -> &ServerId;
}

impl RpcTargetServerId for RpcTarget {
    fn server_id(&self) -> &ServerId {
        match self {
            RpcTarget::Server(target) => &target.server_id,
            RpcTarget::Session(target) => &target.server_id,
        }
    }
}
