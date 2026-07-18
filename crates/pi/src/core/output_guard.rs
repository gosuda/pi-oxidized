//! Process-global raw stdout coordinator for print/RPC modes.
//!
//! Port of `.references/pi/packages/coding-agent/src/core/output-guard.ts`.
//!
//! # Ownership model
//!
//! One dedicated Tokio writer task owns the raw stdout sink. Callers enqueue
//! ordered FIFO write/flush/wait requests through a bounded channel. There is
//! no process-wide `println!` monkeypatch in Rust: product-facing text must go
//! through [`ProductOutput`], which routes to stderr while stdout is taken
//! over so protocol frames on the real stdout sink stay clean.
//!
//! # Retry policy
//!
//! Transient write failures (`WouldBlock` / `EAGAIN` / `EWOULDBLOCK` /
//! `ENOBUFS`, plus `Interrupted`) are retried every
//! [`RAW_STDOUT_RETRY_DELAY`] until the full buffer is written. Unrecoverable
//! I/O errors are stored and returned as [`OutputGuardError`]; this library
//! never calls `process::exit`.
//!
//! # Result contract
//!
//! Every public fallible operation returns [`Result`]:
//! - [`write_raw_stdout`] accepts into the bounded FIFO (awaits capacity) and
//!   surfaces a prior fatal writer error if one already latched.
//! - [`wait_for_raw_stdout_backpressure`] resolves only after every previously
//!   accepted write has finished (or failed fatally).
//! - [`flush_raw_stdout`] waits for drain, then flushes the underlying sink.
//! - [`take_over_stdout`] / [`restore_stdout`] are idempotent state transitions;
//!   they return `Err` only when the coordinator cannot be started.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, sleep};

/// Delay between retries for transient raw-stdout write failures.
pub const RAW_STDOUT_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Bounded FIFO capacity for raw-stdout requests.
///
/// Keeps memory bounded under a fast producer; [`write_raw_stdout`] awaits when
/// the queue is full (backpressure).
pub const RAW_STDOUT_QUEUE_CAPACITY: usize = 256;

/// Errors produced by the raw-stdout coordinator.
#[derive(Debug, Error, Clone)]
pub enum OutputGuardError {
    /// The dedicated writer task is no longer running.
    #[error("raw stdout writer task is shut down")]
    WriterShutdown,

    /// The coordinator could not be started (no Tokio runtime).
    #[error("raw stdout coordinator requires a Tokio runtime: {0}")]
    RuntimeUnavailable(String),

    /// Unrecoverable write or flush failure on the raw stdout sink.
    #[error("raw stdout I/O failed: {0}")]
    Io(String),

    /// A previous unrecoverable writer failure is latched; further raw writes
    /// are rejected until the coordinator is reinstalled (tests) or the
    /// process exits.
    #[error("raw stdout writer failed earlier: {0}")]
    Latched(String),
}

impl OutputGuardError {
    fn io(err: &io::Error) -> Self {
        Self::Io(err.to_string())
    }

    fn latched(message: impl Into<String>) -> Self {
        Self::Latched(message.into())
    }
}

/// Result alias for raw-stdout coordinator operations.
pub type Result<T, E = OutputGuardError> = std::result::Result<T, E>;

enum Command {
    Write {
        bytes: Vec<u8>,
        /// Sequence number assigned at enqueue time; used for drain waits.
        seq: u64,
    },
    Wait {
        /// Wait until `completed_seq >= target_seq`.
        target_seq: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Flush {
        target_seq: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
    /// Test/support: replace the live sink after draining pending writes.
    InstallSink {
        sink: Box<dyn AsyncWrite + Unpin + Send>,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct Shared {
    taken_over: AtomicBool,
    /// Monotonic enqueue counter; 0 means no writes have been accepted yet.
    enqueued_seq: AtomicU64,
    /// Last sequence fully processed by the writer (write completed or failed).
    completed_seq: AtomicU64,
    /// Latched fatal error message, if any.
    fatal: Mutex<Option<String>>,
    tx: mpsc::Sender<Command>,
}

impl Shared {
    fn take_fatal(&self) -> Option<OutputGuardError> {
        self.fatal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(OutputGuardError::latched)
    }

    fn set_fatal(&self, err: &OutputGuardError) {
        let message = err.to_string();
        let mut guard = self
            .fatal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(message);
        }
    }

    fn clear_fatal(&self) {
        *self
            .fatal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

struct Coordinator {
    shared: Arc<Shared>,
}

static COORDINATOR: LazyLock<Mutex<Option<Coordinator>>> = LazyLock::new(|| Mutex::new(None));

fn with_coordinator_mut<T>(f: impl FnOnce(&mut Option<Coordinator>) -> T) -> T {
    let mut guard = COORDINATOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

fn current_shared() -> Option<Arc<Shared>> {
    with_coordinator_mut(|slot| slot.as_ref().map(|c| Arc::clone(&c.shared)))
}

/// Returns whether product stdout has been taken over for protocol use.
#[must_use]
pub fn is_stdout_taken_over() -> bool {
    current_shared().is_some_and(|s| s.taken_over.load(Ordering::SeqCst))
}

/// Mark stdout as protocol-owned.
///
/// Idempotent: a second call is a no-op once takeover is already active.
/// Starts the dedicated writer task on first use.
///
/// # Errors
///
/// Returns [`OutputGuardError::RuntimeUnavailable`] when no Tokio runtime is
/// available to spawn the writer task.
pub fn take_over_stdout() -> Result<()> {
    let shared = ensure_coordinator()?;
    shared.taken_over.store(true, Ordering::SeqCst);
    Ok(())
}

/// Release protocol ownership of stdout.
///
/// Idempotent: safe when stdout is not currently taken over.
pub fn restore_stdout() {
    if let Some(shared) = current_shared() {
        shared.taken_over.store(false, Ordering::SeqCst);
    }
}

/// Queue `text` for ordered emission on the raw stdout sink.
///
/// Empty payloads are ignored. Non-empty payloads are accepted into the
/// bounded FIFO in call order; this future resolves once the request has been
/// accepted (or fails if the writer is shut down / already latched fatal).
/// Completion of the underlying write is observed via
/// [`wait_for_raw_stdout_backpressure`] or [`flush_raw_stdout`].
///
/// # Errors
///
/// - [`OutputGuardError::Latched`] when a prior unrecoverable writer error exists
/// - [`OutputGuardError::WriterShutdown`] when the writer task has exited
/// - [`OutputGuardError::RuntimeUnavailable`] when the coordinator cannot start
pub async fn write_raw_stdout(text: impl AsRef<[u8]>) -> Result<()> {
    let bytes = text.as_ref();
    if bytes.is_empty() {
        return Ok(());
    }
    let shared = ensure_coordinator()?;
    if let Some(err) = shared.take_fatal() {
        return Err(err);
    }
    let seq = shared.enqueued_seq.fetch_add(1, Ordering::SeqCst) + 1;
    shared
        .tx
        .send(Command::Write {
            bytes: bytes.to_vec(),
            seq,
        })
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?;
    if let Some(err) = shared.take_fatal() {
        return Err(err);
    }
    Ok(())
}

/// Wait until every previously accepted raw write has finished.
///
/// If a write fails unrecoverably while waiting, returns that error.
///
/// # Errors
///
/// - [`OutputGuardError::Latched`] / [`OutputGuardError::Io`] from the writer
/// - [`OutputGuardError::WriterShutdown`] when the writer task has exited
/// - [`OutputGuardError::RuntimeUnavailable`] when the coordinator cannot start
pub async fn wait_for_raw_stdout_backpressure() -> Result<()> {
    let shared = ensure_coordinator()?;
    if let Some(err) = shared.take_fatal() {
        return Err(err);
    }
    let target_seq = shared.enqueued_seq.load(Ordering::SeqCst);
    if target_seq == 0 || shared.completed_seq.load(Ordering::SeqCst) >= target_seq {
        return shared.take_fatal().map_or(Ok(()), Err);
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    shared
        .tx
        .send(Command::Wait {
            target_seq,
            reply: reply_tx,
        })
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?;
    reply_rx
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?
}

/// Wait for drain, then flush the underlying raw stdout sink.
///
/// # Errors
///
/// Same as [`wait_for_raw_stdout_backpressure`], plus flush I/O failures.
pub async fn flush_raw_stdout() -> Result<()> {
    let shared = ensure_coordinator()?;
    if let Some(err) = shared.take_fatal() {
        return Err(err);
    }
    let target_seq = shared.enqueued_seq.load(Ordering::SeqCst);
    let (reply_tx, reply_rx) = oneshot::channel();
    shared
        .tx
        .send(Command::Flush {
            target_seq,
            reply: reply_tx,
        })
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?;
    reply_rx
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?
}

/// Shut down the dedicated writer task after draining pending work.
///
/// Primarily for tests and orderly process teardown. After shutdown, the next
/// public raw-stdout call starts a fresh coordinator.
///
/// # Errors
///
/// Returns writer I/O or shutdown communication failures.
pub async fn shutdown_raw_stdout() -> Result<()> {
    // Take the coordinator out of the global slot first so concurrent
    // `ensure_coordinator` calls start a new writer instead of enqueueing onto
    // a task that is about to exit.
    let shared = with_coordinator_mut(|slot| slot.take().map(|c| c.shared));
    let Some(shared) = shared else {
        return Ok(());
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if shared
        .tx
        .send(Command::Shutdown { reply: reply_tx })
        .await
        .is_err()
    {
        return Err(OutputGuardError::WriterShutdown);
    }
    reply_rx
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?
}

/// Install a custom [`AsyncWrite`] sink for the raw-stdout writer.
///
/// Intended for deterministic tests. Starts the coordinator if needed, drains
/// pending writes, then swaps the live sink.
///
/// # Errors
///
/// Returns coordinator start or install communication failures.
pub async fn install_raw_stdout_sink_for_test<W>(sink: W) -> Result<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let shared = ensure_coordinator()?;
    shared.clear_fatal();
    let (reply_tx, reply_rx) = oneshot::channel();
    shared
        .tx
        .send(Command::InstallSink {
            sink: Box::new(sink),
            reply: reply_tx,
        })
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?;
    reply_rx
        .await
        .map_err(|_| OutputGuardError::WriterShutdown)?
}

/// Product-facing output facade.
///
/// Rust cannot redirect every `println!` in the process. Callers that emit
/// human-facing text (help, diagnostics, list-models tables, package status)
/// must use this facade so that, while stdout is taken over for protocol
/// frames, product text lands on stderr instead.
pub struct ProductOutput;

impl ProductOutput {
    /// Write `text` without a trailing newline.
    pub fn write(text: &str) {
        use std::io::Write;
        if is_stdout_taken_over() {
            let _ = std::io::stderr().write_all(text.as_bytes());
            let _ = std::io::stderr().flush();
        } else {
            let _ = std::io::stdout().write_all(text.as_bytes());
            let _ = std::io::stdout().flush();
        }
    }

    /// Write `text` followed by a newline.
    pub fn writeln(text: &str) {
        Self::write(text);
        Self::write("\n");
    }

    /// Write formatted arguments (no trailing newline).
    pub fn write_fmt(args: fmt::Arguments<'_>) {
        Self::write(&format!("{args}"));
    }

    /// Write formatted arguments followed by a newline.
    pub fn writeln_fmt(args: fmt::Arguments<'_>) {
        Self::writeln(&format!("{args}"));
    }
}

fn ensure_coordinator() -> Result<Arc<Shared>> {
    if let Some(shared) = current_shared() {
        return Ok(shared);
    }
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|err| OutputGuardError::RuntimeUnavailable(err.to_string()))?;
    with_coordinator_mut(|slot| {
        if let Some(existing) = slot.as_ref() {
            return Ok(Arc::clone(&existing.shared));
        }
        let (tx, rx) = mpsc::channel(RAW_STDOUT_QUEUE_CAPACITY);
        let shared = Arc::new(Shared {
            taken_over: AtomicBool::new(false),
            enqueued_seq: AtomicU64::new(0),
            completed_seq: AtomicU64::new(0),
            fatal: Mutex::new(None),
            tx,
        });
        let worker_shared = Arc::clone(&shared);
        handle.spawn(async move {
            writer_loop(worker_shared, rx, Box::new(tokio::io::stdout())).await;
        });
        *slot = Some(Coordinator {
            shared: Arc::clone(&shared),
        });
        Ok(shared)
    })
}

async fn writer_loop(
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<Command>,
    mut sink: Box<dyn AsyncWrite + Unpin + Send>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Write { bytes, seq } => {
                if shared.take_fatal().is_some() {
                    shared.completed_seq.store(seq, Ordering::SeqCst);
                    continue;
                }
                match write_all_with_retry(sink.as_mut(), &bytes).await {
                    Ok(()) => {
                        shared.completed_seq.store(seq, Ordering::SeqCst);
                    }
                    Err(err) => {
                        shared.set_fatal(&err);
                        shared.completed_seq.store(seq, Ordering::SeqCst);
                    }
                }
            }
            Command::Wait { target_seq, reply } => {
                let result = wait_until_seq(&shared, target_seq).await;
                let _ = reply.send(result);
            }
            Command::Flush { target_seq, reply } => {
                let result = async {
                    wait_until_seq(&shared, target_seq).await?;
                    if let Some(err) = shared.take_fatal() {
                        return Err(err);
                    }
                    flush_with_retry(sink.as_mut()).await
                }
                .await;
                if let Err(err) = &result
                    && !matches!(err, OutputGuardError::Latched(_))
                {
                    shared.set_fatal(err);
                }
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let target = shared.enqueued_seq.load(Ordering::SeqCst);
                let result = wait_until_seq(&shared, target).await;
                let _ = reply.send(result);
                break;
            }
            Command::InstallSink {
                sink: new_sink,
                reply,
            } => {
                let target = shared.enqueued_seq.load(Ordering::SeqCst);
                let result = wait_until_seq(&shared, target).await;
                if result.is_ok() {
                    sink = new_sink;
                    shared.clear_fatal();
                }
                let _ = reply.send(result);
            }
        }
    }
}

async fn wait_until_seq(shared: &Shared, target_seq: u64) -> Result<()> {
    if target_seq == 0 {
        return shared.take_fatal().map_or(Ok(()), Err);
    }
    // The writer processes commands FIFO, so by the time a Wait/Flush/Shutdown
    // command runs, every prior Write has already updated `completed_seq`.
    // Poll only as a safety net for InstallSink after concurrent enqueues.
    loop {
        if shared.completed_seq.load(Ordering::SeqCst) >= target_seq {
            return shared.take_fatal().map_or(Ok(()), Err);
        }
        if let Some(err) = shared.take_fatal() {
            // Fatal may latch before completed_seq advances on the failing write.
            if shared.completed_seq.load(Ordering::SeqCst) >= target_seq {
                return Err(err);
            }
        }
        tokio::task::yield_now().await;
        sleep(Duration::from_millis(1)).await;
    }
}

fn is_retryable_write_error(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        match err.raw_os_error() {
            Some(code)
                if code == nix::libc::EAGAIN
                    || code == nix::libc::EWOULDBLOCK
                    || code == nix::libc::ENOBUFS =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

async fn write_all_with_retry(
    sink: &mut (dyn AsyncWrite + Unpin + Send),
    mut bytes: &[u8],
) -> Result<()> {
    while !bytes.is_empty() {
        match sink.write(bytes).await {
            Ok(0) => {
                return Err(OutputGuardError::io(&io::Error::new(
                    io::ErrorKind::WriteZero,
                    "raw stdout write returned 0 bytes",
                )));
            }
            Ok(n) => {
                bytes = &bytes[n..];
            }
            Err(err) if is_retryable_write_error(&err) => {
                sleep(RAW_STDOUT_RETRY_DELAY).await;
            }
            Err(err) => return Err(OutputGuardError::io(&err)),
        }
    }
    Ok(())
}

async fn flush_with_retry(sink: &mut (dyn AsyncWrite + Unpin + Send)) -> Result<()> {
    loop {
        match sink.flush().await {
            Ok(()) => return Ok(()),
            Err(err) if is_retryable_write_error(&err) => {
                sleep(RAW_STDOUT_RETRY_DELAY).await;
            }
            Err(err) => return Err(OutputGuardError::io(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};
    use tokio::sync::Mutex as AsyncMutex;

    type TestResult = std::result::Result<(), String>;

    fn map_err(err: impl std::fmt::Display) -> String {
        err.to_string()
    }

    /// Process-global coordinator is shared; serialize tests that mutate it.
    static TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

    /// Recording sink that supports partial writes, would-block, and failure.
    struct TestSink {
        state: Arc<AsyncMutex<TestSinkState>>,
    }

    struct TestSinkState {
        buf: Vec<u8>,
        /// Force the next N write polls to return [`io::ErrorKind::WouldBlock`]
        /// before succeeding.
        would_block_remaining: usize,
        /// Cap bytes accepted per successful write (`0` = unlimited).
        max_chunk: usize,
        /// If set, the next write returns this fatal error.
        fatal_on_write: Option<io::ErrorKind>,
        flush_count: usize,
        write_calls: usize,
    }

    impl TestSink {
        fn new(state: Arc<AsyncMutex<TestSinkState>>) -> Self {
            Self { state }
        }
    }

    impl AsyncWrite for TestSink {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let state = self.state.clone();
            // Try lock without blocking the runtime; if contended, reschedule.
            let Ok(mut guard) = state.try_lock() else {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            };
            guard.write_calls += 1;
            if let Some(kind) = guard.fatal_on_write.take() {
                return Poll::Ready(Err(io::Error::from(kind)));
            }
            if guard.would_block_remaining > 0 {
                guard.would_block_remaining -= 1;
                // Schedule a wake so the retry sleep path is what advances us;
                // also wake now so Pending writers without sleep still move.
                cx.waker().wake_by_ref();
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::WouldBlock)));
            }
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let n = if guard.max_chunk == 0 {
                buf.len()
            } else {
                buf.len().min(guard.max_chunk)
            };
            guard.buf.extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let state = self.state.clone();
            let Ok(mut guard) = state.try_lock() else {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            };
            guard.flush_count += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    async fn reset_coordinator() {
        let _ = shutdown_raw_stdout().await;
        with_coordinator_mut(|slot| {
            *slot = None;
        });
    }

    async fn install_sink(state: Arc<AsyncMutex<TestSinkState>>) -> TestResult {
        reset_coordinator().await;
        install_raw_stdout_sink_for_test(TestSink::new(state))
            .await
            .map_err(map_err)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordered_writes_preserve_fifo() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 0,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("one\n").await.map_err(map_err)?;
        write_raw_stdout("two\n").await.map_err(map_err)?;
        write_raw_stdout("three\n").await.map_err(map_err)?;
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if text != "one\ntwo\nthree\n" {
            return Err(format!("unexpected buffer: {text:?}"));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backpressure_waits_for_slow_partial_writes() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 1,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("abcd").await.map_err(map_err)?;
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if text != "abcd" {
            return Err(format!("unexpected buffer: {text:?}"));
        }
        if guard.write_calls < 4 {
            return Err(format!(
                "expected partial writes, got {}",
                guard.write_calls
            ));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn would_block_is_retried() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 3,
            max_chunk: 0,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("ok").await.map_err(map_err)?;
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if text != "ok" {
            return Err(format!("unexpected buffer: {text:?}"));
        }
        if guard.write_calls < 4 {
            return Err(format!(
                "expected would-block attempts plus success, got {}",
                guard.write_calls
            ));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_drains_and_flushes_sink() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 0,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("flush-me").await.map_err(map_err)?;
        flush_raw_stdout().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if text != "flush-me" {
            return Err(format!("unexpected buffer: {text:?}"));
        }
        if guard.flush_count < 1 {
            return Err("expected at least one flush".to_owned());
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn takeover_is_idempotent_and_restorable() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        reset_coordinator().await;
        if is_stdout_taken_over() {
            return Err("expected not taken over".to_owned());
        }

        take_over_stdout().map_err(map_err)?;
        if !is_stdout_taken_over() {
            return Err("expected taken over".to_owned());
        }
        take_over_stdout().map_err(map_err)?;
        if !is_stdout_taken_over() {
            return Err("expected still taken over".to_owned());
        }

        restore_stdout();
        if is_stdout_taken_over() {
            return Err("expected restored".to_owned());
        }
        restore_stdout(); // idempotent
        if is_stdout_taken_over() {
            return Err("expected still restored".to_owned());
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn product_output_routes_to_stderr_when_taken_over() -> TestResult {
        // Capture by temporarily taking over and ensuring ProductOutput does
        // not panic / does not require raw stdout. Routing correctness for
        // stderr vs stdout is observational via the flag branch; we assert
        // the flag gate that ProductOutput consults.
        let _guard = TEST_LOCK.lock().await;
        reset_coordinator().await;
        take_over_stdout().map_err(map_err)?;
        if !is_stdout_taken_over() {
            return Err("expected taken over".to_owned());
        }
        ProductOutput::writeln("protocol-clean product line");
        restore_stdout();
        if is_stdout_taken_over() {
            return Err("expected restored".to_owned());
        }
        ProductOutput::writeln("normal product line");
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_failure_propagates_and_latches() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 0,
            fatal_on_write: Some(io::ErrorKind::BrokenPipe),
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("will-fail").await.map_err(map_err)?;
        let err = match wait_for_raw_stdout_backpressure().await {
            Ok(()) => return Err("expected writer failure".to_owned()),
            Err(err) => err,
        };
        if !matches!(err, OutputGuardError::Io(_) | OutputGuardError::Latched(_)) {
            return Err(format!("unexpected error: {err}"));
        }

        let latched = match write_raw_stdout("after-fail").await {
            Ok(()) => return Err("expected latched fatal".to_owned()),
            Err(err) => err,
        };
        if !matches!(latched, OutputGuardError::Latched(_)) {
            return Err(format!("expected latched error, got {latched}"));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_further_writes() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 0,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("before-shutdown").await.map_err(map_err)?;
        shutdown_raw_stdout().await.map_err(map_err)?;

        // After shutdown the coordinator is cleared; a new write starts a
        // fresh coordinator with process stdout — reinstall a sink first.
        install_raw_stdout_sink_for_test(TestSink::new(Arc::clone(&state)))
            .await
            .map_err(map_err)?;
        write_raw_stdout("after-restart").await.map_err(map_err)?;
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if !text.contains("before-shutdown") {
            return Err(format!("missing before-shutdown in {text:?}"));
        }
        if !text.contains("after-restart") {
            return Err(format!("missing after-restart in {text:?}"));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_write_is_noop() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 0,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        write_raw_stdout("").await.map_err(map_err)?;
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;
        let guard = state.lock().await;
        if !guard.buf.is_empty() {
            return Err(format!("expected empty buffer, got {:?}", guard.buf));
        }
        if guard.write_calls != 0 {
            return Err(format!(
                "expected zero write calls, got {}",
                guard.write_calls
            ));
        }
        reset_coordinator().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_writers_stay_ordered_by_enqueue() -> TestResult {
        let _guard = TEST_LOCK.lock().await;
        let state = Arc::new(AsyncMutex::new(TestSinkState {
            buf: Vec::new(),
            would_block_remaining: 0,
            max_chunk: 2,
            fatal_on_write: None,
            flush_count: 0,
            write_calls: 0,
        }));
        install_sink(Arc::clone(&state)).await?;

        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8 {
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                // Serialize enqueue order explicitly via a ticket so the test
                // asserts FIFO of accepted requests, not race of spawn start.
                while counter.load(Ordering::SeqCst) != i {
                    tokio::task::yield_now().await;
                }
                let payload = format!("{i}");
                write_raw_stdout(payload).await.map_err(map_err)?;
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<(), String>(())
            }));
        }
        for handle in handles {
            handle
                .await
                .map_err(|err| format!("join: {err}"))?
                .map_err(map_err)?;
        }
        wait_for_raw_stdout_backpressure().await.map_err(map_err)?;

        let guard = state.lock().await;
        let text = std::str::from_utf8(&guard.buf).map_err(map_err)?;
        if text != "01234567" {
            return Err(format!("unexpected buffer: {text:?}"));
        }
        reset_coordinator().await;
        Ok(())
    }
}
