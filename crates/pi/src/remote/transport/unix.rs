//! Unix-domain socket transport — the real off-process adapter, gated to the
//! Unix tier (R3, AR2).
//!
//! Ports upstream `unix.ts`: a factory validating nothing (validation is
//! owned by [`crate::remote::transport::build_transport`]) and connecting a
//! fresh [`tokio::net::UnixStream`] per invocation, plus a writer with a
//! pending-byte budget so a stalled server cannot balloon client memory.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

use super::{
    ByteTransport, ByteTransportFactory, ByteTransportHandlers, ConnectFuture, SendFuture,
    TransportError,
};
use crate::remote::framing::DEFAULT_MAX_FRAME_LENGTH;

/// Read buffer for inbound chunks.
const READ_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderSignal {
    Open,
    LocallyClosed,
    Failed,
}

/// Queued outbound write items before writers feel backpressure; the
/// pending-byte budget bounds the bytes inside it.
const CHANNEL_CAPACITY: usize = 128;

struct Shared {
    signal: watch::Sender<ReaderSignal>,
    error: StdMutex<Option<TransportError>>,
    pending_bytes: AtomicUsize,
    max_pending_bytes: usize,
    closed: AtomicBool,
}

enum WriteItem {
    Bytes {
        bytes: Vec<u8>,
        done: oneshot::Sender<Result<(), TransportError>>,
    },
}

/// A connected Unix-domain byte transport.
pub struct UnixByteTransport {
    shared: Arc<Shared>,
    outbound: StdMutex<Option<mpsc::Sender<WriteItem>>>,
}

impl std::fmt::Debug for UnixByteTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixByteTransport").finish_non_exhaustive()
    }
}

impl UnixByteTransport {
    fn new(
        stream: UnixStream,
        max_pending_bytes: usize,
        handlers: Arc<dyn ByteTransportHandlers>,
    ) -> Arc<Self> {
        let (signal, _) = watch::channel(ReaderSignal::Open);
        let shared = Arc::new(Shared {
            signal,
            error: StdMutex::new(None),
            pending_bytes: AtomicUsize::new(0),
            max_pending_bytes,
            closed: AtomicBool::new(false),
        });
        let (mut read_half, write_half) = stream.into_split();
        let (outbound_tx, outbound_rx) = mpsc::channel::<WriteItem>(CHANNEL_CAPACITY);

        let reader_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut signal = reader_shared.signal.subscribe();
            // A watch subscriber only wakes for changes made after it
            // subscribed, so a terminal that fired before the first poll
            // would otherwise be missed. Seed from the current value,
            // bound out so no read borrow outlives handler calls.
            let seeded = *signal.borrow_and_update();
            match seeded {
                ReaderSignal::Open => {}
                ReaderSignal::LocallyClosed => return,
                ReaderSignal::Failed => {
                    let error = reader_shared
                        .error
                        .lock()
                        .expect("error lock")
                        .take()
                        .unwrap_or_else(|| TransportError::Message("unix transport failed".into()));
                    handlers.on_error(error);
                    return;
                }
            }
            let mut buffer = vec![0u8; READ_CHUNK_BYTES];
            loop {
                tokio::select! {
                    biased;
                    changed = signal.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        // Bind before matching: the Failed arm invokes
                        // handlers that may write the watch (mark_failed),
                        // so the read-lock temporary must not outlive the
                        // discriminant read.
                        let signal_value = *signal.borrow_and_update();
                        match signal_value {
                            ReaderSignal::Open => continue,
                            ReaderSignal::LocallyClosed => break,
                            ReaderSignal::Failed => {
                                let error = reader_shared
                                    .error
                                    .lock()
                                    .expect("error lock")
                                    .take()
                                    .unwrap_or_else(|| TransportError::Message("unix transport failed".into()));
                                handlers.on_error(error);
                                break;
                            }
                        }
                    }
                    read = read_half.read(&mut buffer) => match read {
                        Ok(0) => {
                            handlers.on_close();
                            break;
                        }
                        Ok(count) => {
                            handlers.on_data(buffer[..count].to_vec());
                        }
                        Err(error) => {
                            // Socket errors are terminal: surface one typed
                            // failure instead of a silent close.
                            mark_failed(
                                &reader_shared,
                                Some(TransportError::Io(error)),
                            );
                        }
                    },
                }
            }
        });

        let writer_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut write_half = write_half;
            let mut outbound_rx = outbound_rx;
            while let Some(item) = outbound_rx.recv().await {
                let WriteItem::Bytes { bytes, done } = item;
                let len = bytes.len();
                let result = match write_half.write_all(&bytes).await {
                    Ok(()) => match write_half.flush().await {
                        Ok(()) => Ok(()),
                        Err(error) => Err(TransportError::Io(error)),
                    },
                    Err(error) => Err(TransportError::Io(error)),
                };
                writer_shared.pending_bytes.fetch_sub(len, Ordering::SeqCst);
                if result.is_err() {
                    mark_failed(&writer_shared, result.clone().err());
                }
                let _ = done.send(result);
            }
            // Channel closed: the transport was closed — flush the socket
            // write side so the peer observes an orderly EOF.
            if !writer_shared.closed.load(Ordering::SeqCst) {
                let _ = write_half.shutdown().await;
            }
        });

        Arc::new(Self {
            shared,
            outbound: StdMutex::new(Some(outbound_tx)),
        })
    }
}

fn mark_failed(shared: &Shared, error: Option<TransportError>) {
    if let Some(error) = error {
        let mut slot = shared.error.lock().expect("error lock");
        if slot.is_none() {
            *slot = Some(error);
        }
    }
    shared.signal.send_replace(ReaderSignal::Failed);
}


impl ByteTransport for UnixByteTransport {
    fn send(&self, chunk: Vec<u8>) -> SendFuture {
        let outbound = self.outbound.lock().expect("outbound lock").clone();
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            if shared.closed.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            let Some(outbound) = outbound else {
                return Err(TransportError::Closed);
            };
            let len = chunk.len();
            let reserved = shared.pending_bytes.fetch_add(len, Ordering::SeqCst) + len;
            if reserved > shared.max_pending_bytes {
                shared.pending_bytes.fetch_sub(len, Ordering::SeqCst);
                return Err(TransportError::PendingBytesExceeded);
            }
            let (done_tx, done_rx) = oneshot::channel();
            outbound
                .send(WriteItem::Bytes { bytes: chunk, done: done_tx })
                .await
                .map_err(|_| {
                    shared.pending_bytes.fetch_sub(len, Ordering::SeqCst);
                    TransportError::Closed
                })?;
            match done_rx.await {
                Ok(result) => result,
                Err(_) => {
                    shared.pending_bytes.fetch_sub(len, Ordering::SeqCst);
                    Err(TransportError::Closed)
                }
            }
        })
    }

    fn close(&self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.outbound.lock().expect("outbound lock").take();
        self.shared.signal.send_replace(ReaderSignal::LocallyClosed);
    }
}

/// Options for [`connect`]; validation lives in
/// [`crate::remote::transport::build_transport`].
#[derive(Debug, Clone)]
pub struct UnixTransportOptions {
    /// Socket path.
    pub path: std::path::PathBuf,
    /// Outbound pending-byte budget; defaults to four times the 16 MiB
    /// frame bound, matching upstream.
    pub max_pending_bytes: Option<usize>,
}

/// Connects one fresh Unix-domain transport for the given handlers.
pub async fn connect(
    options: &UnixTransportOptions,
    handlers: Arc<dyn ByteTransportHandlers>,
) -> Result<Arc<dyn ByteTransport>, TransportError> {
    let stream = UnixStream::connect(&options.path).await.map_err(TransportError::Io)?;
    let max_pending_bytes =
        options.max_pending_bytes.unwrap_or(DEFAULT_MAX_FRAME_LENGTH * 4);
    Ok(UnixByteTransport::new(stream, max_pending_bytes, handlers))
}

/// Builds the factory used by [`crate::remote::transport::build_transport`]
/// on the Unix tier.
pub fn factory(
    path: std::path::PathBuf,
    max_pending_bytes: Option<usize>,
) -> ByteTransportFactory {
    Arc::new(move |handlers| {
        let options = UnixTransportOptions {
            path: path.clone(),
            max_pending_bytes,
        };
        Box::pin(async move { connect(&options, handlers).await }) as ConnectFuture
    })
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
        fn on_data(&self, chunk: Vec<u8>) {
            self.chunks.lock().expect("chunks").push(chunk);
        }
        fn on_close(&self) {
            self.closes.fetch_add(1, Ordering::SeqCst);
        }
        fn on_error(&self, error: TransportError) {
            self.errors.lock().expect("errors").push(error.to_string());
        }
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

    #[tokio::test]
    async fn real_socket_roundtrip_and_orderly_close() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("pi-client-unix.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let factory = factory(path, None);
        let client_handlers = Arc::new(RecordingHandlers::default());
        let connecting = factory(Arc::clone(&client_handlers) as Arc<dyn ByteTransportHandlers>);

        let (transport, accepted) = tokio::join!(connecting, listener.accept());
        let transport = transport.expect("connect");
        let (server_stream, _addr) = accepted.expect("accept");

        transport.send(b"ping".to_vec()).await.expect("send");
        let echo = echo_once(server_stream).await;
        await_condition(|| !client_handlers.chunks.lock().expect("chunks").is_empty()).await;
        assert_eq!(
            client_handlers.chunks.lock().expect("chunks").clone(),
            vec![echo]
        );

        transport.close();
        transport.close();
        await_condition(|| client_handlers.closes.load(Ordering::SeqCst) == 1).await;
        assert_eq!(client_handlers.closes.load(Ordering::SeqCst), 1);
        assert!(client_handlers.errors.lock().expect("errors").is_empty());
        let err = transport.send(b"x".to_vec()).await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }

    async fn echo_once(mut stream: tokio::net::UnixStream) -> Vec<u8> {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut buffer = vec![0u8; 4096];
        let count = stream.read(&mut buffer).await.expect("server read");
        let out = buffer[..count].to_vec();
        stream.write_all(&out).await.expect("server write");
        out
    }

    #[tokio::test]
    async fn connect_failure_is_typed_io_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("missing.sock");
        let factory = factory(path, None);
        let handlers = Arc::new(RecordingHandlers::default());
        let error = factory(handlers as Arc<dyn ByteTransportHandlers>)
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, TransportError::Io(_)));
    }

    #[tokio::test]
    async fn pending_byte_budget_rejects_oversized_single_write() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("budget.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");

        let factory = factory(path, Some(8));
        let client_handlers = Arc::new(RecordingHandlers::default());
        let connecting = factory(Arc::clone(&client_handlers) as Arc<dyn ByteTransportHandlers>);
        let (transport, accepted) = tokio::join!(connecting, listener.accept());
        let transport = transport.expect("connect");
        let (server_stream, _addr) = accepted.expect("accept");
        let _server_held = server_stream;

        let error = transport.send(vec![0u8; 9]).await.unwrap_err();
        assert!(
            matches!(error, TransportError::PendingBytesExceeded),
            "a single write above the budget must be rejected, got {error:?}"
        );
        transport.close();
    }
}
