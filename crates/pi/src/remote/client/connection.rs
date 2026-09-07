use std::sync::{Arc, Weak};

use tokio::sync::{mpsc, oneshot, Notify};

use super::{ClientCore, ClientError, ConnectionState, lock};
use crate::remote::codec::{ServerMessageDecoder, encode_client_message};
use crate::remote::framing::FrameDecoderOptions;
use crate::remote::schemas::{ClientMessage, PROTOCOL_VERSION, ServerHello, ServerMessage};
use crate::remote::transport::{ByteTransport, ByteTransportFactory, ByteTransportHandlers, TransportError};

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
    sender: Option<mpsc::UnboundedSender<Vec<u8>>>,
    hello_sent: bool,
    decoder: ServerMessageDecoder,
    handshake: Option<oneshot::Sender<Result<ServerHello, ClientError>>>,
}

pub(super) struct Connection {
    pub id: u64,
    options: ConnectionOptions,
    core: Weak<ClientCore>,
    shutdown: Arc<Notify>,
    state: std::sync::Mutex<State>,
}

impl Connection {
    pub fn new(id: u64, options: ConnectionOptions, core: Weak<ClientCore>) -> Result<(Arc<Self>, oneshot::Receiver<Result<ServerHello, ClientError>>), ClientError> {
        let decoder = ServerMessageDecoder::new(Some(FrameDecoderOptions { max_frame_length: options.max_frame_length }))?;
        let (tx, rx) = oneshot::channel();
        Ok((Arc::new(Self { id, options, core, shutdown: Arc::new(Notify::new()), state: std::sync::Mutex::new(State {
            lifecycle: ConnectionLifecycle::Connecting,
            transport: None,
            sender: None,
            hello_sent: false,
            decoder,
            handshake: Some(tx),
        }) }), rx))
    }

    pub fn state(&self) -> ConnectionState {
        match lock(&self.state).lifecycle {
            ConnectionLifecycle::Connecting => ConnectionState::Connecting,
            ConnectionLifecycle::Connected => ConnectionState::Connected,
            ConnectionLifecycle::Disconnected => ConnectionState::Disconnected,
        }
    }

    pub fn fail(&self, error: ClientError) {
        let (transport, handshake, sender) = {
            let mut state = lock(&self.state);
            if matches!(state.lifecycle, ConnectionLifecycle::Disconnected) { return; }
            state.lifecycle = ConnectionLifecycle::Disconnected;
            (state.transport.take(), state.handshake.take(), state.sender.take())
        };
        drop(sender);
        if let Some(core) = self.core.upgrade() { core.on_disconnected(self.id, error.clone()); }
        if let Some(handshake) = handshake { let _ = handshake.send(Err(error)); }
        if let Some(transport) = transport { transport.close(); }
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
        match sender.send(frame) {
            Ok(()) => Ok(()),
            Err(_) => {
                let error = ClientError::disconnected("Transport writer closed");
                self.fail(error.clone());
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
            if matches!(state.lifecycle, ConnectionLifecycle::Disconnected) { return; }
            if matches!(state.lifecycle, ConnectionLifecycle::Connecting) && !state.hello_sent {
                Err(ClientError::protocol("Received server data before client hello"))
            } else if state.transport.is_none() {
                Err(ClientError::protocol("Received server data before client hello"))
            } else { state.decoder.push(chunk).map_err(ClientError::from) }
        };
        let messages = match messages { Ok(messages) => messages, Err(error) => { self.fail(error); return; } };
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
                            if error.code.is_empty() {
                                self.fail(ClientError::protocol("Handshake error has an empty code"));
                            } else {
                                self.fail(error.into());
                            }
                            return;
                        }
                        _ => {
                            self.fail(ClientError::protocol("Expected matching v8 server hello"));
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
                    ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => { self.fail(ClientError::protocol("Unexpected handshake message")); return; }
                    message => if let Some(core) = self.core.upgrade() { core.on_message(self.id, message); },
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
            let error = decoder_result
                .err()
                .map_or_else(|| ClientError::disconnected("Byte transport closed"), ClientError::from);
            connection.fail(error);
        }
    }

    fn on_error(&self, error: TransportError) {
        if let Some(connection) = self.current() {
            connection.fail(error.into());
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
            connection.fail(error.into());
            return;
        }
    };
    let (sender, mut receiver) = mpsc::unbounded_channel();
    {
        let mut state = lock(&connection.state);
        if !matches!(state.lifecycle, ConnectionLifecycle::Connecting) {
            drop(state);
            transport.close();
            return;
        }
        state.transport = Some(transport.clone());
        state.sender = Some(sender.clone());
    }
    let writer_connection = Arc::downgrade(&connection);
    let shutdown = Arc::clone(&connection.shutdown);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    transport.close();
                    break;
                }
                frame = receiver.recv() => {
                    let Some(frame) = frame else {
                        transport.close();
                        break;
                    };
                    let Some(connection) = writer_connection.upgrade() else {
                        transport.close();
                        break;
                    };
                    if connection.state() == ConnectionState::Disconnected {
                        transport.close();
                        break;
                    }
                    if let Err(error) = transport.send(frame).await {
                        connection.fail(error.into());
                        break;
                    }
                }
            }
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
            connection.fail(error.into());
            return;
        }
    };
    let send_result = {
        let mut state = lock(&connection.state);
        if matches!(state.lifecycle, ConnectionLifecycle::Connecting) {
            state.hello_sent = true;
        }
        sender.send(frame)
    };
    if send_result.is_err() {
        connection.fail(ClientError::disconnected("Transport writer closed"));
    }
}
