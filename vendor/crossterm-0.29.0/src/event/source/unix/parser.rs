//! Vendored patch: the ONE persistent byte parser shared by both unix event
//! sources (`mio` and `use-dev-tty`).
//!
//! The parser owns every byte the tty produces for the lifetime of the
//! process: startup probing, steady-state streaming, mid-session requeries,
//! and pause/resume handoffs all observe the same instance, so no reply
//! prefix, partial CSI, UTF-8 sequence, or bracketed paste ever loses its
//! context across an ownership boundary. There is no raw takeover and no
//! manual re-decode path anywhere else.
//!
//! ## Escape framing contract (approved 50 ms policy)
//!
//! A lone `ESC` is ambiguous: it may open any ordinary key sequence, or the
//! `ESC ] 1 1 ;` header of an OSC 11 background-color reply. The parser holds
//! such a *candidate* under an absolute deadline of [`ESCAPE_FRAMING_DEADLINE`],
//! measured from the first `ESC` byte and never reset by later bytes:
//!
//! * Before the full `ESC ] 1 1 ;` header is recognized, a candidate that
//!   diverges from the reply grammar resolves immediately, and an expired
//!   candidate resolves at the deadline, into the exact ordinary key sequence
//!   decoded by the existing `parse_event` decoder — each held byte decoded
//!   exactly once (`ESC` → `Esc`, `ESC ]` → `Alt+]`, longer held prefixes
//!   additionally decode their `1`/`;` suffix bytes as ordinary keys).
//! * Once the full header is recognized, the framing deadline is removed:
//!   payload and split `ST` fragments remain protocol state across arbitrary
//!   idle gaps, consumer timeouts, and ownership handoffs.
//! * A recognized-but-malformed or oversized reply (an `ESC` inside the
//!   payload not followed by `\`, or more than
//!   [`crate::event::reply::OSC11_REPLY_PAYLOAD_LIMIT`] payload bytes before
//!   a terminator) latches an explicit [`io::Error`] on the parser: polls
//!   fail with it, no payload byte is ever replayed as keys, and the latched
//!   state persists until [`Parser::recover`] is called (defined session
//!   recovery).
use crate::event::InternalEvent;
use crate::event::sys::unix::parse::parse_event;
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

/// Absolute deadline for an unresolved escape-framing candidate, measured
/// from the first `ESC` byte. Explicitly not reset by later candidate bytes.
pub(crate) const ESCAPE_FRAMING_DEADLINE: Duration = Duration::from_millis(50);

/// Length of the full OSC 11 reply header `ESC ] 1 1 ;`.
const OSC11_HEADER_LEN: usize = 5;

/// Latched-error message prefix; `crate::event::reply::is_protocol_error`
/// recognizes it so application layers can distinguish reply framing
/// failures from ordinary I/O errors.
pub(crate) const PROTOCOL_ERROR_PREFIX: &str = "crossterm reply protocol error: ";

const MALFORMED_FRAMING_MSG: &str =
    "crossterm reply protocol error: malformed OSC 11 reply framing (unterminated ST)";
const OVERSIZED_PAYLOAD_MSG: &str =
    "crossterm reply protocol error: OSC 11 reply payload exceeded the 64-byte limit";

/// The persistent byte parser. See the module documentation for the framing
/// contract.
#[derive(Debug)]
pub(crate) struct Parser {
    /// Upstream partial-sequence buffer (CSI/SS3/UTF-8/paste continuations).
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    /// Ambiguous `ESC`-opening candidate, if one is in flight.
    candidate: Option<Candidate>,
    /// Latched protocol error message; set by recognized malformed/oversized
    /// reply framing and cleared only by [`Parser::recover`].
    protocol_error: Option<&'static str>,
}

/// One in-flight escape-framing candidate.
#[derive(Debug)]
struct Candidate {
    /// Held bytes: `ESC`, or `ESC ] 1 1 ;` plus the reply payload.
    bytes: Vec<u8>,
    /// `Some` while the candidate is ambiguous (header incomplete); `None`
    /// once the full OSC 11 header is recognized — payload framing has no
    /// deadline and waits for its terminator across arbitrary timeouts.
    deadline: Option<Instant>,
}

/// Where a re-fed byte must be processed next.
enum Route {
    Done,
    /// Continue through the candidate state machine.
    Candidate(u8, bool),
    /// Continue through the plain sequence buffer.
    Buffer(u8, bool),
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger, because when you look at the
            // `try_read` method implementation, all events are consumed before
            // the next TTY_BUFFER is processed -> events pushed.
            internal_events: VecDeque::with_capacity(128),
            candidate: None,
            protocol_error: None,
        }
    }
}

impl Parser {
    /// Advance the parse state with one read chunk. `more` mirrors the
    /// upstream `input_available` semantics: whether further bytes of this
    /// read chunk follow. `now` is the chunk arrival instant, used to arm
    /// the absolute framing deadline (injected so tests can pin boundaries).
    pub(crate) fn advance(&mut self, buffer: &[u8], more: bool, now: Instant) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;
            self.advance_one(*byte, more, now);
        }
    }

    /// Advance the parse state by one byte, re-feeding diverging input
    /// through the correct state machine iteratively (no recursion; each
    /// re-feed either resolves a candidate or grows one within its grammar,
    /// so the loop is bounded by the chunk length).
    fn advance_one(&mut self, byte: u8, more: bool, now: Instant) {
        let mut route = Route::Candidate(byte, more);
        loop {
            if self.protocol_error.is_some() {
                return;
            }
            route = match route {
                Route::Done => return,
                Route::Candidate(byte, more) => self.advance_candidate(byte, more),
                Route::Buffer(byte, more) => self.advance_buffer(byte, more, now),
            };
        }
    }

    /// Process one byte through the candidate state machine.
    fn advance_candidate(&mut self, byte: u8, more: bool) -> Route {
        let mut candidate = match self.candidate.take() {
            Some(candidate) => candidate,
            None => return Route::Buffer(byte, more),
        };

        if candidate.deadline.is_none() {
            return self.absorb_payload_byte(candidate, byte);
        }

        // Header phase: the candidate is ambiguous between an ordinary key
        // sequence and the OSC 11 reply header.
        if candidate.bytes.len() == 1 {
            if byte == b']' {
                candidate.bytes.push(byte);
                self.candidate = Some(candidate);
                return Route::Done;
            }
            // Not a reply introducer: upstream decode. Hand the held `ESC`
            // to the plain buffer and process the byte there so sequences
            // like `ESC [` or `ESC x` keep their exact upstream meaning.
            self.buffer.push(0x1b);
            return Route::Buffer(byte, more);
        }

        let expected = match candidate.bytes.len() {
            2 => b'1',
            3 => b'1',
            _ => b';',
        };
        if byte == expected {
            candidate.bytes.push(byte);
            if byte == b';' {
                // Full `ESC ] 1 1 ;` header recognized: the framing deadline
                // is cancelled; payload fragments are protocol from here on.
                candidate.deadline = None;
            }
            self.candidate = Some(candidate);
            return Route::Done;
        }

        // Diverged pre-header: resolve the held bytes as the exact ordinary
        // key sequence (each byte through the existing decoder exactly once),
        // then process the diverging byte normally.
        self.resolve_candidate_as_keys(&candidate.bytes);
        Route::Buffer(byte, more)
    }

    /// Process one payload-phase byte of an active candidate. The candidate
    /// has been taken out of `self`; this method reinserts or drops it.
    fn absorb_payload_byte(&mut self, mut candidate: Candidate, byte: u8) -> Route {
        // ST lookahead state first: once an `ESC` is held inside the payload,
        // only `\` completes it and every other byte is malformed framing.
        if candidate.bytes.last() == Some(&0x1b) {
            if byte == b'\\' {
                candidate.bytes.pop();
                let payload = payload_of(&candidate.bytes);
                crate::event::reply::push_osc_11_reply(payload);
                return Route::Done;
            }
            // ESC followed by anything but the ST final byte is recognized
            // framing that cannot complete: explicit error, never keys.
            self.protocol_error = Some(MALFORMED_FRAMING_MSG);
            return Route::Done;
        }
        // Terminators complete regardless of the payload limit: framing is
        // checked before the bound.
        if byte == 0x07 {
            let payload = payload_of(&candidate.bytes);
            crate::event::reply::push_osc_11_reply(payload);
            return Route::Done;
        }
        if byte == 0x1b {
            // Possible ST introducer; hold it as one-byte framing lookahead.
            candidate.bytes.push(byte);
            self.candidate = Some(candidate);
            return Route::Done;
        }
        if candidate.bytes.len() - OSC11_HEADER_LEN
            >= crate::event::reply::OSC11_REPLY_PAYLOAD_LIMIT
        {
            // The next byte would exceed the payload bound: recognized
            // framing that grew past its limit is an explicit error.
            self.protocol_error = Some(OVERSIZED_PAYLOAD_MSG);
            return Route::Done;
        }
        candidate.bytes.push(byte);
        self.candidate = Some(candidate);
        Route::Done
    }

    /// Process one byte through the upstream partial-sequence buffer.
    fn advance_buffer(&mut self, byte: u8, more: bool, now: Instant) -> Route {
        // A lone `ESC` is ambiguous (any key sequence or a reply header):
        // hold it as a candidate under the absolute framing deadline,
        // measured from this ESC byte's arrival and never reset.
        if byte == 0x1b && self.buffer.is_empty() {
            self.candidate = Some(Candidate {
                bytes: vec![0x1b],
                deadline: Some(now + ESCAPE_FRAMING_DEADLINE),
            });
            return Route::Done;
        }
        self.buffer.push(byte);
        match parse_event(&self.buffer, more) {
            Ok(Some(InternalEvent::ReplyConsumed)) => {
                // Consumed by the raw reply layer (e.g. a cell-size reply
                // pushed to the typed sink); nothing surfaces as an event.
                self.buffer.clear();
            }
            Ok(Some(ie)) => {
                self.internal_events.push_back(ie);
                self.buffer.clear();
            }
            Ok(None) => {
                // Event can't be parsed, because we don't have enough bytes for
                // the current sequence. Keep the buffer and process next bytes.
            }
            Err(_) => {
                // Event can't be parsed (not enough parameters, parameter is
                // not a number, ...). Clear the buffer and continue with
                // another sequence.
                self.buffer.clear();
            }
        }
        Route::Done
    }

    /// Decode held candidate bytes as the exact ordinary key sequence via the
    /// existing `parse_event` decoder, each byte exactly once.
    fn resolve_candidate_as_keys(&mut self, held: &[u8]) {
        match held.len() {
            1 => {
                if let Ok(Some(ie)) = parse_event(held, false) {
                    self.internal_events.push_back(ie);
                }
            }
            len => {
                // `ESC ]` introducer decodes upstream-exact as Alt+']'.
                if let Ok(Some(ie)) = parse_event(&held[..2], false) {
                    self.internal_events.push_back(ie);
                }
                // Held header suffix bytes (`1`, `;`) decode as ordinary keys.
                for &byte in &held[2..len] {
                    if let Ok(Some(ie)) = parse_event(&[byte], false) {
                        self.internal_events.push_back(ie);
                    }
                }
            }
        }
    }

    /// Expire an unresolved pre-header candidate whose absolute deadline has
    /// passed. Returns `true` when a candidate was resolved into key events.
    pub(crate) fn expire_due(&mut self, now: Instant) -> bool {
        let Some(candidate) = self.candidate.take() else {
            return false;
        };
        let Some(deadline) = candidate.deadline else {
            // Payload-phase candidates have no deadline; put it back.
            self.candidate = Some(candidate);
            return false;
        };
        if now < deadline {
            self.candidate = Some(candidate);
            return false;
        }
        self.resolve_candidate_as_keys(&candidate.bytes);
        true
    }

    /// Remaining time until the candidate deadline, if one is pending.
    pub(crate) fn deadline_leftover(&self, now: Instant) -> Option<Duration> {
        self.candidate
            .as_ref()
            .and_then(|candidate| candidate.deadline)
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// A fresh error when the parser is latched by a recognized malformed or
    /// oversized reply. The latch persists until [`Parser::recover`].
    pub(crate) fn protocol_error(&self) -> Option<io::Error> {
        self.protocol_error
            .map(|message| io::Error::new(io::ErrorKind::InvalidData, message))
    }

    /// Defined session recovery: clear a latched protocol error and any
    /// partial framing state. Returns `true` when a latch was present.
    ///
    /// Bytes already consumed by the malformed sequence are discarded with
    /// the error; bytes the tty still holds are decoded fresh afterwards.
    pub(crate) fn recover(&mut self) -> bool {
        let was_poisoned = self.protocol_error.take().is_some();
        self.candidate = None;
        self.buffer.clear();
        was_poisoned
    }
}

/// The shorter of two optional waits; `None` means "no bound on this side".
/// Both unix sources use it to honor a parser deadline inside any caller
/// wait.
pub(crate) fn min_duration(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (None, None) => None,
        (None, Some(d)) | (Some(d), None) => Some(d),
        (Some(a), Some(b)) => Some(a.min(b)),
    }
}

fn payload_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[OSC11_HEADER_LEN..]).into_owned()
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, KeyCode, KeyModifiers};
    use crate::event::reply::{TerminalReply, drain_replies, lock_test_globals};

    /// Drain parser events as `Event`s.
    fn events(parser: &mut Parser) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(InternalEvent::Event(event)) = parser.next() {
            out.push(event);
        }
        assert!(
            parser.next().is_none(),
            "parser must not hold non-event internal events in these tests"
        );
        out
    }

    fn key_code(event: &Event) -> KeyCode {
        match event {
            Event::Key(key) => key.code,
            other => panic!("expected key event, got {other:?}"),
        }
    }

    fn key_modifiers(event: &Event) -> KeyModifiers {
        match event {
            Event::Key(key) => key.modifiers,
            other => panic!("expected key event, got {other:?}"),
        }
    }

    fn base() -> Instant {
        // Tests inject fully controlled instants; the parser never reads the
        // clock itself.
        Instant::now()
    }

    #[test]
    fn lone_esc_expires_to_esc_exactly_at_deadline() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b", false, t0);
        assert!(events(&mut parser).is_empty());
        // One nanosecond before the deadline: not yet.
        assert!(!parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE - Duration::from_nanos(1)));
        // Equal deadline: explicit boundary, must expire.
        assert!(parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE));
        assert_eq!(events(&mut parser).len(), 1);
        assert!(drain_replies().is_empty());
    }

    #[test]
    fn deadline_leftover_is_absolute_from_first_esc() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b", false, t0);
        assert_eq!(parser.deadline_leftover(t0), Some(ESCAPE_FRAMING_DEADLINE));
        // A later header byte must NOT reset the absolute deadline.
        parser.advance(b"]", false, t0 + Duration::from_millis(20));
        assert_eq!(
            parser.deadline_leftover(t0 + Duration::from_millis(20)),
            Some(Duration::from_millis(30))
        );
    }

    #[test]
    fn esc_bracket_expires_to_alt_bracket_exactly_once() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b", false, t0);
        parser.advance(b"]", false, t0 + Duration::from_millis(10));
        assert!(parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE));
        let decoded = events(&mut parser);
        assert_eq!(decoded.len(), 1);
        assert_eq!(key_code(&decoded[0]), KeyCode::Char(']'));
        assert!(key_modifiers(&decoded[0]).contains(KeyModifiers::ALT));
        // Exactly once: a second expiry is a no-op.
        assert!(!parser.expire_due(t0 + Duration::from_secs(10)));
        assert!(events(&mut parser).is_empty());
    }

    #[test]
    fn esc_bracket_one_expires_to_alt_bracket_then_one() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]1", false, t0);
        assert!(parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Char(']'), KeyCode::Char('1')]);
    }

    #[test]
    fn diverging_header_resolves_immediately_without_waiting() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]", false, t0);
        // `2` diverges from the OSC 11 grammar: immediate resolution, no 50ms.
        parser.advance(b"2", false, t0 + Duration::from_millis(1));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Char(']'), KeyCode::Char('2')]);
        // The candidate is gone; no late deadline keys can appear.
        assert!(parser.deadline_leftover(t0 + Duration::from_millis(1)).is_none());
        assert!(!parser.expire_due(t0 + Duration::from_secs(60)));
    }

    #[test]
    fn esc_with_plain_continuation_keeps_upstream_decode() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        // Coalesced `ESC x` must stay Alt+x, not Esc + x.
        parser.advance(b"\x1bx", false, t0);
        let decoded = events(&mut parser);
        assert_eq!(decoded.len(), 1);
        assert_eq!(key_code(&decoded[0]), KeyCode::Char('x'));
        assert!(key_modifiers(&decoded[0]).contains(KeyModifiers::ALT));
    }

    #[test]
    fn esc_bracket_keys_still_reach_stream_after_expiry() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        // `ESC ]` expires into Alt+]; the user then really types `x`, which
        // must arrive as its own key afterwards.
        parser.advance(b"\x1b", false, t0);
        parser.advance(b"]", false, t0 + Duration::from_millis(10));
        assert!(parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE));
        parser.advance(b"x", false, t0 + Duration::from_millis(60));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Char(']'), KeyCode::Char('x')]);
    }

    #[test]
    fn arrow_keys_still_decode_across_splits() {
        let _guard = lock_test_globals();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b", false, t0);
        parser.advance(b"[", false, t0 + Duration::from_millis(5));
        parser.advance(b"B", false, t0 + Duration::from_millis(10));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Down]);
    }

    #[test]
    fn completed_header_cancels_deadline_and_carries_split_payload() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]11;", false, t0);
        // Header recognized: no deadline remains.
        assert_eq!(parser.deadline_leftover(t0), None);
        assert!(!parser.expire_due(t0 + Duration::from_secs(3600)));
        // Payload split across arbitrary idle gaps still completes.
        parser.advance(b"rgb:1c/", false, t0 + Duration::from_secs(3600));
        parser.advance(b"1c/1c\x07", false, t0 + Duration::from_secs(7200));
        assert!(events(&mut parser).is_empty());
        assert_eq!(
            drain_replies(),
            vec![TerminalReply::Osc11("rgb:1c/1c/1c".to_string())]
        );
    }

    #[test]
    fn payload_split_st_lookahead_completes() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]11;rgb:00/00/00", false, t0);
        parser.advance(b"\x1b", false, t0 + Duration::from_millis(10));
        parser.advance(b"\\", false, t0 + Duration::from_millis(20));
        assert!(events(&mut parser).is_empty());
        assert_eq!(
            drain_replies(),
            vec![TerminalReply::Osc11("rgb:00/00/00".to_string())]
        );
    }

    #[test]
    fn malformed_st_framing_errors_explicitly_not_replay() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]11;rgb\x1bX", false, t0);
        assert!(
            events(&mut parser).is_empty(),
            "malformed recognized framing must never replay payload bytes as keys"
        );
        let error = parser.protocol_error().expect("parser must be latched");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().starts_with(PROTOCOL_ERROR_PREFIX));
        // The latch persists until recovery.
        assert!(parser.protocol_error().is_some());
        assert!(parser.recover());
        assert!(parser.protocol_error().is_none());
        // Ordinary input decodes fresh after recovery.
        parser.advance(b"a", false, t0);
        assert_eq!(events(&mut parser).len(), 1);
    }

    #[test]
    fn malformed_esc_bel_under_lookahead_errors_not_completes() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        // An ESC lookahead followed by BEL is unterminated framing, not a
        // BEL-terminated reply carrying the lookahead as payload.
        parser.advance(b"\x1b]11;rgb\x1b\x07", false, t0);
        assert!(events(&mut parser).is_empty());
        assert!(drain_replies().is_empty());
        assert!(parser.protocol_error().is_some());
        parser.recover();
    }

    #[test]
    fn payload_bounds_63_64_65_are_consistent() {
        let _guard = lock_test_globals();
        for (payload_len, expect_reply) in [(63usize, true), (64, true), (65, false)] {
            let _ = drain_replies();
            let mut parser = Parser::default();
            let t0 = base();
            parser.advance(b"\x1b]11;", false, t0);
            let payload = vec![b'x'; payload_len];
            parser.advance(&payload, false, t0);
            parser.advance(b"\x07", false, t0);
            assert_eq!(
                drain_replies().len(),
                usize::from(expect_reply),
                "payload length {payload_len} must be {}",
                if expect_reply { "accepted" } else { "rejected with a latch" }
            );
            if expect_reply {
                assert!(parser.protocol_error().is_none());
            } else {
                assert!(
                    parser.protocol_error().is_some(),
                    "65 payload bytes must latch the oversized error"
                );
                parser.recover();
            }
            assert!(events(&mut parser).is_empty());
        }
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn bracketed_paste_with_osc_like_text_stays_literal() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b[200~done\x1b]11;? rgb\x07\x1b[201~", false, t0);
        let pasted = events(&mut parser);
        assert_eq!(pasted.len(), 1);
        assert!(
            drain_replies().is_empty(),
            "paste text containing OSC-like bytes is not a reply"
        );
    }

    #[test]
    fn full_reply_in_one_chunk_is_sunk_without_keys() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        parser.advance(b"\x1b]11;rgb:00/00/00\x07", false, t0);
        assert!(events(&mut parser).is_empty());
        assert_eq!(
            drain_replies(),
            vec![TerminalReply::Osc11("rgb:00/00/00".to_string())]
        );
    }

    #[test]
    fn byte_at_a_time_header_partitions_reach_the_sink() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        for (idx, chunk) in [b"\x1b".as_slice(), b"]", b"1", b"1", b";"]
            .into_iter()
            .enumerate()
        {
            parser.advance(chunk, false, t0 + Duration::from_millis(idx as u64));
        }
        // No deadline keys between partitions: each partition arrives well
        // inside the 50 ms window in this test.
        parser.advance(b"rgb:0", false, t0 + Duration::from_millis(5));
        parser.advance(b"0/00/00\x07", false, t0 + Duration::from_millis(10));
        assert!(events(&mut parser).is_empty());
        assert_eq!(
            drain_replies(),
            vec![TerminalReply::Osc11("rgb:00/00/00".to_string())]
        );
    }

    #[test]
    fn expiry_between_partitions_decodes_prefix_once_then_suffix_arrives() {
        let _guard = lock_test_globals();
        let _ = drain_replies();
        let mut parser = Parser::default();
        let t0 = base();
        // `ESC` in read 1; nothing for 50 ms; the rest of a real reply never
        // comes — the candidate expires into keys exactly once.
        parser.advance(b"\x1b", false, t0);
        assert!(parser.expire_due(t0 + ESCAPE_FRAMING_DEADLINE));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Esc]);
        // A later `]` is an ordinary key, not part of the expired candidate.
        parser.advance(b"]", false, t0 + Duration::from_millis(100));
        let codes: Vec<KeyCode> = events(&mut parser).iter().map(key_code).collect();
        assert_eq!(codes, vec![KeyCode::Char(']')]);
    }
}
