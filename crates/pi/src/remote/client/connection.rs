use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use tokio::sync::{Notify, mpsc, oneshot};

use super::{ClientCore, ClientError, ConnectionState, lock};
use crate::remote::codec::{ServerMessageDecoder, encode_client_message};
use crate::remote::framing::FrameDecoderOptions;
use crate::remote::schemas::{ClientMessage, PROTOCOL_VERSION, ServerHello, ServerMessage};
use crate::remote::transport::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, TransportError,
};

/// Maximum frames queued for the transport writer while it drains.
const MAX_QUEUED_WRITER_FRAMES: usize = 256;

pub(super) struct ConnectionOptions {
    pub factory: ByteTransportFactory,
    pub server_id: String,
    pub max_frame_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectionLifecycle {
    Connecting,
    Connected,
    Disconnected,
}

struct State {
    lifecycle: ConnectionLifecycle,
    transport: Option<Arc<dyn ByteTransport>>,
    sender: Option<WriterQueue>,
    hello_sent: bool,
    decoder: ServerMessageDecoder,
    handshake: Option<oneshot::Sender<Result<ServerHello, ClientError>>>,
}

/// Bounded outbound queue in front of the single transport writer task.
///
/// Each frame reserves its bytes against a queue budget before it is
/// enqueued, and the reservation is released once the writer dequeues the
/// frame. A caller whose frame does not fit is rejected immediately instead
/// of accumulating encoded frames while the transport is stalled.
#[derive(Clone)]
struct WriterQueue {
    tx: mpsc::Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
}

/// One rejected frame admission.
enum Admission {
    /// The frame did not fit the queue bound; the connection stays usable.
    Full,
    /// The writer is gone; the connection must fail.
    Closed(ClientError),
}

impl WriterQueue {
    fn admit(&self, frame: Vec<u8>, max_queued_bytes: usize) -> Result<(), Admission> {
        let length = frame.len();
        if !reserve_queued_bytes(&self.queued, length, max_queued_bytes) {
            return Err(Admission::Full);
        }
        if self.tx.try_send(frame).is_err() {
            self.queued.fetch_sub(length, Ordering::AcqRel);
            return Err(if self.tx.is_closed() {
                Admission::Closed(ClientError::disconnected("Transport writer closed"))
            } else {
                Admission::Full
            });
        }
        Ok(())
    }
}

/// Reserves `bytes` against `max`, returning whether the reserve fit.
///
/// The caller releases the reservation once the bytes leave the queue.
fn reserve_queued_bytes(queued: &AtomicUsize, bytes: usize, max: usize) -> bool {
    if bytes > max {
        return false;
    }
    let mut current = queued.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(bytes) else {
            return false;
        };
        if next > max {
            return false;
        }
        match queued.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}
type HandshakeReceiver = oneshot::Receiver<Result<ServerHello, ClientError>>;

pub(super) struct Connection {
    pub id: u64,
    options: ConnectionOptions,
    core: Weak<ClientCore>,
    shutdown: Arc<Notify>,
    state: std::sync::Mutex<State>,
}

impl Connection {
    pub fn new(
        id: u64,
        options: ConnectionOptions,
        core: Weak<ClientCore>,
    ) -> Result<(Arc<Self>, HandshakeReceiver), ClientError> {
        let decoder = ServerMessageDecoder::new(Some(FrameDecoderOptions {
            max_frame_length: options.max_frame_length,
        }))?;
        let (tx, rx) = oneshot::channel();
        Ok((
            Arc::new(Self {
                id,
                options,
                core,
                shutdown: Arc::new(Notify::new()),
                state: std::sync::Mutex::new(State {
                    lifecycle: ConnectionLifecycle::Connecting,
                    transport: None,
                    sender: None,
                    hello_sent: false,
                    decoder,
                    handshake: Some(tx),
                }),
            }),
            rx,
        ))
    }

    pub fn state(&self) -> ConnectionState {
        match lock(&self.state).lifecycle {
            ConnectionLifecycle::Connecting => ConnectionState::Connecting,
            ConnectionLifecycle::Connected => ConnectionState::Connected,
            ConnectionLifecycle::Disconnected => ConnectionState::Disconnected,
        }
    }

    pub fn fail(&self, error: &ClientError) {
        let (transport, handshake, sender) = {
            let mut state = lock(&self.state);
            if matches!(state.lifecycle, ConnectionLifecycle::Disconnected) {
                return;
            }
            state.lifecycle = ConnectionLifecycle::Disconnected;
            (
                state.transport.take(),
                state.handshake.take(),
                state.sender.take(),
            )
        };
        drop(sender);
        if let Some(core) = self.core.upgrade() {
            core.on_disconnected(self.id, error);
        }
        if let Some(handshake) = handshake {
            let _ = handshake.send(Err(error.clone()));
        }
        if let Some(transport) = transport {
            transport.close();
        }
    }

    pub fn send(&self, message: &ClientMessage) -> Result<(), ClientError> {
        let frame = encode_client_message(
            message,
            Some(FrameDecoderOptions {
                max_frame_length: self.options.max_frame_length,
            }),
        )?;
        let sender = {
            let state = lock(&self.state);
            if !matches!(state.lifecycle, ConnectionLifecycle::Connected) {
                return Err(ClientError::disconnected("Client is not connected"));
            }
            state
                .sender
                .clone()
                .ok_or_else(|| ClientError::disconnected("Transport writer unavailable"))?
        };
        // The frame's bytes are reserved against the queue bound before it is
        // enqueued, so a stalled transport sheds new frames instead of
        // accumulating them without limit.
        match sender.admit(frame, self.options.max_frame_length) {
            Ok(()) => Ok(()),
            Err(Admission::Full) => {
                Err(ClientError::disconnected("Transport writer queue is full"))
            }
            Err(Admission::Closed(error)) => {
                self.fail(&error);
                Err(error)
            }
        }
    }

    fn data(&self, chunk: &[u8]) {
        let Some(core) = self.core.upgrade() else {
            return;
        };
        if !core.is_current(self.id) {
            return;
        }
        let messages = {
            let mut state = lock(&self.state);
            if matches!(state.lifecycle, ConnectionLifecycle::Disconnected) {
                return;
            }
            let handshake_pending =
                matches!(state.lifecycle, ConnectionLifecycle::Connecting) && !state.hello_sent;
            if handshake_pending || state.transport.is_none() {
                Err(ClientError::protocol(
                    "Received server data before client hello",
                ))
            } else {
                state.decoder.push(chunk).map_err(ClientError::from)
            }
        };
        let messages = match messages {
            Ok(messages) => messages,
            Err(error) => {
                self.fail(&error);
                return;
            }
        };
        for message in messages {
            if !core.is_current(self.id) {
                return;
            }
            match self.state() {
                ConnectionState::Disconnected => return,
                ConnectionState::Connecting => {
                    let hello = match message {
                        ServerMessage::Hello { version, server_id }
                            if version == PROTOCOL_VERSION
                                && server_id.as_str() == self.options.server_id.as_str() =>
                        {
                            ServerHello { version, server_id }
                        }
                        ServerMessage::HelloError { error } => {
                            let error = if error.code.is_empty() {
                                ClientError::protocol("Handshake error has an empty code")
                            } else {
                                error.into()
                            };
                            self.fail(&error);
                            return;
                        }
                        _ => {
                            self.fail(&ClientError::protocol("Expected matching v8 server hello"));
                            return;
                        }
                    };
                    {
                        let mut state = lock(&self.state);
                        if !matches!(state.lifecycle, ConnectionLifecycle::Connecting) {
                            return;
                        }
                        state.lifecycle = ConnectionLifecycle::Connected;
                    }
                    if let Some(core) = self.core.upgrade() {
                        core.on_handshake(self.id, hello.clone());
                    }
                    let handshake = {
                        let mut state = lock(&self.state);
                        state.handshake.take()
                    };
                    if let Some(handshake) = handshake {
                        let _ = handshake.send(Ok(hello));
                    }
                }
                ConnectionState::Connected => match message {
                    ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
                        self.fail(&ClientError::protocol("Unexpected handshake message"));
                        return;
                    }
                    message => {
                        if let Some(core) = self.core.upgrade() {
                            core.on_message(self.id, message);
                        }
                    }
                },
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.shutdown.notify_one();
    }
}

pub(super) struct ConnHandlers {
    id: u64,
    connection: Weak<Connection>,
}

impl ConnHandlers {
    /// Upgrades only when the callback still belongs to its own live
    /// connection attempt. Delayed callbacks from stale transports are
    /// dropped before they can touch the decoder or lifecycle.
    fn current(&self) -> Option<Arc<Connection>> {
        let connection = self.connection.upgrade()?;
        if connection.id != self.id || matches!(connection.state(), ConnectionState::Disconnected) {
            return None;
        }
        let core = connection.core.upgrade()?;
        if !core.is_current(self.id) {
            return None;
        }
        Some(connection)
    }
}

impl ByteTransportHandlers for ConnHandlers {
    fn on_data(&self, chunk: Vec<u8>) {
        if let Some(connection) = self.current() {
            connection.data(&chunk);
        }
    }

    fn on_close(&self) {
        if let Some(connection) = self.current() {
            let decoder_result = {
                let mut state = lock(&connection.state);
                state.decoder.end()
            };
            let error = decoder_result.err().map_or_else(
                || ClientError::disconnected("Byte transport closed"),
                ClientError::from,
            );
            connection.fail(&error);
        }
    }

    fn on_error(&self, error: TransportError) {
        if let Some(connection) = self.current() {
            connection.fail(&error.into());
        }
    }
}

pub(super) async fn open_transport(connection: Arc<Connection>) {
    let handlers = Arc::new(ConnHandlers {
        id: connection.id,
        connection: Arc::downgrade(&connection),
    });
    let transport = match (connection.options.factory)(handlers).await {
        Ok(transport) => transport,
        Err(error) => {
            connection.fail(&error.into());
            return;
        }
    };
    let queued = Arc::new(AtomicUsize::new(0));
    let (sender, mut receiver) = mpsc::channel(MAX_QUEUED_WRITER_FRAMES);
    let writer_queue = WriterQueue {
        tx: sender,
        queued: Arc::clone(&queued),
    };
    {
        let mut state = lock(&connection.state);
        if !matches!(state.lifecycle, ConnectionLifecycle::Connecting) {
            drop(state);
            transport.close();
            return;
        }
        state.transport = Some(transport.clone());
        state.sender = Some(writer_queue.clone());
    }
    let writer_connection = Arc::downgrade(&connection);
    let shutdown = Arc::clone(&connection.shutdown);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown.notified() => {
                    transport.close();
                    break;
                }
                frame = receiver.recv() => {
                    let Some(frame) = frame else {
                        transport.close();
                        break;
                    };
                    queued.fetch_sub(frame.len(), Ordering::AcqRel);
                    let Some(connection) = writer_connection.upgrade() else {
                        transport.close();
                        break;
                    };
                    if connection.state() == ConnectionState::Disconnected {
                        transport.close();
                        break;
                    }
                    if let Err(error) = transport.send(frame).await {
                        connection.fail(&error.into());
                        break;
                    }
                }
            }
        }
        // Frames still queued when the writer exits are never sent; release
        // their reservations so the bound holds across reconnect attempts.
        while let Ok(frame) = receiver.try_recv() {
            queued.fetch_sub(frame.len(), Ordering::AcqRel);
        }
    });
    let frame = match encode_client_message(
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        Some(FrameDecoderOptions {
            max_frame_length: connection.options.max_frame_length,
        }),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            connection.fail(&error.into());
            return;
        }
    };
    let hello_admitted = {
        let mut state = lock(&connection.state);
        if matches!(state.lifecycle, ConnectionLifecycle::Connecting) {
            state.hello_sent = true;
        }
        writer_queue.admit(frame, connection.options.max_frame_length)
    };
    if let Err(Admission::Closed(error)) = hello_admitted {
        connection.fail(&error);
    }
}
