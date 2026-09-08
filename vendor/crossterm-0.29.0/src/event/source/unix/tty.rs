use std::io;
use std::os::unix::net::UnixStream;
#[cfg(feature = "libc")]
use std::os::unix::prelude::AsRawFd;
use std::time::{Duration, Instant};

#[cfg(not(feature = "libc"))]
use rustix::fd::{AsFd, AsRawFd};

use signal_hook::low_level::pipe;

use crate::event::timeout::PollTimeout;
use crate::event::Event;
use filedescriptor::{poll, pollfd, POLLIN};

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, InternalEvent};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

use super::parser::{min_duration, Parser};

/// Holds a prototypical Waker and a receiver we can wait on when doing select().
#[cfg(feature = "event-stream")]
struct WakePipe {
    receiver: UnixStream,
    waker: Waker,
}

#[cfg(feature = "event-stream")]
impl WakePipe {
    fn new() -> io::Result<Self> {
        let (receiver, sender) = nonblocking_unix_pair()?;
        Ok(WakePipe {
            receiver,
            waker: Waker::new(sender),
        })
    }
}

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: FileDesc<'static>,
    winch_signal_receiver: UnixStream,
    #[cfg(feature = "event-stream")]
    wake_pipe: WakePipe,
}

fn nonblocking_unix_pair() -> io::Result<(UnixStream, UnixStream)> {
    let (receiver, sender) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((receiver, sender))
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        Ok(UnixInternalEventSource {
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty: input_fd,
            winch_signal_receiver: {
                let (receiver, sender) = nonblocking_unix_pair()?;
                // Unregistering is unnecessary because EventSource is a singleton
                #[cfg(feature = "libc")]
                pipe::register(libc::SIGWINCH, sender)?;
                #[cfg(not(feature = "libc"))]
                pipe::register(rustix::process::Signal::WINCH.as_raw(), sender)?;
                receiver
            },
            #[cfg(feature = "event-stream")]
            wake_pipe: WakePipe::new()?,
        })
    }
}

/// read_complete reads from a non-blocking file descriptor
/// until the buffer is full or it would block.
///
/// Similar to `std::io::Read::read_to_end`, except this function
/// only fills the given buffer and does not read beyond that.
fn read_complete(fd: &FileDesc, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match fd.read(buf) {
            Ok(x) => return Ok(x),
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(0),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            },
        }
    }
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

        fn make_pollfd<F: AsRawFd>(fd: &F) -> pollfd {
            pollfd {
                fd: fd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            }
        }

        #[cfg(not(feature = "event-stream"))]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
        ];

        #[cfg(feature = "event-stream")]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
            make_pollfd(&self.wake_pipe.receiver),
        ];

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

            match poll(&mut fds, wait) {
                Err(filedescriptor::Error::Poll(e)) | Err(filedescriptor::Error::Io(e)) => {
                    match e.kind() {
                        // retry on EINTR
                        io::ErrorKind::Interrupted => continue,
                        _ => return Err(e),
                    }
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("got unexpected error while polling: {e:?}"),
                    ))
                }
                Ok(_) => (),
            };

            if fds[0].revents & POLLIN != 0 {
                // Vendored patch: ONE bounded read per poll iteration.
                // Level-triggered readiness re-reports pending bytes, so
                // draining still completes — while the extra poll between
                // chunks keeps wake/shutdown honored under a reply flood
                // instead of starving inside a drain loop.
                let reply_epoch_before = crate::event::reply::reply_epoch();
                let read_count = read_complete(&self.tty, &mut self.tty_buffer)?;
                if read_count > 0 {
                    self.parser.advance(
                        &self.tty_buffer[..read_count],
                        read_count == TTY_BUFFER_SIZE,
                        Instant::now(),
                    );
                }

                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if let Some(error) = self.parser.protocol_error() {
                    return Err(error);
                }
                if crate::event::reply::reply_epoch() != reply_epoch_before {
                    // A reply completed inside this chunk: return promptly so
                    // reply consumers observe it without waiting out the
                    // caller's timeout.
                    return Ok(None);
                }
            }
            if fds[1].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.winch_signal_receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.winch_signal_receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}
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

            #[cfg(feature = "event-stream")]
            if fds[2].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.wake_pipe.receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.wake_pipe.receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}

                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }

            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    /// Vendored patch: defined recovery from a latched reply protocol error.
    fn recover_protocol_error(&mut self) -> bool {
        self.parser.recover()
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.wake_pipe.waker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::reply::{self, drain_replies, lock_test_globals, TerminalReply};
    use crate::event::KeyCode;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    /// Transfer ownership of the socket read end to either descriptor backend.
    #[cfg(feature = "libc")]
    fn source_fd(read_end: UnixStream) -> FileDesc<'static> {
        use std::os::fd::IntoRawFd;
        FileDesc::new(read_end.into_raw_fd(), true)
    }

    #[cfg(not(feature = "libc"))]
    fn source_fd(read_end: UnixStream) -> FileDesc<'static> {
        FileDesc::Owned(std::os::fd::OwnedFd::from(read_end))
    }

    /// Each returned object owns one end of the socket pair.
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
}
