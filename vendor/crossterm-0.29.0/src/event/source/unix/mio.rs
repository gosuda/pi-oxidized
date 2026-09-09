use std::io;
use std::time::{Duration, Instant};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, timeout::PollTimeout, Event, InternalEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

use super::parser::{min_duration, Parser};
#[cfg(not(feature = "libc"))]
use rustix::fd::AsFd;

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

/// Vendored patch: fairness bound for the edge-triggered drain. At most
/// this many tty reads happen between control-readiness checks (the
/// nonblocking wake/signal probe and the caller timeout), so a producer
/// that keeps the input queue replenished can never hold the poll loop
/// hostage.
const TTY_DRAIN_READ_BUDGET: usize = 4;

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_readable: bool,
    tty_fd: FileDesc<'static>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    /// Vendored patch: nonblocking control-fd probe used while the tty
    /// readable obligation is live. Never waits; retries EINTR like the
    /// blocking poll.
    fn poll_control_nonblocking(&mut self) -> io::Result<()> {
        loop {
            match self.poll.poll(&mut self.events, Some(Duration::ZERO)) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Vendored patch: SIGWINCH handling shared by the token loop and the
    /// drain window boundary.
    fn sigwinch_event(&mut self) -> io::Result<Option<InternalEvent>> {
        if self.signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
            // TODO Should we remove tput?
            //
            // This can take a really long time, because terminal::size can
            // launch new process (tput) and then it parses its output. It's
            // not a really long time from the origin point of view, but
            // it's a really long time from the mio, async-std/tokio executor, ...
            // point of view.
            let new_size = crate::terminal::size()?;
            return Ok(Some(InternalEvent::Event(Event::Resize(
                new_size.0, new_size.1,
            ))));
        }
        Ok(None)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_readable: false,
            tty_fd: input_fd,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

/// Vendored patch: authoritative "bytes queued on the tty input queue"
/// probe via `ioctl(fd, FIONREAD)`. The count is read without consuming
/// bytes and without touching shared file-status flags (no `O_NONBLOCK`
/// flip on the stdin-sharing fd), so a guarded read only runs when bytes
/// are actually queued — a blocking fd cannot block on such a read — and
/// a count of zero is a level-readiness drained proof.
///
/// `Ok(None)`: this fd does not support FIONREAD; callers fall back to the
/// upstream short-read heuristic, still bounded by the drain budget.
fn pending_input_bytes(fd: &FileDesc<'_>) -> io::Result<Option<usize>> {
    #[cfg(feature = "libc")]
    // SAFETY: `fd` is a live descriptor owned by the source for the call's
    // duration; `BorrowedFd` never closes it.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd.raw_fd()) };
    #[cfg(not(feature = "libc"))]
    let fd = fd.as_fd();
    Ok(rustix::io::ioctl_fionread(fd)
        .ok()
        .map(|count| count as usize))
}

/// Vendored patch: the canonical wake interruption error, shared by the
/// token loop and the drain window boundary.
#[cfg(feature = "event-stream")]
fn wake_interrupted() -> io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "Poll operation was woken up by `Waker::wake`",
    )
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        // Vendored patch: a latched reply protocol error surfaces on every
        // poll until recovery; nothing is read or decoded while latched.
        if let Some(error) = self.parser.protocol_error() {
            return Err(error);
        }
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            // Vendored patch: an escape-framing candidate whose absolute
            // deadline passed decodes into ordinary keys here — even when the
            // caller waits longer, because the poll below is deadline-bounded.
            let now = Instant::now();
            if self.parser.expire_due(now) {
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
            }

            // Vendored patch: the effective wait honors an earlier parser
            // deadline inside any caller wait (indefinite included).
            let wait = min_duration(timeout.leftover(), self.parser.deadline_leftover(now));

            // Vendored patch (ET fairness): the tty registration is edge-
            // triggered. While the persisted "tty still readable" obligation
            // is live, its unread bytes carry no new edge, so a blocking ET
            // poll here could sleep past them indefinitely. Probe the
            // control fds NONBLOCKING instead and resume the bounded drain
            // below; the blocking poll runs only once the obligation was
            // cleared on an authoritative drained proof.
            if self.tty_readable {
                self.poll_control_nonblocking()?;
            } else if let Err(e) = self.poll.poll(&mut self.events, wait) {
                // Mio will throw an interrupted error in case of cursor position retrieval. We need to retry until it succeeds.
                // Previous versions of Mio (< 0.7) would automatically retry the poll call if it was interrupted (if EINTR was returned).
                // https://docs.rs/mio/0.7.0/mio/struct.Poll.html#notes
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    return Err(e);
                }
            }

            if !self.tty_readable && self.events.is_empty() {
                // No readiness events = the effective wait elapsed: caller
                // timeout or candidate deadline. Surface deadline keys before
                // reporting an empty wait.
                if self.parser.expire_due(Instant::now()) {
                    if let Some(event) = self.parser.next() {
                        return Ok(Some(event));
                    }
                }
                if timeout.elapsed() {
                    return Ok(None);
                }
                continue;
            }

            // Vendored patch (ET fairness): latch the obligation for EVERY
            // fresh tty edge in this batch BEFORE any early return — a
            // consumed-but-undrained ET edge never re-arms, so discarding a
            // batch that contains TTY_TOKEN would strand unread bytes until
            // the next arrival.
            if self.events.iter().any(|event| event.token() == TTY_TOKEN) {
                self.tty_readable = true;
            }

            let tokens: Vec<Token> = self.events.iter().map(|x| x.token()).collect();
            for token in tokens {
                match token {
                    TTY_TOKEN => {
                        // Vendored patch (ET fairness): the edge only latches
                        // the persisted readable obligation (done above, for
                        // the whole batch, before any early return); the
                        // bounded readiness-guarded drain runs after the
                        // token loop, so a wake or signal observed in the
                        // SAME batch is honored before more tty work. The
                        // old queue-bounded-drain claim here was false: the
                        // input queue bounds occupancy, not cumulative
                        // refilling input, so a producer that kept bytes
                        // queued could hold the old drain loop hostage and
                        // the wake token was only observed after it.
                    }
                    SIGNAL_TOKEN => {
                        if let Some(event) = self.sigwinch_event()? {
                            return Ok(Some(event));
                        }
                    }
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => {
                        return Err(wake_interrupted());
                    }
                    _ => unreachable!("Synchronize Evented handle registration & token handling"),
                }
            }
            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }

            // Vendored patch (ET fairness): bounded, readiness-guarded
            // drain. It runs whenever the obligation is live — on a fresh
            // edge or resumed from an earlier return. Every read is guarded
            // by FIONREAD, the authoritative queued-byte count: a blocking
            // fd cannot block on a guarded read, and zero is a
            // level-readiness drained proof. Control fds are re-probed
            // nonblocking at every window boundary, so a pending wake is
            // honored within TTY_DRAIN_READ_BUDGET reads even under a
            // sustained reply flood, and the caller timeout bounds the
            // drain across windows. The obligation persists across every
            // return until FIONREAD == 0, WouldBlock, or EOF proves the
            // queue drained; the next call resumes without waiting for a
            // fresh edge. A short read is never treated as a drained proof
            // while bytes remain queued.
            if self.tty_readable {
                let reply_epoch_before = crate::event::reply::reply_epoch();
                let mut reads_since_control_check = 0usize;
                while self.tty_readable {
                    if reads_since_control_check == TTY_DRAIN_READ_BUDGET {
                        reads_since_control_check = 0;
                        // Control-readiness check with wake priority.
                        self.poll_control_nonblocking()?;
                        let tokens: Vec<Token> =
                            self.events.iter().map(|event| event.token()).collect();
                        for token in tokens {
                            match token {
                                TTY_TOKEN => {} // more input arrived; the obligation already covers it
                                SIGNAL_TOKEN => {
                                    if let Some(event) = self.sigwinch_event()? {
                                        return Ok(Some(event));
                                    }
                                }
                                #[cfg(feature = "event-stream")]
                                WAKE_TOKEN => {
                                    return Err(wake_interrupted());
                                }
                                _ => unreachable!(
                                    "Synchronize Evented handle registration & token handling"
                                ),
                            }
                        }
                        if timeout.elapsed() {
                            break;
                        }
                        if crate::event::reply::reply_epoch() != reply_epoch_before {
                            // A reply completed inside this window: return
                            // promptly so reply consumers observe it without
                            // waiting out the caller's timeout.
                            break;
                        }
                    }

                    // Authoritative pending-byte proof. `None`: this fd
                    // cannot report FIONREAD; fall back to the upstream
                    // short-read heuristic, still bounded by the window
                    // budget and the control checks above.
                    let pending = pending_input_bytes(&self.tty_fd)?;
                    if pending == Some(0) {
                        // Level-readiness drained proof at this instant;
                        // any later arrival raises a fresh ET edge.
                        self.tty_readable = false;
                        break;
                    }
                    match self.tty_fd.read(&mut self.tty_buffer) {
                        Ok(0) => {
                            // EOF: nothing further will arrive on this fd.
                            self.tty_readable = false;
                            break;
                        }
                        Ok(read_count) => {
                            // `more` mirrors the upstream `input_available`
                            // semantics; with a pending count it is exact.
                            let more = match pending {
                                Some(queued) => queued > read_count,
                                None => read_count == TTY_BUFFER_SIZE,
                            };
                            self.parser.advance(
                                &self.tty_buffer[..read_count],
                                more,
                                Instant::now(),
                            );
                            reads_since_control_check += 1;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // Authoritative drained proof on a nonblocking fd.
                            self.tty_readable = false;
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                    if let Some(error) = self.parser.protocol_error() {
                        // The obligation persists: bytes may still be queued
                        // for the poll after a defined recovery.
                        return Err(error);
                    }
                    if let Some(event) = self.parser.next() {
                        return Ok(Some(event));
                    }
                }
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if let Some(error) = self.parser.protocol_error() {
                    return Err(error);
                }
                if crate::event::reply::reply_epoch() != reply_epoch_before {
                    // A reply completed inside this drain: return promptly
                    // so reply consumers observe it without waiting out the
                    // caller's timeout.
                    return Ok(None);
                }
            }
        }
    }

    /// Vendored patch: defined recovery from a latched reply protocol error.
    fn recover_protocol_error(&mut self) -> bool {
        self.parser.recover()
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::reply::{self, drain_replies, lock_test_globals, TerminalReply};
    use crate::event::KeyCode;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    /// Build the source-side file descriptor from a socket end. Both
    /// configs take ownership: under the `libc` feature the raw fd is
    /// adopted (closed when the `FileDesc` drops), otherwise the stream
    /// becomes the `OwnedFd`. The source must outlive no borrowed fd.
    #[cfg(feature = "libc")]
    fn source_fd(read_end: UnixStream) -> FileDesc<'static> {
        use std::os::unix::io::IntoRawFd;
        FileDesc::new(read_end.into_raw_fd(), true)
    }

    #[cfg(not(feature = "libc"))]
    fn source_fd(read_end: UnixStream) -> FileDesc<'static> {
        FileDesc::Owned(std::os::fd::OwnedFd::from(read_end))
    }
    /// A source reading from one end of a socket pair; the test writes into
    /// the other end. Both ends stay owned here for the whole test.
    fn pipe_source() -> (UnixInternalEventSource, UnixStream) {
        let (read_end, write_end) = UnixStream::pair().unwrap();
        let source = UnixInternalEventSource::from_file_descriptor(source_fd(read_end)).unwrap();
        (source, write_end)
    }
    fn key_of(event: &InternalEvent) -> KeyCode {
        match event {
            InternalEvent::Event(Event::Key(key)) => key.code,
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn split_reply_across_reads_is_sunk_without_keys() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();

        writer.write_all(b"\x1b]").unwrap();
        // Held: neither Esc nor `]` keys may leak while the candidate is
        // alive, and the short caller wait expires normally.
        assert!(source
            .try_read(Some(Duration::from_millis(30)))
            .unwrap()
            .is_none());

        writer.write_all(b"11;rgb:00/00/00\x07").unwrap();
        // The chunk completing the reply returns promptly (no keys), and the
        // reply is observable on the typed sink.
        assert!(source
            .try_read(Some(Duration::from_millis(200)))
            .unwrap()
            .is_none());
        assert!(drain_replies().contains(&TerminalReply::Osc11("rgb:00/00/00".to_string())));
    }

    #[test]
    fn lone_esc_expires_within_long_caller_wait() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();

        writer.write_all(b"\x1b").unwrap();
        let started = Instant::now();
        let event = source
            .try_read(Some(Duration::from_millis(500)))
            .unwrap()
            .expect("the framing deadline must produce the Esc key");
        let elapsed = started.elapsed();
        assert_eq!(key_of(&event), KeyCode::Esc);
        assert!(
            elapsed >= Duration::from_millis(40) && elapsed <= Duration::from_millis(200),
            "Esc must surface at the 50 ms deadline, not at the 500 ms caller wait; took {elapsed:?}"
        );
    }

    #[test]
    fn payload_overflow_latches_error_and_recovers() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();

        writer.write_all(b"\x1b]11;").unwrap();
        assert!(source
            .try_read(Some(Duration::from_millis(30)))
            .unwrap()
            .is_none());

        let mut chunk = vec![0x1b, b']', b'1', b'1', b';'];
        chunk.extend(std::iter::repeat_n(b'x', 70));
        writer.write_all(&chunk).unwrap();
        let error = source
            .try_read(Some(Duration::from_millis(200)))
            .expect_err("oversized recognized framing must be an explicit error");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(reply::is_protocol_error(&error));
        // The latch persists across polls...
        assert!(reply::is_protocol_error(
            &source.try_read(Some(Duration::ZERO)).unwrap_err()
        ));
        // ...until defined recovery, after which decoding resumes fresh.
        assert!(source.recover_protocol_error());
        writer.write_all(b"a").unwrap();
        let event = source
            .try_read(Some(Duration::from_millis(200)))
            .unwrap()
            .expect("input must decode after recovery");
        assert_eq!(key_of(&event), KeyCode::Char('a'));
    }

    #[test]
    fn ordinary_keys_still_flow_between_reply_polls() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();

        writer.write_all(b"ab").unwrap();
        assert_eq!(
            key_of(
                &source
                    .try_read(Some(Duration::from_millis(200)))
                    .unwrap()
                    .unwrap()
            ),
            KeyCode::Char('a')
        );
        assert_eq!(
            key_of(
                &source
                    .try_read(Some(Duration::from_millis(200)))
                    .unwrap()
                    .unwrap()
            ),
            KeyCode::Char('b')
        );
        assert!(drain_replies().is_empty());
    }
    #[cfg(feature = "event-stream")]
    #[test]
    fn wake_is_drained_and_does_not_poison_later_polls() {
        let (mut source, _writer) = pipe_source();
        source.waker().wake().unwrap();
        // The first poll observes the wake...
        let error = source
            .try_read(Some(Duration::from_millis(200)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        // ...and once consumed, later polls must NOT see the stale wake:
        // they block out the caller wait instead of returning instantly.
        let started = Instant::now();
        assert!(source
            .try_read(Some(Duration::from_millis(150)))
            .unwrap()
            .is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "a drained wake must not keep interrupting later polls; returned after {:?}",
            started.elapsed()
        );
    }
    /// One complete OSC 11 reply: 18 bytes. Deterministic generator for the
    /// flood fixtures; reply boundaries span read-chunk boundaries.
    #[cfg(feature = "event-stream")]
    fn osc11_reply_bytes(count: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(count * 18);
        for i in 0..count {
            bytes.extend_from_slice(
                format!("\x1b]11;rgb:{:02x}/00/00\x07", (i % 256) as u8).as_bytes(),
            );
        }
        bytes
    }

    /// Vendored patch regression: a sustained, continuously replenished
    /// reply flood must not hold the drain hostage. The wake — requested as
    /// soon as the first full 1,024-byte read was consumed — must be
    /// acknowledged within the drain read budget while the producer is
    /// STILL WRITING, and every byte queued before the wake must survive
    /// the boundary. The wall-clock watchdog is cleanup-only evidence; the
    /// budget assertions are the real proof.
    #[cfg(feature = "event-stream")]
    #[test]
    fn sustained_reply_flood_acks_wake_within_read_budget() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;
        use std::thread;

        // Keep in sync with the source's TTY_DRAIN_READ_BUDGET.
        const DRAIN_BUDGET_READS: usize = 4;

        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();

        // Controlled start: a static burst of 1,600 replies (28,800 bytes,
        // about 29 consecutive full reads) — far beyond the drain budget.
        writer.write_all(&osc11_reply_bytes(1_600)).unwrap();

        // Producer: keeps the input queue replenished until the wake is
        // acknowledged, so the drain never sees a momentary empty queue to
        // mistake for a drained proof. Its socket end is private to this
        // thread; its nonblocking flag never reaches the source fd.
        let acked = Arc::new(AtomicBool::new(false));
        let producer = {
            let acked = acked.clone();
            writer.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                let reply = osc11_reply_bytes(1);
                loop {
                    if acked.load(Ordering::SeqCst) {
                        return;
                    }
                    match writer.write_all(&reply) {
                        Ok(()) => thread::yield_now(),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        {
            let acked = acked.clone();
            thread::spawn(move || {
                for _ in 0..1_000 {
                    if acked.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                std::process::exit(101);
            });
        }

        // Wake exactly after the first full read: 57 replies exceed
        // 1,024 consumed bytes.
        let wake_fired_epoch = Arc::new(AtomicU64::new(0));
        let waker = source.waker();
        {
            let wake_fired_epoch = wake_fired_epoch.clone();
            thread::spawn(move || {
                while reply::reply_epoch() < 57 {
                    thread::sleep(Duration::from_millis(1));
                }
                wake_fired_epoch.store(reply::reply_epoch(), Ordering::SeqCst);
                waker.wake().unwrap();
            });
        }

        let started = Instant::now();
        let error = loop {
            match source.try_read(Some(Duration::from_millis(100))) {
                Err(error) => break error,
                Ok(_) => {}
            }
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::Interrupted,
            "the pending wake must be acknowledged, not drained past"
        );
        let ack_elapsed = started.elapsed();

        acked.store(true, Ordering::SeqCst);
        producer.join().unwrap();

        // (1) Acknowledged within the declared read budget, measured in
        // replies consumed after the wake fired. The bound covers the one
        // budget window possibly still in flight when the wake fired, the
        // acknowledgment window, and the watcher's snapshot slack.
        let consumed_since_wake =
            (reply::reply_epoch() - wake_fired_epoch.load(Ordering::SeqCst)) as usize;
        let budget_replies = (DRAIN_BUDGET_READS * 2 + 2) * TTY_BUFFER_SIZE / 18 + 2;
        assert!(
            consumed_since_wake <= budget_replies,
            "wake ack consumed {consumed_since_wake} replies; budget is {budget_replies}"
        );
        assert!(
            ack_elapsed < Duration::from_secs(5),
            "wake ack took {ack_elapsed:?}"
        );

        // (2) With the producer stopped, every remaining queued byte is
        // consumed WITHOUT any new input edge, and nothing is lost.
        loop {
            let before = reply::reply_epoch();
            match source.try_read(Some(Duration::from_millis(100))) {
                Ok(Some(_)) => {}
                Ok(None) if reply::reply_epoch() == before => break,
                Ok(None) => {}
                Err(e) => panic!("unexpected error draining the flood remainder: {e}"),
            }
        }
        assert!(
            reply::reply_epoch() >= 1_600,
            "every burst reply must reach the sink across the wake boundary (got {})",
            reply::reply_epoch()
        );

        // (3) Drained proof: the next poll blocks out the caller wait
        // (obligation cleared on an authoritative drained condition)
        // instead of spinning or stranding state.
        let started = Instant::now();
        assert!(source
            .try_read(Some(Duration::from_millis(120)))
            .unwrap()
            .is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "a drained source must block out the wait, not spin: {:?}",
            started.elapsed()
        );
    }

    /// Vendored patch regression: bytes queued before a wake — including
    /// the final ordinary key — must be consumed afterwards WITHOUT a
    /// fresh input edge. This is the lost-edge guard: a pseudo-fix that
    /// breaks after one chunk and returns to an indefinite ET poll would
    /// strand the queued tail (its edge was already consumed) and never
    /// deliver `z`.
    #[cfg(feature = "event-stream")]
    #[test]
    fn queued_tail_after_wake_is_consumed_without_new_edge() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let (mut source, mut writer) = pipe_source();
        let epoch_base = reply::reply_epoch();

        // 300 replies (5,400 bytes, about six full reads) then one ordinary
        // key, all in one burst: many consecutive full reads with no writer
        // activity afterwards.
        let mut burst = osc11_reply_bytes(300);
        burst.push(b'z');
        writer.write_all(&burst).unwrap();

        // Window 1: the drain consumes its full-read budget, then returns
        // promptly (a reply completed inside the window) with the readable
        // obligation still live.
        assert!(source
            .try_read(Some(Duration::from_millis(200)))
            .unwrap()
            .is_none());
        assert_eq!(
            reply::reply_epoch() - epoch_base,
            ((TTY_BUFFER_SIZE * 4) / 18) as u64,
            "window 1 must consume exactly the budget's full reads"
        );

        // Wake while the tail is still queued.
        source.waker().wake().unwrap();
        let error = source
            .try_read(Some(Duration::from_millis(200)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);

        // From here: NO new input. The queued tail — including the final
        // ordinary key — must still be consumed.
        let mut saw_z = false;
        for _ in 0..4 {
            match source.try_read(Some(Duration::from_millis(100))).unwrap() {
                Some(InternalEvent::Event(Event::Key(key))) => {
                    assert_eq!(key.code, KeyCode::Char('z'));
                    saw_z = true;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(
            saw_z,
            "the queued final key must be consumed without a new edge"
        );
        assert_eq!(
            reply::reply_epoch() - epoch_base,
            300,
            "every pre-wake reply must reach the sink"
        );

        // Drained proof: the next poll waits out the caller timeout.
        let started = Instant::now();
        assert!(source
            .try_read(Some(Duration::from_millis(120)))
            .unwrap()
            .is_none());
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    /// Vendored patch regression: a short read is NOT a drained proof while
    /// bytes remain queued. SOCK_SEQPACKET delivers one packet per read
    /// even with more packets queued — the tty-shaped short-read-with-
    /// pending case: the old loop stopped at the first short read and,
    /// with the ET edge already consumed, stranded every remaining packet
    /// until new input arrived.
    #[cfg(feature = "libc")]
    #[test]
    fn short_read_with_pending_bytes_keeps_draining() {
        use std::os::unix::io::FromRawFd;

        let _guard = lock_test_globals();
        let _ = drain_replies();
        let epoch_base = reply::reply_epoch();
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: plain socketpair(2); both fds land in owned UnixStreams.
        let ret = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(ret, 0, "socketpair failed: {}", io::Error::last_os_error());
        let read_end = unsafe { UnixStream::from_raw_fd(fds[0]) };
        let mut write_end = unsafe { UnixStream::from_raw_fd(fds[1]) };
        let mut source =
            UnixInternalEventSource::from_file_descriptor(source_fd(read_end)).unwrap();

        // 40 packets of two replies each: every read is short (36 bytes)
        // while packets stay queued.
        for _ in 0..40 {
            write_end.write_all(&osc11_reply_bytes(2)).unwrap();
        }

        for _ in 0..16 {
            match source.try_read(Some(Duration::from_millis(50))) {
                Ok(Some(_)) | Ok(None) => {}
                Err(e) => panic!("unexpected drain error: {e}"),
            }
        }
        assert_eq!(
            reply::reply_epoch() - epoch_base,
            80,
            "all queued packets must drain past short reads"
        );
        // Drained: the next poll waits out the caller wait.
        let started = Instant::now();
        assert!(source
            .try_read(Some(Duration::from_millis(80)))
            .unwrap()
            .is_none());
        assert!(started.elapsed() >= Duration::from_millis(60));
    }
}
