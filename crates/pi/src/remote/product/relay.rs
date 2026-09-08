//! Native Radius relay WebSocket transport.
//!
//! Radius exposes one authenticated WebSocket per logical server.  Host
//! connections multiplex server byte connections with a small JSON control
//! envelope, while client connections carry the released v8 byte transport
//! unchanged.  This module owns only the relay envelope; it never decodes or
//! re-encodes the protocol bytes that flow through the generic server/client
//! seams.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::remote::client::{
    AttachmentChangeListener, Client, ClientError, ConnectionState, ConnectionStateListener,
    Subscription,
};
use crate::remote::framing::DEFAULT_MAX_FRAME_LENGTH;
use crate::remote::schemas::{ServerId, is_server_id};
use crate::remote::server::{ByteConnection, ConnectionHandler, Server, ServerHost};
use crate::remote::transport::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, SendFuture, TransportError,
};
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::protocol::{CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::relay_auth::{
    RadiusRelayAuth, RadiusRelayAuthError, RadiusRelayAuthResolveOptions,
    RadiusRelayAuthResolver,
};

/// Subprotocol requested by a Radius relay host.
pub const RADIUS_RELAY_HOST_SUBPROTOCOL: &str = "pi-session-relay.host.v1";
/// Subprotocol requested by a Radius relay client.
pub const RADIUS_RELAY_CLIENT_SUBPROTOCOL: &str = "pi-session-relay.client.v1";

const RELAY_DATA_HEADER_BYTES: usize = 18;
const RELAY_DATA_FRAME_VERSION: u8 = 1;
const RELAY_DATA_FRAME_TYPE: u8 = 1;
const MAX_PENDING_BYTES: usize = DEFAULT_MAX_FRAME_LENGTH * 4;
const MAX_CONTROL_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_RELAY_DATA_MESSAGE_BYTES: usize = RELAY_DATA_HEADER_BYTES + DEFAULT_MAX_FRAME_LENGTH;
const HOST_RETRY_INITIAL: Duration = Duration::from_secs(1);
const HOST_RETRY_MAX: Duration = Duration::from_secs(30);
const MISSING_AUTH_RETRY: Duration = Duration::from_secs(30);
const CLIENT_RETRY_INITIAL: Duration = Duration::from_secs(1);
const CLIENT_RETRY_MAX: Duration = Duration::from_secs(30);
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const LOCAL_PROTOCOL_ERROR_CLOSE_CODE: u16 = 4000;
const LOCAL_TRANSPORT_ERROR_CLOSE_CODE: u16 = 4001;

/// A server accept seam used by [`RadiusRelayHost`].
///
/// The blanket implementation lets callers pass `Arc<Server<H>>` without
/// coupling this transport module to the product host's concrete metadata
/// type.
pub trait RelayServerAcceptor: Send + Sync {
    /// Accept one already-authorized byte connection.
    fn accept(&self, connection: Arc<dyn ByteConnection>) -> Arc<dyn ConnectionHandler>;
}

impl<H: ServerHost> RelayServerAcceptor for Server<H> {
    fn accept(&self, connection: Arc<dyn ByteConnection>) -> Arc<dyn ConnectionHandler> {
        Server::accept(self, connection)
    }
}

/// Status emitted by a long-lived host relay loop.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RadiusRelayHostStatus {
    /// No explicit or stored Radius credential is available.
    NotAuthenticated,
    /// A fresh authenticated WebSocket is being opened.
    Connecting,
    /// The authenticated relay is serving multiplexed connections.
    Connected,
    /// The relay dropped and will be retried.
    Retrying {
        /// Source-faithful error text for the failed attempt.
        error: String,
    },
}

/// Options for constructing a Radius host relay.
pub struct RadiusRelayHostOptions {
    /// Stable logical server identity used in the relay URL.
    pub server_id: ServerId,
    /// Generic native server accept seam.
    pub server: Arc<dyn RelayServerAcceptor>,
    /// Native explicit/stored credential resolver.
    pub auth: Arc<RadiusRelayAuthResolver>,
    /// Optional status observer.  Callback failures cannot affect transport.
    pub on_status: Option<Arc<dyn Fn(RadiusRelayHostStatus) + Send + Sync>>,
}

impl fmt::Debug for RadiusRelayHostOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RadiusRelayHostOptions")
            .field("server_id", &self.server_id)
            .field("server", &"[acceptor]")
            .field("auth", &"[resolver]")
            .field("on_status", &self.on_status.is_some())
            .finish()
    }
}

/// Maintain one experimental server's authenticated, multiplexed Radius host.
pub struct RadiusRelayHost {
    state: Arc<HostState>,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for RadiusRelayHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RadiusRelayHost")
            .field("server_id", &self.state.server_id)
            .field("closed", &self.state.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl RadiusRelayHost {
    /// Creates an unstarted host relay.
    #[must_use]
    pub fn new(options: RadiusRelayHostOptions) -> Self {
        Self {
            state: Arc::new(HostState {
                server_id: options.server_id,
                server: options.server,
                auth: options.auth,
                on_status: options.on_status,
                cancel: CancellationToken::new(),
                closed: AtomicBool::new(false),
                writer: StdMutex::new(None),
                connections: StdMutex::new(HashMap::new()),
            }),
            task: StdMutex::new(None),
        }
    }

    /// Starts the reconnecting host loop once.  Repeated calls are harmless.
    pub fn start(&self) {
        if self.state.closed.load(Ordering::Acquire) {
            return;
        }
        let mut task = lock_std(&self.task);
        if task.is_some() || self.state.closed.load(Ordering::Acquire) {
            return;
        }
        let state = Arc::clone(&self.state);
        *task = Some(tokio::spawn(async move {
            run_host_loop(state).await;
        }));
    }

    /// Cancels the host loop, closes the current socket, and releases every
    /// accepted relay connection before returning.
    pub async fn close(&self) {
        if !self.state.closed.swap(true, Ordering::AcqRel) {
            self.state.cancel.cancel();
        }
        let writer = lock_std(&self.state.writer).take();
        if let Some(writer) = writer {
            writer
                .close_with_code(1000, "Pi server stopped")
                .await;
        }
        self.state.drop_connections(None);
        let task = lock_std(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for RadiusRelayHost {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.cancel.cancel();
        if let Some(writer) = lock_std(&self.state.writer).take() {
            writer.close();
        }
        self.state.drop_connections(None);
        if let Some(task) = lock_std(&self.task).take() {
            task.abort();
        }
    }
}

/// Creates a fresh authenticated Radius client byte transport per attempt.
///
/// The returned factory deliberately performs auth resolution inside the
/// attempt, so an OAuth refresh or explicit token-file change is observed on
/// every reconnect.  The WebSocket's TLS verifier is the default
/// `tokio-tungstenite` rustls verifier; no insecure verifier is installed.
pub fn create_radius_client_transport_factory(
    server_id: ServerId,
    auth: Arc<RadiusRelayAuthResolver>,
) -> ByteTransportFactory {
    Arc::new(move |handlers| {
        let server_id = server_id.clone();
        let auth = Arc::clone(&auth);
        Box::pin(async move {
            let resolved = auth
                .resolve(RadiusRelayAuthResolveOptions {
                    required: true,
                    signal: None,
                })
                .await
                .map_err(auth_error_to_transport)?;
            let credentials = resolved.ok_or_else(|| {
                TransportError::Message("Radius authentication is required".to_owned())
            })?;
            let socket = open_radius_relay_websocket(
                &credentials,
                &server_id,
                RADIUS_RELAY_CLIENT_SUBPROTOCOL,
                None,
            )
            .await
            .map_err(relay_error_to_transport)?;
            Ok(RadiusClientByteTransport::new(socket, handlers)
                as Arc<dyn ByteTransport>)
        })
    })
}

/// Reconnect one established native client and restore its selected session.
pub struct RadiusClientReconnect {
    state: Arc<ReconnectState>,
    connection_subscription: StdMutex<Option<Subscription>>,
    attachment_subscription: StdMutex<Option<Subscription>>,
}

impl fmt::Debug for RadiusClientReconnect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RadiusClientReconnect")
            .field("disposed", &self.state.disposed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl RadiusClientReconnect {
    /// Installs reconnect/attachment listeners on `client`.
    pub fn new<F, Fut>(
        client: Arc<Client>,
        reattach: F,
    ) -> Result<Self, ClientError>
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ClientError>> + Send + 'static,
    {
        let reattach: ReattachCallback = Arc::new(move |session_id| {
            Box::pin(reattach(session_id))
        });
        let state = Arc::new(ReconnectState {
            client: Arc::clone(&client),
            reattach,
            desired_session: StdMutex::new(client.attachment().map(|target| target.session_id)),
            cancel: CancellationToken::new(),
            disposed: AtomicBool::new(false),
            reconnecting: AtomicBool::new(false),
            task: StdMutex::new(None),
        });

        let attachment_state = Arc::clone(&state);
        let attachment_listener: AttachmentChangeListener = Arc::new(move |attachment| {
            let mut desired = lock_std(&attachment_state.desired_session);
            if let Some(attachment) = attachment {
                *desired = Some(attachment.session_id.clone());
            } else if attachment_state.client.connected() {
                *desired = None;
            }
        });
        let attachment_subscription = client.on_attachment_change(attachment_listener)?;

        let connection_state = Arc::clone(&state);
        let connection_listener: ConnectionStateListener = Arc::new(move |change| {
            if change.state == ConnectionState::Disconnected
                && !connection_state.disposed.load(Ordering::Acquire)
            {
                start_reconnect(&connection_state);
            }
        });
        let connection_subscription = match client.on_connection_state_change(connection_listener) {
            Ok(subscription) => subscription,
            Err(error) => {
                drop(attachment_subscription);
                return Err(error);
            }
        };

        Ok(Self {
            state,
            connection_subscription: StdMutex::new(Some(connection_subscription)),
            attachment_subscription: StdMutex::new(Some(attachment_subscription)),
        })
    }

    /// Stops reconnect attempts, disconnects a live client, and joins the
    /// owned retry task.
    pub async fn dispose(&self) {
        if self.state.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.cancel.cancel();
        lock_std(&self.connection_subscription).take();
        lock_std(&self.attachment_subscription).take();
        if self.state.client.connection_state() != ConnectionState::Disconnected {
            self.state.client.disconnect("Radius reconnect stopped");
        }
        if let Some(task) = lock_std(&self.state.task).take() {
            let _ = task.await;
        }
    }
}

impl Drop for RadiusClientReconnect {
    fn drop(&mut self) {
        self.state.disposed.store(true, Ordering::Release);
        self.state.cancel.cancel();
        if let Some(task) = lock_std(&self.state.task).take() {
            task.abort();
        }
    }
}

struct HostState {
    server_id: ServerId,
    server: Arc<dyn RelayServerAcceptor>,
    auth: Arc<RadiusRelayAuthResolver>,
    on_status: Option<Arc<dyn Fn(RadiusRelayHostStatus) + Send + Sync>>,
    cancel: CancellationToken,
    closed: AtomicBool,
    writer: StdMutex<Option<Arc<OrderedWebSocketWriter>>>,
    connections: StdMutex<HashMap<String, ActiveRelayConnection>>,
}

struct ActiveRelayConnection {
    connection: Arc<RelayServerByteConnection>,
    handler: Arc<dyn ConnectionHandler>,
}

impl HostState {
    fn emit_status(&self, status: RadiusRelayHostStatus) {
        if let Some(on_status) = self.on_status.as_ref() {
            on_status(status);
        }
    }

    fn current_writer(&self) -> Option<Arc<OrderedWebSocketWriter>> {
        lock_std(&self.writer).clone()
    }

    fn set_writer(&self, writer: Arc<OrderedWebSocketWriter>) {
        *lock_std(&self.writer) = Some(writer);
    }

    fn clear_writer(&self, writer: &Arc<OrderedWebSocketWriter>) {
        let mut current = lock_std(&self.writer);
        if current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, writer))
        {
            *current = None;
        }
    }

    fn has_connection(&self, connection_id: &str) -> bool {
        lock_std(&self.connections).contains_key(connection_id)
    }

    fn drop_connections(&self, error: Option<TransportError>) {
        let connections = std::mem::take(&mut *lock_std(&self.connections));
        for active in connections.into_values() {
            active.connection.mark_closed();
            if let Some(error) = error.as_ref() {
                active.handler.on_error(error.clone());
            } else {
                active.handler.on_close();
            }
        }
    }
}

async fn run_host_loop(state: Arc<HostState>) {
    let mut retry = HOST_RETRY_INITIAL;
    while !state.closed.load(Ordering::Acquire) {
        let credentials = match state
            .auth
            .resolve(RadiusRelayAuthResolveOptions {
                required: false,
                signal: Some(state.cancel.clone()),
            })
            .await
        {
            Ok(Some(credentials)) => credentials,
            Ok(None) => {
                state.emit_status(RadiusRelayHostStatus::NotAuthenticated);
                if !delay_or_cancel(&state.cancel, MISSING_AUTH_RETRY).await {
                    break;
                }
                continue;
            }
            Err(RadiusRelayAuthError::Cancelled) if state.closed.load(Ordering::Acquire) => break,
            Err(error) => {
                state.emit_status(RadiusRelayHostStatus::Retrying {
                    error: error.to_string(),
                });
                if !delay_or_cancel(&state.cancel, retry).await {
                    break;
                }
                retry = retry.saturating_mul(2).min(HOST_RETRY_MAX);
                continue;
            }
        };

        state.emit_status(RadiusRelayHostStatus::Connecting);
        let socket = match open_radius_relay_websocket(
            &credentials,
            &state.server_id,
            RADIUS_RELAY_HOST_SUBPROTOCOL,
            Some(&state.cancel),
        )
        .await
        {
            Ok(socket) => socket,
            Err(RelayError::Cancelled) if state.closed.load(Ordering::Acquire) => break,
            Err(error) => {
                state.emit_status(RadiusRelayHostStatus::Retrying {
                    error: error.to_string(),
                });
                if !delay_or_cancel(&state.cancel, retry).await {
                    break;
                }
                retry = retry.saturating_mul(2).min(HOST_RETRY_MAX);
                continue;
            }
        };

        if state.closed.load(Ordering::Acquire) {
            break;
        }
        retry = HOST_RETRY_INITIAL;
        state.emit_status(RadiusRelayHostStatus::Connected);
        let result = serve_host_socket(Arc::clone(&state), socket).await;
        if state.closed.load(Ordering::Acquire) {
            break;
        }
        let error = match result {
            Ok(()) => RelayError::Message("Radius relay host disconnected".to_owned()),
            Err(error) => error,
        };
        state.emit_status(RadiusRelayHostStatus::Retrying {
            error: error.to_string(),
        });
        if !delay_or_cancel(&state.cancel, retry).await {
            break;
        }
        retry = retry.saturating_mul(2).min(HOST_RETRY_MAX);
    }
}

async fn serve_host_socket(
    state: Arc<HostState>,
    socket: RelayWebSocket,
) -> Result<(), RelayError> {
    let (sink, mut stream) = socket.split();
    let writer = Arc::new(OrderedWebSocketWriter::new(sink));
    state.set_writer(Arc::clone(&writer));

    let result = loop {
        let next = tokio::select! {
            biased;
            _ = state.cancel.cancelled() => break Ok(()),
            message = stream.next() => message,
        };
        let Some(message) = next else {
            break Err(RelayError::Message("Radius relay host closed (1006)".to_owned()));
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => break Err(RelayError::Message(format!("Radius relay WebSocket failed: {error}"))),
        };
        match message {
            Message::Text(text) => {
                if text.len() > MAX_CONTROL_MESSAGE_BYTES {
                    let error =
                        RelayError::Protocol("Radius relay control message is too large".to_owned());
                    writer
                        .close_with_code(LOCAL_PROTOCOL_ERROR_CLOSE_CODE, "Radius relay protocol error")
                        .await;
                    break Err(error);
                }
                if let Err(error) = handle_host_control(&state, &writer, text.as_ref()).await {
                    let close_code = if matches!(&error, RelayError::Transport(_)) {
                        LOCAL_TRANSPORT_ERROR_CLOSE_CODE
                    } else {
                        LOCAL_PROTOCOL_ERROR_CLOSE_CODE
                    };
                    writer
                        .close_with_code(close_code, "Radius relay error")
                        .await;
                    break Err(error);
                }
            }
            Message::Binary(data) => {
                if data.len() > MAX_RELAY_DATA_MESSAGE_BYTES {
                    let error = RelayError::Protocol("Invalid Radius relay data frame".to_owned());
                    writer
                        .close_with_code(LOCAL_PROTOCOL_ERROR_CLOSE_CODE, "Radius relay protocol error")
                        .await;
                    break Err(error);
                }
                let Some((connection_id, payload)) = parse_relay_data_frame(data.as_ref()) else {
                    let error = RelayError::Protocol("Invalid Radius relay data frame".to_owned());
                    writer
                        .close_with_code(LOCAL_PROTOCOL_ERROR_CLOSE_CODE, "Radius relay protocol error")
                        .await;
                    break Err(error);
                };
                let handler = lock_std(&state.connections)
                    .get(&connection_id)
                    .map(|active| Arc::clone(&active.handler));
                if let Some(handler) = handler {
                    handler.on_data(payload);
                } else if let Err(error) =
                    send_connection_close(&writer, &connection_id, Some(1000)).await
                {
                    break Err(error);
                }
            }
            Message::Close(Some(frame)) => {
                let code: u16 = frame.code.into();
                if code == 1000 {
                    break Ok(());
                }
                break Err(RelayError::Message(format!(
                    "Radius relay host closed ({code}{}{})",
                    if frame.reason.is_empty() { "" } else { ": " },
                    frame.reason
                )));
            }
            Message::Close(None) => {
                break Err(RelayError::Message("Radius relay host closed (1005)".to_owned()));
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    };

    state.clear_writer(&writer);
    writer.close();
    let connection_error = if state.closed.load(Ordering::Acquire) {
        None
    } else {
        result
            .as_ref()
            .err()
            .map(|error| TransportError::Message(error.to_string()))
    };
    state.drop_connections(connection_error);
    result
}

async fn handle_host_control(
    state: &Arc<HostState>,
    writer: &Arc<OrderedWebSocketWriter>,
    text: &str,
) -> Result<(), RelayError> {
    match parse_host_control_message(text)? {
        HostInputControlMessage::Ping => {
            send_control(writer, HostOutputControlMessage::Pong { version: 1 }).await
        }
        HostInputControlMessage::Pong => Ok(()),
        HostInputControlMessage::ConnectionOpen { connection_id } => {
            if state.has_connection(&connection_id) {
                return Err(RelayError::Protocol("Radius relay reused a connection ID".to_owned()));
            }
            let connection = Arc::new(RelayServerByteConnection {
                state: Arc::downgrade(state),
                connection_id: connection_id.clone(),
                closed: AtomicBool::new(false),
            });
            let handler = state
                .server
                .accept(Arc::clone(&connection) as Arc<dyn ByteConnection>);
            if connection.closed() {
                send_connection_close(writer, &connection_id, Some(1012)).await?;
            } else {
                lock_std(&state.connections).insert(
                    connection_id,
                    ActiveRelayConnection { connection, handler },
                );
            }
            Ok(())
        }
        HostInputControlMessage::ConnectionClose {
            connection_id,
            code: _,
        } => {
            let active = lock_std(&state.connections).remove(&connection_id);
            if let Some(active) = active {
                active.connection.mark_closed();
                active.handler.on_close();
            }
            Ok(())
        }
    }
}

async fn send_connection_close(
    writer: &Arc<OrderedWebSocketWriter>,
    connection_id: &str,
    code: Option<u16>,
) -> Result<(), RelayError> {
    send_control(
        writer,
        HostOutputControlMessage::ConnectionClose {
            version: 1,
            connection_id: connection_id.to_owned(),
            code,
        },
    )
    .await
}

async fn send_control(
    writer: &Arc<OrderedWebSocketWriter>,
    message: HostOutputControlMessage,
) -> Result<(), RelayError> {
    let text = serde_json::to_string(&message)
        .map_err(|error| RelayError::Message(format!("Radius relay control encode failed: {error}")))?;
    writer
        .send_text(text)
        .await
        .map_err(relay_error_from_transport)
}

struct RelayServerByteConnection {
    state: Weak<HostState>,
    connection_id: String,
    closed: AtomicBool,
}

impl RelayServerByteConnection {
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl ByteConnection for RelayServerByteConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed() {
            return Box::pin(async { Err(TransportError::Closed) });
        }
        let Some(state) = self.state.upgrade() else {
            return Box::pin(async { Err(TransportError::Closed) });
        };
        let connection_id = self.connection_id.clone();
        Box::pin(async move {
            if !state.has_connection(&connection_id) {
                return Err(TransportError::Closed);
            }
            let frame = encode_relay_data_frame(&connection_id, &chunk)
                .map_err(|error| TransportError::Message(error.to_string()))?;
            let Some(writer) = state.current_writer() else {
                return Err(TransportError::Closed);
            };
            writer.send_binary(frame).await
        })
    }

    fn close(&self, final_chunk: Option<Vec<u8>>) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Box::pin(async { Ok(()) });
        }
        let Some(state) = self.state.upgrade() else {
            return Box::pin(async { Ok(()) });
        };
        let connection_id = self.connection_id.clone();
        Box::pin(async move { close_server_connection(&state, &connection_id, final_chunk).await })
    }
}

async fn close_server_connection(
    state: &Arc<HostState>,
    connection_id: &str,
    final_chunk: Option<Vec<u8>>,
) -> Result<(), TransportError> {
    if lock_std(&state.connections).remove(connection_id).is_none() {
        return Ok(());
    }
    let Some(writer) = state.current_writer() else {
        return Err(TransportError::Closed);
    };
    if let Some(final_chunk) = final_chunk {
        let frame = encode_relay_data_frame(connection_id, &final_chunk)
            .map_err(|error| TransportError::Message(error.to_string()))?;
        writer.send_binary(frame).await?;
    }
    let text = serde_json::to_string(&HostOutputControlMessage::ConnectionClose {
        version: 1,
        connection_id: connection_id.to_owned(),
        code: Some(1000),
    })
    .map_err(|error| TransportError::Message(format!("Radius relay control encode failed: {error}")))?;
    writer.send_text(text).await
}

struct RadiusClientByteTransport {
    writer: Arc<OrderedWebSocketWriter>,
    closed: AtomicBool,
    cancel: CancellationToken,
    reader: StdMutex<Option<JoinHandle<()>>>,
    close_task: StdMutex<Option<JoinHandle<()>>>,
}

impl RadiusClientByteTransport {
    fn new(socket: RelayWebSocket, handlers: Arc<dyn ByteTransportHandlers>) -> Arc<Self> {
        let (sink, reader) = socket.split();
        let transport = Arc::new(Self {
            writer: Arc::new(OrderedWebSocketWriter::new(sink)),
            closed: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            reader: StdMutex::new(None),
            close_task: StdMutex::new(None),
        });
        let weak = Arc::downgrade(&transport);
        let task = tokio::spawn(async move {
            let Some(transport) = weak.upgrade() else {
                return;
            };
            transport.read_loop(reader, handlers).await;
        });
        *lock_std(&transport.reader) = Some(task);
        transport
    }

    async fn read_loop(
        &self,
        mut reader: futures::stream::SplitStream<RelayWebSocket>,
        handlers: Arc<dyn ByteTransportHandlers>,
    ) {
        loop {
            let next = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                message = reader.next() => message,
            };
            let Some(message) = next else {
                self.finish_close(&handlers);
                return;
            };
            match message {
                Ok(Message::Binary(data)) => {
                    if data.len() > MAX_RELAY_DATA_MESSAGE_BYTES {
                        self.finish_error(
                            &handlers,
                            TransportError::Message(
                                "Radius relay client received an oversized binary message".to_owned(),
                            ),
                        )
                        .await;
                        return;
                    }
                    if !self.closed.load(Ordering::Acquire) {
                        handlers.on_data(data.to_vec());
                    }
                }
                Ok(Message::Text(_)) => {
                    self.finish_error(
                        &handlers,
                        TransportError::Message(
                            "Radius relay client received a non-binary message".to_owned(),
                        ),
                    )
                    .await;
                    return;
                }
                Ok(Message::Close(_)) => {
                    self.finish_close(&handlers);
                    return;
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(error) => {
                    self.finish_error(
                        &handlers,
                        TransportError::Message(format!("Radius relay WebSocket failed: {error}")),
                    )
                    .await;
                    return;
                }
            }
        }
    }

    fn finish_close(&self, handlers: &Arc<dyn ByteTransportHandlers>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel();
        self.writer.close();
        handlers.on_close();
    }

    async fn finish_error(
        &self,
        handlers: &Arc<dyn ByteTransportHandlers>,
        error: TransportError,
    ) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel();
        self.writer
            .close_with_code(LOCAL_TRANSPORT_ERROR_CLOSE_CODE, "Radius relay transport error")
            .await;
        handlers.on_error(error);
    }
}

impl ByteTransport for RadiusClientByteTransport {
    fn send(&self, chunk: Vec<u8>) -> SendFuture {
        if self.closed.load(Ordering::Acquire) {
            return Box::pin(async { Err(TransportError::Closed) });
        }
        self.writer.send_binary(chunk)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel();
        if let Some(task) = lock_std(&self.reader).take() {
            task.abort();
        }
        let writer = Arc::clone(&self.writer);
        let close = async move {
            writer.close_with_code(1000, "Pi client closed").await;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            *lock_std(&self.close_task) = Some(handle.spawn(close));
        } else {
            self.writer.close();
        }
    }
}

impl Drop for RadiusClientByteTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.writer.close();
        if let Some(task) = lock_std(&self.reader).take() {
            task.abort();
        }
        if let Some(task) = lock_std(&self.close_task).take() {
            task.abort();
        }
    }
}

struct OrderedWebSocketWriter {
    sink: Mutex<Option<futures::stream::SplitSink<RelayWebSocket, Message>>>,
    pending_bytes: Arc<AtomicUsize>,
    closed: AtomicBool,
}

impl OrderedWebSocketWriter {
    fn new(sink: futures::stream::SplitSink<RelayWebSocket, Message>) -> Self {
        Self {
            sink: Mutex::new(Some(sink)),
            pending_bytes: Arc::new(AtomicUsize::new(0)),
            closed: AtomicBool::new(false),
        }
    }

    fn send_text(self: &Arc<Self>, text: String) -> BoxFuture<'static, Result<(), TransportError>> {
        let byte_length = text.len();
        self.send_message(Message::Text(text.into()), byte_length)
    }

    fn send_binary(self: &Arc<Self>, data: Vec<u8>) -> SendFuture {
        let byte_length = data.len();
        self.send_message(Message::Binary(data.into()), byte_length)
    }

    fn send_message(
        self: &Arc<Self>,
        message: Message,
        byte_length: usize,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed.load(Ordering::Acquire) {
            return Box::pin(async { Err(TransportError::Closed) });
        }
        if !reserve_pending(&self.pending_bytes, byte_length) {
            return Box::pin(async { Err(TransportError::PendingBytesExceeded) });
        }
        let writer = Arc::clone(self);
        let reservation = PendingBytesReservation {
            pending: Arc::clone(&self.pending_bytes),
            bytes: byte_length,
        };
        Box::pin(async move {
            let _reservation = reservation;
            let mut sink = writer.sink.lock().await;
            if writer.closed.load(Ordering::Acquire) {
                sink.take();
                return Err(TransportError::Closed);
            }
            let Some(sink) = sink.as_mut() else {
                return Err(TransportError::Closed);
            };
            sink.send(message)
                .await
                .map_err(|error| TransportError::Message(format!("Radius relay WebSocket write failed: {error}")))
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut sink) = self.sink.try_lock() {
            sink.take();
        }
    }

    async fn close_with_code(&self, code: u16, reason: &str) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let close = async {
            let mut sink = self.sink.lock().await;
            if let Some(sink) = sink.as_mut() {
                let frame = CloseFrame {
                    code: close_code(code),
                    reason: reason.to_owned().into(),
                };
                let _ = sink.send(Message::Close(Some(frame))).await;
                let _ = sink.close().await;
            }
            sink.take();
        };
        let _ = tokio::time::timeout(CLOSE_FLUSH_TIMEOUT, close).await;
        if let Ok(mut sink) = self.sink.try_lock() {
            sink.take();
        }
    }
}

struct PendingBytesReservation {
    pending: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for PendingBytesReservation {
    fn drop(&mut self) {
        self.pending.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

fn reserve_pending(pending: &AtomicUsize, bytes: usize) -> bool {
    if bytes > MAX_PENDING_BYTES {
        return false;
    }
    let mut current = pending.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(bytes) else {
            return false;
        };
        if next > MAX_PENDING_BYTES {
            return false;
        }
        match pending.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug, Error)]
enum RelayError {
    #[error("{0}")]
    Message(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{0}")]
    Transport(TransportError),
    #[error("Radius relay connection cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RelayProtocolError {
    /// Connection ID is not a lowercase UUIDv4.
    #[error("Invalid Radius relay connection ID")]
    InvalidConnectionId,
    /// Data exceeds the released remote frame budget.
    #[error("Radius relay payload exceeds the maximum frame length")]
    PayloadTooLarge,
}

#[derive(Debug)]
enum HostInputControlMessage {
    Ping,
    Pong,
    ConnectionOpen { connection_id: String },
    ConnectionClose { connection_id: String, code: Option<u16> },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostOutputControlMessage {
    Pong {
        version: u8,
    },
    ConnectionClose {
        version: u8,
        #[serde(rename = "connection_id")]
        connection_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<u16>,
    },
}

fn parse_host_control_message(text: &str) -> Result<HostInputControlMessage, RelayError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|_| RelayError::Protocol("Invalid Radius relay control message".to_owned()))?;
    let Some(object) = value.as_object() else {
        return Err(RelayError::Protocol(
            "Invalid Radius relay control message".to_owned(),
        ));
    };
    let version_is_supported = object
        .get("version")
        .and_then(json_integer)
        .is_some_and(|version| version == 1);
    if !version_is_supported {
        return Err(RelayError::Protocol(
            "Unsupported Radius relay control version".to_owned(),
        ));
    }
    let Some(message_type) = object.get("type").and_then(Value::as_str) else {
        return Err(RelayError::Protocol(
            "Invalid Radius relay control message".to_owned(),
        ));
    };
    if message_type == "ping" {
        return Ok(HostInputControlMessage::Ping);
    }
    if message_type == "pong" {
        return Ok(HostInputControlMessage::Pong);
    }
    let valid_type = matches!(message_type, "connection_open" | "connection_close");
    let Some(connection_id) = object.get("connection_id").and_then(Value::as_str) else {
        return Err(RelayError::Protocol(
            "Invalid Radius relay control message".to_owned(),
        ));
    };
    if !valid_type || !is_server_id(connection_id) {
        return Err(RelayError::Protocol(
            "Invalid Radius relay control message".to_owned(),
        ));
    }
    let code = match object.get("code") {
        None => None,
        Some(value) => {
            let Some(code) = json_integer(value).and_then(|code| u16::try_from(code).ok()) else {
                return Err(RelayError::Protocol(
                    "Invalid Radius relay control message".to_owned(),
                ));
            };
            if !(1000..=4999).contains(&code) {
                return Err(RelayError::Protocol(
                    "Invalid Radius relay control message".to_owned(),
                ));
            }
            Some(code)
        }
    };
    if message_type == "connection_open" {
        Ok(HostInputControlMessage::ConnectionOpen {
            connection_id: connection_id.to_owned(),
        })
    } else {
        Ok(HostInputControlMessage::ConnectionClose {
            connection_id: connection_id.to_owned(),
            code,
        })
    }
}

/// Encodes one Radius multiplexing data frame.
///
/// The returned bytes contain only the relay envelope; the payload is passed
/// through unchanged and is not interpreted as CBOR here.
pub fn encode_relay_data_frame(
    connection_id: &str,
    payload: &[u8],
) -> Result<Vec<u8>, RelayProtocolError> {
    if !is_server_id(connection_id) {
        return Err(RelayProtocolError::InvalidConnectionId);
    }
    if payload.len() > DEFAULT_MAX_FRAME_LENGTH {
        return Err(RelayProtocolError::PayloadTooLarge);
    }
    let mut frame = Vec::with_capacity(RELAY_DATA_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&[RELAY_DATA_FRAME_VERSION, RELAY_DATA_FRAME_TYPE]);
    let mut index = 0;
    let mut hex = [0_u8; 32];
    for byte in connection_id.bytes().filter(|byte| *byte != b'-') {
        hex[index] = byte;
        index += 1;
    }
    for pair in hex.chunks_exact(2) {
        let high = hex_value(pair[0]).ok_or(RelayProtocolError::InvalidConnectionId)?;
        let low = hex_value(pair[1]).ok_or(RelayProtocolError::InvalidConnectionId)?;
        frame.push((high << 4) | low);
    }
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Parses one Radius multiplexing data frame.
#[must_use]
pub fn parse_relay_data_frame(frame: &[u8]) -> Option<(String, Vec<u8>)> {
    if frame.len() < RELAY_DATA_HEADER_BYTES || frame.len() > MAX_RELAY_DATA_MESSAGE_BYTES {
        return None;
    }
    if frame[0] != RELAY_DATA_FRAME_VERSION || frame[1] != RELAY_DATA_FRAME_TYPE {
        return None;
    }
    let mut hex = String::with_capacity(32);
    for byte in &frame[2..RELAY_DATA_HEADER_BYTES] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    let connection_id = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    if !is_server_id(&connection_id) {
        return None;
    }
    Some((connection_id, frame[RELAY_DATA_HEADER_BYTES..].to_vec()))
}

fn json_integer(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let value = value.as_f64()?;
    (value.is_finite() && value >= 0.0 && value.fract() == 0.0).then_some(value as u64)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn relay_websocket_url(gateway: &str, server_id: &ServerId) -> Result<String, RelayError> {
    let mut url = Url::parse(gateway)
        .map_err(|error| RelayError::Message(format!("Invalid Radius gateway URL: {error}")))?;
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(RelayError::Message("Invalid Radius gateway URL".to_owned()));
    }
    match url.scheme() {
        "https" => {
            url.set_scheme("wss")
                .map_err(|()| RelayError::Message("Invalid Radius gateway URL".to_owned()))?;
        }
        "http" => {
            url.set_scheme("ws")
                .map_err(|()| RelayError::Message("Invalid Radius gateway URL".to_owned()))?;
        }
        scheme => {
            return Err(RelayError::Message(format!(
                "Unsupported Radius gateway protocol: {scheme}"
            )));
        }
    }
    url.set_path(&format!("/v1/session-relays/{}/connect", server_id.as_str()));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

async fn open_radius_relay_websocket(
    auth: &RadiusRelayAuth,
    server_id: &ServerId,
    protocol: &str,
    signal: Option<&CancellationToken>,
) -> Result<RelayWebSocket, RelayError> {
    if auth.token.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RelayError::Message(
            "Invalid Radius authentication token".to_owned(),
        ));
    }
    let url = relay_websocket_url(&auth.gateway, server_id)?;
    let request = Request::builder()
        .uri(url.as_str())
        .header(AUTHORIZATION, format!("Bearer {}", auth.token))
        .header(SEC_WEBSOCKET_PROTOCOL, protocol)
        .body(())
        .map_err(|error| RelayError::Message(format!("Radius relay request failed: {error}")))?;
    let connect = connect_async(request);
    let (socket, response) = wait_connect(connect, signal).await?;
    let selected = response
        .headers()
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());
    if selected != Some(protocol) {
        return Err(RelayError::Message(format!(
            "Radius relay selected unexpected WebSocket protocol {:?}",
            selected.unwrap_or_default()
        )));
    }
    Ok(socket)
}

async fn wait_connect(
    connect: impl Future<Output = Result<(RelayWebSocket, tungstenite::handshake::client::Response), tungstenite::Error>>,
    signal: Option<&CancellationToken>,
) -> Result<(RelayWebSocket, tungstenite::handshake::client::Response), RelayError> {
    let result = if let Some(signal) = signal {
        tokio::select! {
            biased;
            _ = signal.cancelled() => return Err(RelayError::Cancelled),
            result = connect => result,
        }
    } else {
        connect.await
    };
    result.map_err(|error| RelayError::Message(format!("Radius relay WebSocket connection failed: {error}")))
}

fn close_code(code: u16) -> CloseCode {
    match code {
        1000 => CloseCode::Normal,
        1001 => CloseCode::Away,
        1002 => CloseCode::Protocol,
        1003 => CloseCode::Unsupported,
        1007 => CloseCode::Invalid,
        1008 => CloseCode::Policy,
        1009 => CloseCode::Size,
        1010 => CloseCode::Extension,
        1011 => CloseCode::Error,
        1012 => CloseCode::Restart,
        1013 => CloseCode::Again,
        custom => CloseCode::Library(custom),
    }
}

fn delay_or_cancel(cancel: &CancellationToken, duration: Duration) -> impl Future<Output = bool> + Send + 'static {
    let cancel = cancel.clone();
    async move {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            () = tokio::time::sleep(duration) => true,
        }
    }
}

type RelayWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ReattachCallback = Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), ClientError>> + Send + Sync>;

struct ReconnectState {
    client: Arc<Client>,
    reattach: ReattachCallback,
    desired_session: StdMutex<Option<String>>,
    cancel: CancellationToken,
    disposed: AtomicBool,
    reconnecting: AtomicBool,
    task: StdMutex<Option<JoinHandle<()>>>,
}

fn start_reconnect(state: &Arc<ReconnectState>) {
    if state
        .reconnecting
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let task_state = Arc::clone(state);
    let task = tokio::spawn(async move {
        run_reconnect(task_state.clone()).await;
        task_state.reconnecting.store(false, Ordering::Release);
    });
    *lock_std(&state.task) = Some(task);
}

async fn run_reconnect(state: Arc<ReconnectState>) {
    let mut retry = CLIENT_RETRY_INITIAL;
    while !state.disposed.load(Ordering::Acquire) && !state.client.connected() {
        if !delay_or_cancel(&state.cancel, retry).await {
            return;
        }
        match state.client.reconnect().await {
            Ok(_) => {
                let session_id = lock_std(&state.desired_session).clone();
                if let Some(session_id) = session_id {
                    if let Err(error) = (state.reattach)(session_id).await {
                        if state.client.connected() {
                            state.client.disconnect(error.to_string());
                        }
                        retry = retry.saturating_mul(2).min(CLIENT_RETRY_MAX);
                        continue;
                    }
                }
                return;
            }
            Err(error) => {
                if state.disposed.load(Ordering::Acquire) || state.cancel.is_cancelled() {
                    return;
                }
                if state.client.connected() {
                    state.client.disconnect(error.to_string());
                }
                retry = retry.saturating_mul(2).min(CLIENT_RETRY_MAX);
            }
        }
    }
}

fn auth_error_to_transport(error: RadiusRelayAuthError) -> TransportError {
    TransportError::Message(error.to_string())
}

fn relay_error_to_transport(error: RelayError) -> TransportError {
    match error {
        RelayError::Transport(error) => error,
        other => TransportError::Message(other.to_string()),
    }
}

fn relay_error_from_transport(error: TransportError) -> RelayError {
    RelayError::Transport(error)
}

fn lock_std<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
