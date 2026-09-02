//! In-memory byte transport — an unconditional, real adapter for tests and
//! same-process servers (R3).
//!
//! [`InMemoryListener`] accepts connections dialed from a cloneable
//! [`InMemoryEndpoint`]; both ends are [`InMemoryTransport`]s implementing
//! [`ByteTransport`] over bounded tokio channels, so the pair exercises the
//! exact client seam (framing, ordering, backpressure, terminal delivery)
//! that the Unix adapter exercises across a process boundary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{mpsc, watch};

use super::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, SendFuture, TransportError,
};

/// Pending chunks buffered per direction before writers feel backpressure.
/// Chunks are whole frames (bounded by the frame limit in real use).
const CHANNEL_CAPACITY: usize = 16;

/// Terminal control for one side's reader loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideSignal {
    /// No terminal signal yet.
    Open,
    /// The owning side closed locally; its reader stops silently.
    LocallyClosed,
    /// The peer injected a failure; the reader delivers `on_error`.
    PeerFailed,
}

struct SideCell {
    signal: watch::Sender<SideSignal>,
    error: StdMutex<Option<TransportError>>,
    handlers: StdMutex<Option<Arc<dyn ByteTransportHandlers>>>,
    inbound: StdMutex<Option<mpsc::Receiver<Vec<u8>>>>,
    closed: AtomicBool,
}

impl SideCell {
    fn new(inbound: mpsc::Receiver<Vec<u8>>) -> Self {
        let (signal, _) = watch::channel(SideSignal::Open);
        Self {
            signal,
            error: StdMutex::new(None),
            handlers: StdMutex::new(None),
            inbound: StdMutex::new(Some(inbound)),
            closed: AtomicBool::new(false),
        }
    }
}

struct PairCore {
    sides: [SideCell; 2],
}

/// Reads one side's inbound channel until a terminal condition and delivers
/// exactly one terminal handler call (close or error), never both.
#[expect(
    clippy::expect_used,
    reason = "mutex poisoning is fatal; lock is never held across a panic"
)]
async fn read_loop(core: Arc<PairCore>, index: usize) {
    let side = &core.sides[index];
    let Some(mut inbound) = side.inbound.lock().expect("inbound lock").take() else {
        return;
    };
    let Some(handlers) = side.handlers.lock().expect("handlers lock").clone() else {
        return;
    };
    let mut signal = side.signal.subscribe();
    // A watch subscriber only wakes for changes made after it subscribed,
    // so a terminal that fired before the first poll would otherwise be
    // missed. Seed the loop from the current value first. The value is
    // bound out before matching: the arms invoke handlers that may write
    // this watch (transport close), so no borrow may outlive the read.
    let seeded = *signal.borrow_and_update();
    match seeded {
        SideSignal::Open => {}
        SideSignal::LocallyClosed => return,
        SideSignal::PeerFailed => {
            let error = side
                .error
                .lock()
                .expect("error lock")
                .take()
                .unwrap_or_else(|| TransportError::Message("in-memory transport failed".into()));
            handlers.on_error(error);
            return;
        }
    }
    loop {
        tokio::select! {
            biased;
            changed = signal.changed() => {
                if changed.is_err() {
                    break;
                }
                // Bind before matching: the match arms invoke handlers that
                // may write the watch (transport close), so the scrutinee's
                // read-lock temporary must not outlive the discriminant read.
                let signal_value = *signal.borrow_and_update();
                match signal_value {
                    SideSignal::Open => {},
                    SideSignal::LocallyClosed => break,
                    SideSignal::PeerFailed => {
                        let error = side
                            .error
                            .lock()
                            .expect("error lock")
                            .take()
                            .unwrap_or_else(|| TransportError::Message("in-memory transport failed".into()));
                        handlers.on_error(error);
                        break;
                    }
                }
            }
            chunk = inbound.recv() => {
                if let Some(bytes) = chunk {
                    handlers.on_data(bytes);
                } else {
                    handlers.on_close();
                    break;
                }
            }
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "mutex poisoning is fatal; lock is never held across a panic"
)]
fn start_reader(core: &Arc<PairCore>, index: usize, handlers: Arc<dyn ByteTransportHandlers>) {
    {
        let side = &core.sides[index];
        *side.handlers.lock().expect("handlers lock") = Some(handlers);
    }
    let core = Arc::clone(core);
    tokio::spawn(async move {
        read_loop(core, index).await;
    });
}

/// One end of a paired in-memory byte pipe.
#[derive(Clone)]
pub struct InMemoryTransport {
    core: Arc<PairCore>,
    index: usize,
    outbound: Arc<StdMutex<Option<mpsc::Sender<Vec<u8>>>>>,
}

impl std::fmt::Debug for InMemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryTransport").finish_non_exhaustive()
    }
}

impl InMemoryTransport {
    /// Closes this end idempotently: the local reader stops without a
    /// terminal handler and the peer observes an orderly EOF.
    /// # Panics
    ///
    /// Panics if the outbound mutex is poisoned — this is fatal and never
    /// held across a panic.
    #[expect(
        clippy::expect_used,
        reason = "mutex poisoning is fatal; lock is never held across a panic"
    )]
    pub fn close(&self) {
        let side = &self.core.sides[self.index];
        if side.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.outbound.lock().expect("outbound lock").take();
        // send_replace (not send): the terminal wake must be unconditional —
        // watch::send may skip notification, which strands a parked reader.
        side.signal.send_replace(SideSignal::LocallyClosed);
    }

    /// Injects a typed terminal failure into the peer's handlers (mirrors
    /// upstream test affordance `server.error(error)`).
    /// # Panics
    ///
    /// Panics if the peer error mutex is poisoned — this is fatal and never
    /// held across a panic.
    #[expect(
        clippy::expect_used,
        reason = "mutex poisoning is fatal; lock is never held across a panic"
    )]
    pub fn fail_peer(&self, error: TransportError) {
        self.close();
        let peer = &self.core.sides[1 - self.index];
        *peer.error.lock().expect("peer error lock") = Some(error);
        peer.signal.send_replace(SideSignal::PeerFailed);
    }
}

impl ByteTransport for InMemoryTransport {
    #[expect(
        clippy::expect_used,
        reason = "mutex poisoning is fatal; lock is never held across a panic"
    )]
    fn send(&self, chunk: Vec<u8>) -> SendFuture {
        let outbound = self.outbound.lock().expect("outbound lock").clone();
        let closed = self.core.sides[self.index].closed.load(Ordering::SeqCst);
        Box::pin(async move {
            if closed {
                return Err(TransportError::Closed);
            }
            let Some(outbound) = outbound else {
                return Err(TransportError::Closed);
            };
            outbound
                .send(chunk)
                .await
                .map_err(|_| TransportError::Closed)
        })
    }

    fn close(&self) {
        InMemoryTransport::close(self);
    }
}

/// Address of an [`InMemoryListener`]; cloneable and dialable through
/// [`crate::remote::transport::build_transport`].
#[derive(Clone)]
pub struct InMemoryEndpoint {
    dial_tx: mpsc::Sender<InMemoryTransport>,
}

impl std::fmt::Debug for InMemoryEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryEndpoint").finish_non_exhaustive()
    }
}

impl InMemoryEndpoint {
    fn dial(
        &self,
        handlers: Arc<dyn ByteTransportHandlers>,
    ) -> Result<InMemoryTransport, TransportError> {
        let (client, server) = pair();
        start_reader(&client.core, client.index, handlers);
        self.dial_tx.try_send(server).map_err(|_| {
            TransportError::Message("in-memory listener is closed or saturated".into())
        })?;
        Ok(client)
    }

    pub(crate) fn factory(&self) -> ByteTransportFactory {
        let endpoint = self.clone();
        Arc::new(move |handlers| {
            let endpoint = endpoint.clone();
            Box::pin(async move {
                endpoint
                    .dial(handlers)
                    .map(|transport| Arc::new(transport) as Arc<dyn ByteTransport>)
            })
        })
    }
}

/// Accepts dialed in-memory connections.
pub struct InMemoryListener {
    dial_rx: tokio::sync::Mutex<mpsc::Receiver<InMemoryTransport>>,
    endpoint: InMemoryEndpoint,
}

impl std::fmt::Debug for InMemoryListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryListener").finish_non_exhaustive()
    }
}

impl InMemoryListener {
    /// Creates a listener and its dialable endpoint.
    #[must_use]
    pub fn new() -> (Self, InMemoryEndpoint) {
        let (dial_tx, dial_rx) = mpsc::channel(1);
        let endpoint = InMemoryEndpoint { dial_tx };
        let listener = Self {
            dial_rx: tokio::sync::Mutex::new(dial_rx),
            endpoint: endpoint.clone(),
        };
        (listener, endpoint)
    }

    /// Returns the endpoint clients dial.
    #[must_use]
    pub fn endpoint(&self) -> InMemoryEndpoint {
        self.endpoint.clone()
    }

    /// Waits for the next dialed connection and installs `handlers` on the
    /// accepted end. Bytes sent before the accept are buffered by the
    /// bounded pipe channel.
    /// # Errors
    ///
    /// Returns [`TransportError::Message`] if the listener was closed before
    /// a connection arrived.
    pub async fn accept(
        &self,
        handlers: Arc<dyn ByteTransportHandlers>,
    ) -> Result<InMemoryTransport, TransportError> {
        let server = {
            let mut dial_rx = self.dial_rx.lock().await;
            dial_rx
                .recv()
                .await
                .ok_or_else(|| TransportError::Message("in-memory listener closed".into()))?
        };
        start_reader(&server.core, server.index, handlers);
        Ok(server)
    }
}

fn pair() -> (InMemoryTransport, InMemoryTransport) {
    let (a_to_b, b_inbound) = mpsc::channel(CHANNEL_CAPACITY);
    let (b_to_a, a_inbound) = mpsc::channel(CHANNEL_CAPACITY);
    let core = Arc::new(PairCore {
        sides: [SideCell::new(a_inbound), SideCell::new(b_inbound)],
    });
    let a = InMemoryTransport {
        core: Arc::clone(&core),
        index: 0,
        outbound: Arc::new(StdMutex::new(Some(a_to_b))),
    };
    let b = InMemoryTransport {
        core: Arc::clone(&core),
        index: 1,
        outbound: Arc::new(StdMutex::new(Some(b_to_a))),
    };
    (a, b)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[derive(Default)]
    struct RecordingHandlers {
        chunks: StdMutex<Vec<Vec<u8>>>,
        closes: AtomicUsize,
        errors: StdMutex<Vec<String>>,
    }

    impl ByteTransportHandlers for RecordingHandlers {
        #[expect(clippy::expect_used, reason = "test handler: mutex poisoning is fatal")]
        fn on_data(&self, chunk: Vec<u8>) {
            self.chunks.lock().expect("chunks").push(chunk);
        }
        fn on_close(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        #[expect(clippy::expect_used, reason = "test handler: mutex poisoning is fatal")]
        fn on_error(&self, error: TransportError) {
            self.errors.lock().expect("errors").push(error.to_string());
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "test setup: dial and accept must succeed"
    )]
    async fn accepted_pair() -> (
        Arc<dyn ByteTransport>,
        InMemoryTransport,
        Arc<RecordingHandlers>,
        Arc<RecordingHandlers>,
    ) {
        let (listener, endpoint) = InMemoryListener::new();
        let client_handlers = Arc::new(RecordingHandlers::default());
        let server_handlers = Arc::new(RecordingHandlers::default());
        let factory = endpoint.factory();
        let client = factory(Arc::clone(&client_handlers) as Arc<dyn ByteTransportHandlers>)
            .await
            .expect("dial");
        let server = listener
            .accept(Arc::clone(&server_handlers) as Arc<dyn ByteTransportHandlers>)
            .await
            .expect("accept");
        (client, server, client_handlers, server_handlers)
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: in-memory transport operations must succeed"
    )]
    #[tokio::test]
    async fn pair_delivers_ordered_chunks_both_ways() {
        let (client, server, client_handlers, server_handlers) = accepted_pair().await;
        client.send(b"one".to_vec()).await.expect("send one");
        client.send(b"two".to_vec()).await.expect("send two");
        server.send(b"ack".to_vec()).await.expect("send ack");
        await_chunks(&server_handlers, 2).await;
        await_chunks(&client_handlers, 1).await;
        assert_eq!(
            server_handlers.chunks.lock().expect("chunks").clone(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(
            client_handlers.chunks.lock().expect("chunks").clone(),
            vec![b"ack".to_vec()]
        );
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: in-memory transport operations must succeed"
    )]
    #[tokio::test]
    async fn peer_close_delivers_exactly_one_on_close() {
        let (client, server, client_handlers, _server_handlers) = accepted_pair().await;
        server.close();
        server.close();
        await_condition(|| client_handlers.closes.load(Ordering::SeqCst) == 1).await;
        assert_eq!(client_handlers.closes.load(Ordering::SeqCst), 1);
        assert!(client_handlers.errors.lock().expect("errors").is_empty());
        // The local send buffer only observes the peer's departure once
        // the peer's reader releases the channel: the failure is eventual.
        let err = {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match client.send(b"x".to_vec()).await {
                    Err(err) => break err,
                    Ok(()) => assert!(
                        tokio::time::Instant::now() < deadline,
                        "send must fail once the peer's reader releases the channel"
                    ),
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        };
        assert!(matches!(err, TransportError::Closed));
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: in-memory transport operations must succeed"
    )]
    #[tokio::test]
    async fn fail_peer_delivers_exactly_one_typed_on_error() {
        let (_client, server, client_handlers, _server_handlers) = accepted_pair().await;
        server.fail_peer(TransportError::Message("read failed".into()));
        await_condition(|| !client_handlers.errors.lock().expect("errors").is_empty()).await;
        assert_eq!(
            client_handlers.errors.lock().expect("errors").clone(),
            vec!["read failed".to_string()]
        );
        assert_eq!(client_handlers.closes.load(Ordering::SeqCst), 0);
    }

    #[expect(
        clippy::expect_used,
        reason = "test assertions: in-memory transport operations must succeed"
    )]
    #[tokio::test]
    async fn buffered_sends_before_accept_are_delivered_in_order() {
        let (listener, endpoint) = InMemoryListener::new();
        let client_handlers = Arc::new(RecordingHandlers::default());
        let server_handlers = Arc::new(RecordingHandlers::default());
        let factory = endpoint.factory();
        let client = factory(Arc::clone(&client_handlers) as Arc<dyn ByteTransportHandlers>)
            .await
            .expect("dial");
        client.send(b"early".to_vec()).await.expect("early send");
        let _server = listener
            .accept(Arc::clone(&server_handlers) as Arc<dyn ByteTransportHandlers>)
            .await
            .expect("accept");
        await_chunks(&server_handlers, 1).await;
        assert_eq!(
            server_handlers.chunks.lock().expect("chunks").clone(),
            vec![b"early".to_vec()]
        );
    }

    #[expect(clippy::expect_used, reason = "test helper: mutex poisoning is fatal")]
    async fn await_chunks(handlers: &RecordingHandlers, count: usize) {
        await_condition(|| handlers.chunks.lock().expect("chunks").len() >= count).await;
    }

    async fn await_condition(condition: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for condition"
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }
}
