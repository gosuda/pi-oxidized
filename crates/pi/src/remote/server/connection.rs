//! Transport-neutral server connection and listener seams.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError};

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::{oneshot, watch};

use crate::remote::transport::{
    ByteTransport, ByteTransportHandlers, EndpointSpec, EndpointSpecError, InMemoryListener,
    TransportError, build_transport,
};

fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An established, authorized ordered byte connection.
pub trait ByteConnection: Send + Sync + 'static {
    /// Whether the connection is terminal.
    fn closed(&self) -> bool;
    /// Sends one byte chunk in invocation order.
    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>>;
    /// Closes the connection, optionally delivering one final chunk first.
    fn close(&self, final_chunk: Option<Vec<u8>>)
        -> BoxFuture<'static, Result<(), TransportError>>;
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

/// A listener that supplies established byte connections.
pub trait ServerListener: Send + Sync + 'static {
    /// Human-readable address, when the transport has one.
    fn address(&self) -> Option<String>;
    /// Starts listening and passes authorized connections to `accept`.
    fn start(&self, accept: ConnectionAcceptor) -> BoxFuture<'static, Result<(), ListenerError>>;
    /// Stops listening and releases transport resources.
    fn close(&self) -> BoxFuture<'static, ()>;
}

/// Listener lifecycle failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListenerError {
    /// `start` was called twice.
    AlreadyStarted,
    /// The listener is closing or closed.
    Closing,
    /// The listener failed to bind or accept.
    Io(String),
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("listener is already started"),
            Self::Closing => formatter.write_str("listener is closing or closed"),
            Self::Io(message) => write!(formatter, "listener failure: {message}"),
        }
    }
}

impl std::error::Error for ListenerError {}

/// Declares where a server listens.  Platform gating is performed by
/// [`build_listener`], not by the enum itself.
#[derive(Clone, Debug)]
pub enum ListenSpec {
    /// Accept connections from an in-process listener.
    InMemory {
        /// Listener supplying accepted ends.
        listener: Arc<InMemoryListener>,
    },
    /// Listen on a Unix-domain socket.
    Unix {
        /// Socket path.
        path: PathBuf,
        /// Outbound pending-byte budget per connection.
        max_pending_bytes: Option<usize>,
    },
}

/// Builds one listener from a portable listen specification.
///
/// Unix path and budget validation is delegated to the shared endpoint-spec
/// owner, so client and server adapters cannot drift.
pub fn build_listener(
    spec: &ListenSpec,
) -> Result<Arc<dyn ServerListener>, EndpointSpecError> {
    match spec {
        ListenSpec::InMemory { listener } => {
            Ok(Arc::new(InMemoryServerListener::new(Arc::clone(listener))))
        }
        ListenSpec::Unix {
            path,
            max_pending_bytes,
        } => {
            build_transport(&EndpointSpec::Unix {
                path: path.clone(),
                max_pending_bytes: *max_pending_bytes,
            })?;
            #[cfg(unix)]
            {
                Ok(super::unix::create_listener(super::unix::UnixListenerOptions {
                    path: path.clone(),
                    mode: None,
                    max_pending_bytes: *max_pending_bytes,
                    max_frame_length: None,
                    graceful_close_timeout_ms: None,
                    on_error: None,
                }))
            }
            #[cfg(not(unix))]
            {
                Err(EndpointSpecError::UnsupportedOnPlatform {
                    kind: crate::remote::transport::EndpointKind::Unix,
                    os: std::env::consts::OS,
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InMemoryListenerState {
    Idle,
    Started,
    Closing,
}

/// Adapts the portable in-memory byte transport to [`ServerListener`].
pub struct InMemoryServerListener {
    listener: Arc<InMemoryListener>,
    state: StdMutex<InMemoryListenerState>,
    stop: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for InMemoryServerListener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InMemoryServerListener")
            .finish_non_exhaustive()
    }
}

impl InMemoryServerListener {
    /// Wraps one in-memory listener.
    #[must_use]
    pub fn new(listener: Arc<InMemoryListener>) -> Self {
        Self {
            listener,
            state: StdMutex::new(InMemoryListenerState::Idle),
            stop: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Returns the endpoint dialed by in-process clients.
    #[must_use]
    pub fn endpoint(&self) -> crate::remote::transport::InMemoryEndpoint {
        self.listener.endpoint()
    }
}

/// Bridges an accepted in-memory transport into a [`ByteConnection`].
struct InMemoryConnection {
    slot_rx: watch::Receiver<Option<Arc<dyn ByteTransport>>>,
    slot_tx: watch::Sender<Option<Arc<dyn ByteTransport>>>,
    closed: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    send_tail: StdMutex<Shared<BoxFuture<'static, ()>>>,
}

impl InMemoryConnection {
    fn new() -> Self {
        let (slot_tx, slot_rx) = watch::channel(None::<Arc<dyn ByteTransport>>);
        Self {
            slot_rx,
            slot_tx,
            closed: Arc::new(AtomicBool::new(false)),
            closing: Arc::new(AtomicBool::new(false)),
            send_tail: StdMutex::new(futures::future::ready(()).boxed().shared()),
        }
    }

    fn fill(&self, transport: Arc<dyn ByteTransport>) {
        self.slot_tx.send_replace(Some(Arc::clone(&transport)));
        if self.closed.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            transport.close();
        }
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closing.store(true, Ordering::SeqCst);
        self.slot_tx.send_modify(|_| {});
    }
}
impl ByteConnection for InMemoryConnection {
    fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn send(&self, chunk: Vec<u8>) -> BoxFuture<'static, Result<(), TransportError>> {
        if self.closed.load(Ordering::Acquire) {
            return Box::pin(futures::future::ready(Err(TransportError::Closed)));
        }
        let (sender, receiver) = oneshot::channel();
        let mut transport_rx = self.slot_rx.clone();
        let closed = Arc::clone(&self.closed);
        let task = {
            let mut tail = lock(&self.send_tail);
            if self.closed.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
                return Box::pin(futures::future::ready(Err(TransportError::Closed)));
            }
            let previous = tail.clone();
            let task = async move {
                previous.await;
                let result = async {
                    loop {
                        if closed.load(Ordering::Acquire) {
                            return Err(TransportError::Closed);
                        }
                        let transport = transport_rx.borrow().clone();
                        if let Some(transport) = transport {
                            return transport.send(chunk).await;
                        }
                        if transport_rx.changed().await.is_err() {
                            return Err(TransportError::Closed);
                        }
                    }
                }
                .await;
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
            return Box::pin(futures::future::ready(Ok(())));
        }
        let Some(transport) = self.slot_rx.borrow().clone() else {
            self.closed.store(true, Ordering::Release);
            self.slot_tx.send_modify(|_| {});
            return Box::pin(futures::future::ready(Ok(())));
        };
        let (sender, receiver) = oneshot::channel();
        let closed = Arc::clone(&self.closed);
        let task = {
            let mut tail = lock(&self.send_tail);
            let previous = tail.clone();
            let task = async move {
                previous.await;
                let result = async {
                    if let Some(chunk) = final_chunk {
                        transport.send(chunk).await?;
                    }
                    transport.close();
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
                    biased;
                    () = stop.notified() => break,
                    accepted = listener.accept(handlers) => accepted,
                };
                match accepted {
                    Ok(transport) => connection.fill(Arc::new(transport)),
                    Err(_) => break,
                }
            }
        });
        Box::pin(futures::future::ready(Ok(())))
    }

    fn close(&self) -> BoxFuture<'static, ()> {
        {
            let mut state = lock(&self.state);
            if matches!(*state, InMemoryListenerState::Idle) {
                *state = InMemoryListenerState::Closing;
            } else if matches!(*state, InMemoryListenerState::Started) {
                *state = InMemoryListenerState::Closing;
            }
        }
        self.stop.notify_waiters();
        Box::pin(futures::future::ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use tokio::sync::oneshot;

    struct RecordingHandlers {
        sender: Mutex<Option<oneshot::Sender<Vec<u8>>>>,
    }

    impl ByteTransportHandlers for RecordingHandlers {
        fn on_data(&self, chunk: Vec<u8>) {
            if let Some(sender) = self.sender.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = sender.send(chunk);
            }
        }

        fn on_close(&self) {}

        fn on_error(&self, _error: TransportError) {}
    }

    struct NoopHandlers;

    impl ByteTransportHandlers for NoopHandlers {
        fn on_data(&self, _chunk: Vec<u8>) {}
        fn on_close(&self) {}
        fn on_error(&self, _error: TransportError) {}
    }

    #[expect(
        clippy::expect_used,
        reason = "in-memory transport setup must succeed"
    )]
    #[tokio::test]
    async fn in_memory_server_connection_round_trip_preserves_chunks() {
        let (listener, endpoint) = InMemoryListener::new();
        let (sender, receiver) = oneshot::channel();
        let client_handlers = Arc::new(RecordingHandlers {
            sender: Mutex::new(Some(sender)),
        });
        let client = (endpoint.factory())(client_handlers).await.expect("client transport");
        let server_transport = listener
            .accept(Arc::new(NoopHandlers))
            .await
            .expect("server transport");
        let connection = InMemoryConnection::new();
        connection.fill(Arc::new(server_transport));

        connection
            .send(b"server-to-client".to_vec())
            .await
            .expect("queued server send");
        let chunk = tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("client receives chunk")
            .expect("client handler remains live");
        assert_eq!(chunk, b"server-to-client");

        connection.close(None).await.expect("close server connection");
        client.close();
    }

    #[tokio::test]
    async fn in_memory_close_before_accept_wakes_queued_send() {
        let connection = InMemoryConnection::new();
        let send = connection.send(b"queued-before-accept".to_vec());
        let close = connection.close(None);
        assert!(close.await.is_ok(), "close before accept must succeed");
        assert!(
            matches!(send.await, Err(TransportError::Closed)),
            "queued send must fail with TransportError::Closed"
        );
    }
}
