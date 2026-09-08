//! Vendored patch: typed, bounded capture of terminal query replies with a
//! narrow collection API.
//!
//! Upstream crossterm consumes no OSC color-query or cell-size replies: an
//! unsolicited `ESC ] 11 ; rgb:... BEL` background reply decodes as
//! `Alt+']'` plus ordinary character keys, leaking the payload into the
//! application's input stream. The patched unix byte parsers consume
//! recognized replies at the raw layer instead: a completed, within-bound
//! reply is pushed here rather than becoming an [`InternalEvent`], so it can
//! never surface as a public `Event` and can never accumulate in the
//! internal reader's event queues.
//!
//! Everything here is bounded: the reply sink holds at most
//! `REPLY_QUEUE_CAPACITY` replies (oldest dropped first) and each OSC 11
//! payload is bounded by [`OSC11_REPLY_PAYLOAD_LIMIT`], so replies nobody
//! observes cannot grow memory.
//!
//! [`poll_reply`] is the ONE collection path: it drives the same persistent
//! reader and parser that every other consumer uses, so replies collected
//! during startup probing, steady state, and mid-session requeries all share
//! one decoder — no raw takeover, no manual re-decode. Ordinary input
//! encountered while collecting stays queued in the shared reader and is
//! delivered by the regular event stream afterwards, in arrival order.
//!
//! Lock ordering: parser code runs while holding the internal event reader
//! mutex and may acquire the reply mutex; nothing here may acquire the
//! reader mutex while holding the reply mutex.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::event::timeout::PollTimeout;
#[cfg(unix)]
use crate::event::InternalEvent;
use crate::event::KeyboardEnhancementFlags;

/// Byte limit for one OSC 11 reply payload, shared by every consumer of the
/// reply grammar so all sides agree on when a sequence stops being a reply.
pub const OSC11_REPLY_PAYLOAD_LIMIT: usize = 64;

/// Maximum queued replies. Beyond this the oldest reply is dropped, keeping
/// memory bounded when no one drains.
const REPLY_QUEUE_CAPACITY: usize = 16;

/// A recognized terminal reply to a capability or status query.
///
/// OSC 11 and cell-size replies exist only in the sink (they have no public
/// crossterm `Event`); cursor position, keyboard enhancement flags, and
/// primary device attributes additionally exist as internal events and are
/// surfaced here with the same meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalReply {
    /// OSC 11 background-color payload (without OSC/ST framing).
    Osc11(String),
    /// Cell size in pixels: `CSI 4 ; height ; width t` (text-area) or
    /// `CSI 6 ; height ; width t` (cell) reply forms.
    CellSize {
        /// Cell width in pixels.
        width: u16,
        /// Cell height in pixels.
        height: u16,
    },
    /// Cursor position report `CSI row ; col R`, zero-based, mirroring
    /// `InternalEvent::CursorPosition` (column, row) ordering.
    CursorPosition {
        /// Zero-based terminal column.
        column: u16,
        /// Zero-based terminal row.
        row: u16,
    },
    /// Kitty progressive keyboard enhancement flags (`CSI ? flags u`).
    KeyboardEnhancementFlags(KeyboardEnhancementFlags),
    /// Primary device attributes (`CSI ? ... c`).
    PrimaryDeviceAttributes,
}

static REPLY_QUEUE: LazyLock<Mutex<VecDeque<TerminalReply>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(REPLY_QUEUE_CAPACITY)));

/// Bumped on every sink push; sources compare epochs around a read chunk to
/// detect "a reply completed inside this chunk" edge without polling.
static REPLY_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Store one completed reply (drop-oldest when full).
///
/// Called by the unix parsers while holding the internal event reader
/// mutex; must never acquire that lock itself.
fn push_reply(reply: TerminalReply) {
    {
        let mut queue = REPLY_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= REPLY_QUEUE_CAPACITY {
            queue.pop_front();
        }
        queue.push_back(reply);
    }
    REPLY_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Store one completed OSC 11 reply payload.
pub(crate) fn push_osc_11_reply(payload: String) {
    push_reply(TerminalReply::Osc11(payload));
}

/// Store one completed cell-size reply.
pub(crate) fn push_cell_size_reply(width: u16, height: u16) {
    push_reply(TerminalReply::CellSize { width, height });
}

/// Take the oldest queued reply, if any.
fn take_sink_reply() -> Option<TerminalReply> {
    REPLY_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop_front()
}

/// Current reply epoch. Sources snapshot it around a read chunk.
pub(crate) fn reply_epoch() -> u64 {
    REPLY_EPOCH.load(Ordering::Relaxed)
}

/// Drain replies consumed by the raw parser since the last drain, in
/// arrival order.
///
/// The queue is bounded (oldest dropped beyond capacity), so calling this is
/// an observation, not a liveness requirement. Prefer [`poll_reply`] in
/// application collection loops: it drives the shared reader instead of
/// racing it.
pub fn drain_replies() -> Vec<TerminalReply> {
    REPLY_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect()
}

/// Poll for the next recognized reply, driving the shared terminal reader.
///
/// This is the single collection path over the ONE persistent parser: call
/// it to await probe replies before the event stream starts, while it runs,
/// and after pausing it for a mid-session requery. The wait respects the
/// parser's escape-framing deadline internally, and replies split across
/// reads remain protocol state across repeated calls and timeouts.
///
/// Ordinary input seen while collecting is queued for the regular event
/// stream, in arrival order — it is never replayed, dropped, or re-decoded.
///
/// A recognized malformed or oversized reply latches a protocol error: this
/// call and subsequent reads return [`is_protocol_error`]-recognizable
/// errors until `recover_protocol_error` is called.
///
/// Returns `Ok(None)` on timeout, when the wait elapsed without a reply.
///
/// # Errors
///
/// Propagates reader failures, including the latched reply protocol error.
pub fn poll_reply(timeout: Option<Duration>) -> io::Result<Option<TerminalReply>> {
    let mut reader = super::lock_internal_event_reader();
    let wait = PollTimeout::new(timeout);
    loop {
        if let Some(reply) = take_sink_reply() {
            return Ok(Some(reply));
        }
        #[cfg(unix)]
        {
            if let Some(internal) = reader.take_queued_reply() {
                return Ok(Some(internal_event_to_reply(internal)));
            }
        }
        match reader.probe_try_read(wait.leftover()) {
            #[cfg(unix)]
            Ok(Some(internal)) => match internal {
                InternalEvent::CursorPosition(column, row) => {
                    return Ok(Some(TerminalReply::CursorPosition { column, row }));
                }
                InternalEvent::KeyboardEnhancementFlags(flags) => {
                    return Ok(Some(TerminalReply::KeyboardEnhancementFlags(flags)));
                }
                InternalEvent::PrimaryDeviceAttributes => {
                    return Ok(Some(TerminalReply::PrimaryDeviceAttributes));
                }
                // Ordinary input stays queued for the sole event stream.
                other => reader.queue_event(other),
            },
            #[cfg(not(unix))]
            Ok(Some(internal)) => reader.queue_event(internal),
            Ok(None) => {
                // Either the caller wait elapsed or a read chunk completed a
                // reply (the source returns early for that edge). Re-check
                // the sink before reporting an empty wait.
                if let Some(reply) = take_sink_reply() {
                    return Ok(Some(reply));
                }
                if wait.elapsed() {
                    return Ok(None);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn internal_event_to_reply(internal: InternalEvent) -> TerminalReply {
    match internal {
        InternalEvent::CursorPosition(column, row) => {
            TerminalReply::CursorPosition { column, row }
        }
        InternalEvent::KeyboardEnhancementFlags(flags) => {
            TerminalReply::KeyboardEnhancementFlags(flags)
        }
        InternalEvent::PrimaryDeviceAttributes => TerminalReply::PrimaryDeviceAttributes,
        InternalEvent::Event(_) | InternalEvent::ReplyConsumed => {
            unreachable!("only reply-class internal events reach this mapping")
        }
    }
}

/// Defined session recovery from a latched reply protocol error: clears the
/// error and any partial framing state so the session can resume decoding.
///
/// Returns `true` when a latch was present and has been cleared. Bytes the
/// malformed sequence already consumed were discarded with the error; bytes
/// the tty still holds decode fresh afterwards. Call only while no event
/// stream is polling (pause the input first) — same quiescence rule as the
/// rest of the recovery surface.
pub fn recover_protocol_error() -> bool {
    #[cfg(unix)]
    {
        // Bounded lock attempt: when an event stream is polling, the reader
        // mutex is held inside its poll and recovery must not block the
        // caller. Pause the stream first (its Drop joins the worker), then
        // call again.
        super::try_lock_internal_event_reader_for(std::time::Duration::from_millis(100))
            .map(|mut reader| reader.recover_protocol_error())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether an error is the latched reply-framing protocol error (recognized
/// malformed or oversized OSC 11 framing), as opposed to ordinary I/O.
pub fn is_protocol_error(error: &io::Error) -> bool {
    if error.kind() != io::ErrorKind::InvalidData {
        return false;
    }
    #[cfg(unix)]
    {
        error
            .to_string()
            .starts_with(crate::event::source::unix::parser::PROTOCOL_ERROR_PREFIX)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
pub(crate) fn lock_test_globals() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_is_bounded_drop_oldest() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        for i in 0..(REPLY_QUEUE_CAPACITY + 4) {
            push_osc_11_reply(format!("p{i}"));
        }
        let drained = drain_replies();
        assert_eq!(drained.len(), REPLY_QUEUE_CAPACITY);
        assert_eq!(
            drained.first(),
            Some(&TerminalReply::Osc11("p4".to_string())),
            "oldest replies are dropped first"
        );
        assert!(drain_replies().is_empty());
    }

    #[test]
    fn epoch_bumps_on_push_only() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let before = reply_epoch();
        push_cell_size_reply(9, 18);
        assert!(reply_epoch() > before);
    }
}
