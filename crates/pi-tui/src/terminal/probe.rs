//! Capability / cursor probe session with fragmented-reply handling.

#[cfg(unix)]
use std::io::Read;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::component::UiEvent;
use crate::terminal::caps::{CellDimensions, TerminalCapabilities};

/// Fragment timeout for split capability replies (TS: 150 ms).
pub const PROBE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);

/// Wait bounding the FIRST reply byte after a probe query write.
///
/// [`PROBE_FRAGMENT_TIMEOUT`] protects replies split across reads and stays
/// the budget once a reply stream exists; a terminal that has sent nothing
/// cannot hold a fragment in flight, so a silent terminal ends the probe wait
/// here instead of billing the full fragment budget (the first-frame lane is
/// otherwise charged 150 ms on every non-responding terminal). Round-trip
/// class per the R9 floor (~1 ms pipe RT) with scheduler-jitter headroom.
pub const PROBE_FIRST_BYTE_TIMEOUT: Duration = Duration::from_millis(25);

/// Poll-slice bound honored once a reply stream exists AND the owner armed
/// the yield flag: the collector must hand stdin back within a few
/// milliseconds of the arm, not at the next full-budget deadline. Slicing is
/// event-driven (each slice is a poll that wakes on bytes), so a quiet
/// responding terminal costs at most a handful of extra wakeups.
const PROBE_YIELD_POLL_SLICE: Duration = Duration::from_millis(3);

/// Terminal background polarity used by automatic theme selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTheme {
    /// A dark terminal background.
    Dark,
    /// A light terminal background.
    Light,
}

/// Probe batch written outside synchronized output before `EventStream` starts.
#[must_use]
pub fn probe_query_batch(include_cell_size: bool) -> Vec<u8> {
    let mut out = Vec::new();
    // Kitty progressive enhancement: push desired flags + query + DA1 sentinel.
    // Flags 7 = disambiguate | event types | alternate keys.
    out.extend_from_slice(b"\x1b[>1u\x1b[?u\x1b[c");
    if include_cell_size {
        out.extend_from_slice(b"\x1b[16t");
    }
    // OSC 11 background query.
    out.extend_from_slice(b"\x1b]11;?\x07");
    // Cursor position report.
    out.extend_from_slice(b"\x1b[6n");
    out
}

/// OSC 11 background query only (mid-session re-probe; no DA1/cursor batch).
#[must_use]
pub fn osc_11_query() -> &'static [u8] {
    b"\x1b]11;?\x07"
}

/// Classify dark-background from collected probe replies, if any OSC 11 landed.
#[must_use]
pub fn background_from_replies(replies: &[ProbeReply]) -> Option<bool> {
    replies.iter().find_map(|reply| match reply {
        ProbeReply::Background(payload) => classify_background(payload),
        _ => None,
    })
}

/// Select terminal polarity from an OSC 11 classification, `COLORFGBG`, or the
/// conservative dark fallback, in that order.
#[must_use]
pub fn detect_terminal_theme(osc_dark: Option<bool>, colorfgbg: Option<&str>) -> TerminalTheme {
    if let Some(dark) = osc_dark {
        return if dark {
            TerminalTheme::Dark
        } else {
            TerminalTheme::Light
        };
    }

    colorfgbg
        .and_then(colorfgbg_background_index)
        .map_or(TerminalTheme::Dark, |index| {
            if ansi256_luminance(index) >= 128 {
                TerminalTheme::Light
            } else {
                TerminalTheme::Dark
            }
        })
}

/// Write the startup probe batch (phase 1 of the startup probe).
///
/// Returns `false` when stdin is not a terminal — no batch is written and
/// the matching [`probe_collect_replies`] call completes immediately.
/// Written outside synchronized output, before
/// [`crate::terminal::TerminalInput`] takes ownership of stdin.
///
/// # Errors
///
/// Returns [`io::Error`] when writing or flushing the probe batch fails.
pub fn probe_write_batch<W: Write>(output: &mut W) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }

    output.write_all(&probe_query_batch(true))?;
    output.flush()?;
    Ok(true)
}

/// Collect the startup probe replies written by [`probe_write_batch`]
/// (phase 2), merging recognized replies into `caps` and returning early
/// keystrokes as UI events for re-injection.
///
/// Blocks for the two-phase reply budget (see [`collect_probe_replies`]) —
/// at most [`PROBE_FIRST_BYTE_TIMEOUT`] on a silent terminal — so callers
/// that painted a first frame speculatively during this window re-derive
/// theme and capability state afterwards and repaint when it changed.
///
/// # Errors
///
/// Returns [`io::Error`] when reading stdin fails.
pub fn probe_collect_replies(caps: &mut TerminalCapabilities) -> io::Result<Vec<UiEvent>> {
    probe_collect_replies_with_yield(caps, &AtomicBool::new(false))
}

/// Yield-aware variant of [`probe_collect_replies`]: when `yield_now` is
/// armed, the collector stops reading within [`PROBE_YIELD_POLL_SLICE`] once
/// a reply stream exists and returns the bytes it already consumed as early
/// input. Callers that must take stdin back by a deadline (the runtime arms
/// this right before painting the first frame) guarantee the EventStream
/// parser owns stdin from that point onward, so input written at
/// first-paint time is parsed by crossterm instead of re-injected through
/// the lossy startup mapper.
///
/// # Errors
///
/// Returns [`io::Error`] when reading stdin fails.
pub fn probe_collect_replies_with_yield(
    caps: &mut TerminalCapabilities,
    yield_now: &AtomicBool,
) -> io::Result<Vec<UiEvent>> {
    let mut session = ProbeSession::new();
    let mut pending = Vec::new();
    collect_probe_replies(&mut session, &mut pending, ProbeSession::is_complete, yield_now)?;
    pending.extend(session.flush_timeout());
    session.apply_to(caps);
    Ok(reinject_bytes_as_events(&pending))
}

/// Shared probe wait: merge stdin bytes into `session` under the two-phase
/// reply budget, returning interleaved non-probe input through `pending`.
///
/// Before the first reply byte the wait is bounded by
/// [`PROBE_FIRST_BYTE_TIMEOUT`]; after bytes start flowing the full
/// [`PROBE_FRAGMENT_TIMEOUT`] window applies (measured from the query write),
/// so fragmented replies keep today's acceptance budget. Readiness is
/// event-driven: the wait blocks in `poll` until bytes arrive or the active
/// budget expires, with no fixed tick — except while `yield_now` is armed
/// with a reply stream present, where polls are sliced to
/// [`PROBE_YIELD_POLL_SLICE`] so the owner gets stdin back promptly.
///
/// `complete` is the caller's collected-enough predicate (full probe set or
/// a classified background).
fn collect_probe_replies(
    session: &mut ProbeSession,
    pending: &mut Vec<u8>,
    complete: impl Fn(&ProbeSession) -> bool,
    yield_now: &AtomicBool,
) -> io::Result<()> {
    let fragment_deadline = Instant::now() + PROBE_FRAGMENT_TIMEOUT;
    let first_byte_deadline = Instant::now() + PROBE_FIRST_BYTE_TIMEOUT;
    let mut reply_seen = false;
    loop {
        if complete(session) || (reply_seen && yield_now.load(Ordering::Relaxed)) {
            return Ok(());
        }
        let active_deadline = if reply_seen {
            fragment_deadline
        } else {
            first_byte_deadline
        };
        let Some(remaining) = active_deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        let wait = if reply_seen && yield_now.load(Ordering::Relaxed) {
            remaining.min(PROBE_YIELD_POLL_SLICE)
        } else {
            remaining
        };
        match read_stdin_within(wait)? {
            // EOF: stdin closed, no reply can arrive.
            Some(bytes) if bytes.is_empty() => return Ok(()),
            Some(bytes) => {
                if let ProbeFeed::PendingInput(bytes) = session.feed(&bytes) {
                    pending.extend(bytes);
                }
                // Arm the fragment phase only on probe-reply evidence: a
                // recognized reply or a buffered partial sequence. Ordinary
                // early keystrokes must not extend the wait.
                reply_seen = !session.replies().is_empty() || !session.buffer.is_empty();
            }
            // Readiness timeout: the active budget expired.
            None => return Ok(()),
        }
    }
}

/// Mid-session OSC 11 re-probe: emit only the background query and parse a
/// bounded reply. Non-probe stdin bytes are returned for re-injection.
///
/// Call only while the sole [`crate::terminal::TerminalInput`] `EventStream`
/// is paused so this path owns stdin. `None` means timeout / no-TTY / unparseable
/// — the caller keeps its prior classification.
///
/// # Errors
///
/// Returns [`io::Error`] when writing or flushing the query fails.
pub fn probe_background<W: Write>(output: &mut W) -> io::Result<(Option<bool>, Vec<UiEvent>)> {
    if !io::stdin().is_terminal() {
        return Ok((None, Vec::new()));
    }

    output.write_all(osc_11_query())?;
    output.flush()?;

    let mut session = ProbeSession::new();
    let mut pending = Vec::new();
    collect_probe_replies(
        &mut session,
        &mut pending,
        |session| background_from_replies(session.replies()).is_some(),
        &AtomicBool::new(false),
    )?;
    let dark = background_from_replies(session.replies());
    Ok((dark, reinject_bytes_as_events(&pending)))
}

/// Drive a mid-session OSC 11 classification from canned stdin chunks.
///
/// Processes every chunk, then treats any incomplete fragment as user input
/// (timeout path). Used by unit tests and fakes that cannot touch real stdin.
#[must_use]
pub fn probe_background_from_chunks(
    chunks: impl IntoIterator<Item = impl AsRef<[u8]>>,
) -> (Option<bool>, Vec<UiEvent>) {
    let mut session = ProbeSession::new();
    let mut pending = Vec::new();
    for chunk in chunks {
        if let ProbeFeed::PendingInput(bytes) = session.feed(chunk.as_ref()) {
            pending.extend(bytes);
        }
    }
    let dark = background_from_replies(session.replies());
    pending.extend(session.flush_timeout());
    (dark, reinject_bytes_as_events(&pending))
}

/// Outcome of feeding bytes into a [`ProbeSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFeed {
    /// Bytes consumed as part of an in-progress or completed probe reply.
    Consumed,
    /// Non-probe input that should be re-injected after the stream starts.
    PendingInput(Vec<u8>),
}

/// One recognized probe reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeReply {
    /// Kitty keyboard flags response `CSI ? <flags> u`.
    KittyFlags(u16),
    /// Primary Device Attributes `CSI ? ... c`.
    DeviceAttributes,
    /// Cell size `CSI 4 ; height ; width t` or `CSI 16 t` reply form `CSI 6 ; h ; w t`.
    CellSize {
        /// Cell width in pixels.
        width: u16,
        /// Cell height in pixels.
        height: u16,
    },
    /// OSC 11 background color payload (without OSC/ST framing).
    Background(String),
    /// Cursor position `CSI row ; col R` (1-based).
    CursorPosition {
        /// One-based terminal row.
        row: u16,
        /// One-based terminal column.
        col: u16,
    },
}

/// Stateful parser for probe replies interleaved with early keystrokes.
#[derive(Debug, Default)]
pub struct ProbeSession {
    buffer: Vec<u8>,
    replies: Vec<ProbeReply>,
    pending_input: Vec<u8>,
    saw_kitty: bool,
    saw_da1: bool,
    saw_cursor: bool,
}

impl ProbeSession {
    /// Create an empty probe session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw terminal bytes; returns whether they were probe data or input.
    pub fn feed(&mut self, bytes: &[u8]) -> ProbeFeed {
        if bytes.is_empty() {
            return ProbeFeed::Consumed;
        }
        self.buffer.extend_from_slice(bytes);
        self.drain_buffer();
        if self.pending_input.is_empty() {
            ProbeFeed::Consumed
        } else {
            let pending = std::mem::take(&mut self.pending_input);
            ProbeFeed::PendingInput(pending)
        }
    }

    /// Force incomplete fragments to be treated as user input (timeout path).
    pub fn flush_timeout(&mut self) -> Vec<u8> {
        if !self.buffer.is_empty() {
            self.pending_input.extend_from_slice(&self.buffer);
            self.buffer.clear();
        }
        std::mem::take(&mut self.pending_input)
    }

    /// Apply collected replies onto a capability cache and optional cursor.
    pub fn apply_to(&self, caps: &mut TerminalCapabilities) -> Option<(u16, u16)> {
        let mut cursor = None;
        for reply in &self.replies {
            match reply {
                ProbeReply::KittyFlags(flags) => {
                    caps.set_kitty_keyboard(*flags != 0);
                }
                ProbeReply::DeviceAttributes => {
                    if !self.saw_kitty {
                        caps.set_kitty_keyboard(false);
                    }
                }
                ProbeReply::CellSize { width, height } => {
                    caps.set_cell_dimensions(*width, *height);
                }
                ProbeReply::Background(payload) => {
                    caps.set_dark_background(classify_background(payload));
                }
                ProbeReply::CursorPosition { row, col } => {
                    cursor = Some((col.saturating_sub(1), row.saturating_sub(1)));
                }
            }
        }
        cursor
    }

    /// Collected replies in arrival order.
    #[must_use]
    pub fn replies(&self) -> &[ProbeReply] {
        &self.replies
    }

    /// Pending non-probe bytes waiting for re-injection.
    #[must_use]
    pub fn pending_input(&self) -> &[u8] {
        &self.pending_input
    }

    /// Convert pending raw input into synthetic UI events where possible.
    #[must_use]
    pub fn pending_ui_events(&self) -> Vec<UiEvent> {
        reinject_bytes_as_events(&self.pending_input)
    }

    /// Whether the session has enough replies to stop waiting.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        // DA1 is the sentinel; cursor is also required for cache seeding.
        (self.saw_da1 || self.saw_kitty) && self.saw_cursor
    }

    fn drain_buffer(&mut self) {
        loop {
            if self.buffer.is_empty() {
                break;
            }
            match take_one(&mut self.buffer) {
                TakeResult::NeedMore => break,
                TakeResult::Reply(reply) => {
                    match &reply {
                        ProbeReply::KittyFlags(_) => self.saw_kitty = true,
                        ProbeReply::DeviceAttributes => self.saw_da1 = true,
                        ProbeReply::CursorPosition { .. } => self.saw_cursor = true,
                        _ => {}
                    }
                    self.replies.push(reply);
                }
                TakeResult::Input(bytes) => self.pending_input.extend_from_slice(&bytes),
            }
        }
    }
}

enum TakeResult {
    NeedMore,
    Reply(ProbeReply),
    Input(Vec<u8>),
}

fn take_one(buffer: &mut Vec<u8>) -> TakeResult {
    if buffer.is_empty() {
        return TakeResult::NeedMore;
    }

    // OSC 11 reply: ESC ] 11 ; ... BEL or ST
    if buffer.starts_with(b"\x1b]11;") {
        return take_osc_11(buffer);
    }

    // CSI sequences: ESC [
    if buffer.starts_with(b"\x1b[") {
        return take_csi(buffer);
    }

    // Bare ESC that may still be a prefix.
    if buffer == b"\x1b" || buffer == b"\x1b]" {
        return TakeResult::NeedMore;
    }

    // Not a probe sequence: emit the first byte as input and continue.
    let byte = buffer.remove(0);
    TakeResult::Input(vec![byte])
}

fn take_osc_11(buffer: &mut Vec<u8>) -> TakeResult {
    // Find BEL (0x07) or ST (ESC \)
    let mut i = 5; // after ESC ] 11 ;
    while i < buffer.len() {
        if buffer[i] == 0x07 {
            let payload = buffer[5..i].to_vec();
            let _ = buffer.drain(..=i);
            let text = String::from_utf8_lossy(&payload).into_owned();
            return TakeResult::Reply(ProbeReply::Background(text));
        }
        if buffer[i] == 0x1b {
            if i + 1 >= buffer.len() {
                return TakeResult::NeedMore;
            }
            if buffer[i + 1] == b'\\' {
                let payload = buffer[5..i].to_vec();
                let _ = buffer.drain(..i + 2);
                let text = String::from_utf8_lossy(&payload).into_owned();
                return TakeResult::Reply(ProbeReply::Background(text));
            }
        }
        i += 1;
    }
    TakeResult::NeedMore
}

fn take_csi(buffer: &mut Vec<u8>) -> TakeResult {
    // Need at least ESC [ + final byte.
    if buffer.len() < 3 {
        return TakeResult::NeedMore;
    }
    // Find final byte 0x40-0x7E.
    let mut idx = 2;
    while idx < buffer.len() {
        let b = buffer[idx];
        if (0x40..=0x7e).contains(&b) {
            let seq = buffer[..=idx].to_vec();
            let _ = buffer.drain(..=idx);
            if let Some(reply) = parse_csi_reply(&seq) {
                return TakeResult::Reply(reply);
            }
            // Unknown CSI: treat as input so it can be re-injected.
            return TakeResult::Input(seq);
        }
        idx += 1;
    }
    // Still a valid CSI prefix?
    if is_csi_prefix(buffer) {
        TakeResult::NeedMore
    } else {
        let byte = buffer.remove(0);
        TakeResult::Input(vec![byte])
    }
}

fn parse_csi_reply(seq: &[u8]) -> Option<ProbeReply> {
    let body = seq.strip_prefix(b"\x1b[")?;
    if body.is_empty() {
        return None;
    }
    let final_byte = *body.last()?;
    let params = &body[..body.len() - 1];

    match final_byte {
        b'u' => {
            // CSI ? <flags> u
            if let Some(rest) = params.strip_prefix(b"?") {
                let flags = std::str::from_utf8(rest).ok()?.parse::<u16>().ok()?;
                return Some(ProbeReply::KittyFlags(flags));
            }
            None
        }
        b'c' => {
            // CSI ? ... c  (DA1)
            if params.starts_with(b"?") {
                return Some(ProbeReply::DeviceAttributes);
            }
            None
        }
        b't' => {
            // CSI 6 ; height ; width t  (cell size reply for 16t)
            // Also accept CSI 4 ; height ; width t
            let text = std::str::from_utf8(params).ok()?;
            let mut parts = text.split(';');
            let kind = parts.next()?;
            if kind == "6" || kind == "4" {
                let height = parts.next()?.parse::<u16>().ok()?;
                let width = parts.next()?.parse::<u16>().ok()?;
                return Some(ProbeReply::CellSize { width, height });
            }
            None
        }
        b'R' => {
            // CSI row ; col R  (optionally with ? prefix for some terminals)
            let text = std::str::from_utf8(params).ok()?;
            let text = text.strip_prefix('?').unwrap_or(text);
            let mut parts = text.split(';');
            let row = parts.next()?.parse::<u16>().ok()?;
            let col = parts.next()?.parse::<u16>().ok()?;
            Some(ProbeReply::CursorPosition { row, col })
        }
        _ => None,
    }
}

fn is_csi_prefix(buf: &[u8]) -> bool {
    if !buf.starts_with(b"\x1b[") {
        return buf == b"\x1b";
    }
    // Intermediate bytes until a final is seen.
    buf.iter().skip(2).all(|b| *b < 0x40 || *b > 0x7e)
}

/// Classify an OSC 11 payload into dark/light when possible.
#[must_use]
pub fn classify_background(payload: &str) -> Option<bool> {
    // Forms: rgb:RR/GG/BB, #RRGGBB, #RRRRGGGGBBBB
    let rgb = parse_background_rgb(payload)?;
    let luminance =
        (u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000;
    Some(luminance < 128)
}

fn colorfgbg_background_index(colorfgbg: &str) -> Option<u8> {
    colorfgbg
        .split(';')
        .rev()
        .find_map(|part| part.trim().parse::<u8>().ok())
}

fn ansi256_luminance(index: u8) -> u8 {
    let [red, green, blue] = ansi256_rgb(index);
    let weighted = (u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000;
    u8::try_from(weighted).unwrap_or(u8::MAX)
}

fn ansi256_rgb(index: u8) -> [u8; 3] {
    const ANSI16: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match index {
        0..=15 => ANSI16[usize::from(index)],
        16..=231 => {
            let offset = index - 16;
            let channel = |value| if value == 0 { 0 } else { value * 40 + 55 };
            [
                channel(offset / 36),
                channel((offset / 6) % 6),
                channel(offset % 6),
            ]
        }
        232..=255 => {
            // index - 232 ∈ 0..=23, so the ramp value 8..=238 always fits u8.
            let gray = u8::try_from(u16::from(index - 232) * 10 + 8).unwrap_or(u8::MAX);
            [gray, gray, gray]
        }
    }
}

fn parse_background_rgb(payload: &str) -> Option<[u8; 3]> {
    let payload = payload.trim();
    if let Some(rest) = payload.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r = parse_hex_component(parts.next()?)?;
        let g = parse_hex_component(parts.next()?)?;
        let b = parse_hex_component(parts.next()?)?;
        return Some([r, g, b]);
    }
    if let Some(rest) = payload.strip_prefix('#') {
        match rest.len() {
            6 => {
                let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
                let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
                let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
                return Some([r, g, b]);
            }
            12 => {
                let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
                let g = u8::from_str_radix(&rest[4..6], 16).ok()?;
                let b = u8::from_str_radix(&rest[8..10], 16).ok()?;
                return Some([r, g, b]);
            }
            _ => {}
        }
    }
    None
}

fn parse_hex_component(component: &str) -> Option<u8> {
    if component.is_empty() || component.len() > 4 {
        return None;
    }
    let value = u16::from_str_radix(component, 16).ok()?;
    // Scale 1/2/3/4 nibble components into 8-bit.
    let scaled = match component.len() {
        1 => value * 17,
        2 => value,
        3 => value >> 4,
        4 => value >> 8,
        _ => return None,
    };
    u8::try_from(scaled).ok()
}

/// Convert reinjected raw bytes into coarse UI events (printable keys + enter).
#[must_use]
pub fn reinject_bytes_as_events(bytes: &[u8]) -> Vec<UiEvent> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\r' | b'\n' => {
                events.push(UiEvent::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )));
                // Collapse CRLF.
                if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 1;
                }
            }
            b'\t' => {
                events.push(UiEvent::Key(KeyEvent::new(
                    KeyCode::Tab,
                    KeyModifiers::NONE,
                )));
            }
            0x7f | 0x08 => {
                events.push(UiEvent::Key(KeyEvent::new(
                    KeyCode::Backspace,
                    KeyModifiers::NONE,
                )));
            }
            0x1b => {
                // Leave complex escapes alone as individual Esc keys; EventStream
                // owns full parsing after probes complete.
                events.push(UiEvent::Key(KeyEvent::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                )));
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                events.push(UiEvent::Key(KeyEvent::new(
                    KeyCode::Char(char::from(b)),
                    KeyModifiers::NONE,
                )));
            }
            _ => {}
        }
        i += 1;
    }
    events
}

/// Read pending probe bytes, blocking at most `timeout` for readiness.
///
/// `Ok(None)` means the readiness window expired without bytes; `Ok(Some)`
/// carries one read (empty on EOF). Zero timeout keeps the old
/// non-blocking semantics.
fn read_stdin_within(timeout: Duration) -> io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;

        let stdin = io::stdin();
        let fd = stdin.as_fd();
        let mut fds = [nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)];
        // Round sub-millisecond remainders up: the wait must not expire early.
        let timeout_ms = if timeout.is_zero() {
            0_u16
        } else {
            u16::try_from(timeout.as_millis() + 1).unwrap_or(u16::MAX)
        };
        if nix::poll::poll(&mut fds, timeout_ms)
            .map_err(|error| io::Error::other(format!("poll stdin: {error}")))?
            == 0
        {
            return Ok(None);
        }
        let mut buffer = [0_u8; 512];
        match stdin.lock().read(&mut buffer) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(len) => Ok(Some(buffer[..len].to_vec())),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(unix))]
    {
        // No readiness primitive here: honor the budget with a bounded sleep
        // so the caller's deadline logic stays identical on this path.
        if !timeout.is_zero() {
            std::thread::sleep(timeout);
        }
        Ok(None)
    }
}

/// Seed cell dimensions into caps when a probe reply provided them.
#[must_use]
pub fn cell_from_caps(caps: &TerminalCapabilities) -> CellDimensions {
    caps.cell
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::caps::TerminalCapabilities;

    #[test]
    fn parses_fragmented_kitty_and_da1() {
        let mut session = ProbeSession::new();
        assert_eq!(session.feed(b"\x1b[?"), ProbeFeed::Consumed);
        assert_eq!(session.feed(b"7u"), ProbeFeed::Consumed);
        assert_eq!(session.feed(b"\x1b[?62;c"), ProbeFeed::Consumed);
        assert!(session.replies().contains(&ProbeReply::KittyFlags(7)));
        assert!(session.replies().contains(&ProbeReply::DeviceAttributes));
    }

    #[test]
    fn interleaves_keystroke_with_probe_replies() {
        let mut session = ProbeSession::new();
        let result = session.feed(b"x");
        assert!(matches!(result, ProbeFeed::PendingInput(_)));
        if let ProbeFeed::PendingInput(bytes) = result {
            assert_eq!(bytes, b"x");
        }
        assert_eq!(session.feed(b"\x1b[10;5R"), ProbeFeed::Consumed);
        assert!(
            session
                .replies()
                .contains(&ProbeReply::CursorPosition { row: 10, col: 5 })
        );
        let events = reinject_bytes_as_events(b"x");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn timeout_reinjects_incomplete_prefix() {
        let mut session = ProbeSession::new();
        let _ = session.feed(b"\x1b[?");
        let pending = session.flush_timeout();
        assert_eq!(pending, b"\x1b[?");
    }

    #[test]
    fn apply_updates_capabilities_and_cursor() {
        let mut session = ProbeSession::new();
        let _ = session.feed(b"\x1b[?7u\x1b[?62;c\x1b[6;18;9t\x1b]11;rgb:00/00/00\x07\x1b[3;4R");
        let mut caps = TerminalCapabilities::default();
        let cursor = session.apply_to(&mut caps);
        assert!(caps.kitty_keyboard());
        assert_eq!(caps.cell.width, 9);
        assert_eq!(caps.cell.height, 18);
        assert_eq!(caps.dark_background, Some(true));
        assert_eq!(cursor, Some((3, 2)));
    }

    #[test]
    fn terminal_theme_detection_prefers_osc_then_colorfgbg_then_dark() {
        assert_eq!(
            detect_terminal_theme(Some(false), Some("15;0")),
            TerminalTheme::Light
        );
        assert_eq!(
            detect_terminal_theme(None, Some("15;0")),
            TerminalTheme::Dark
        );
        assert_eq!(
            detect_terminal_theme(None, Some("0;15")),
            TerminalTheme::Light
        );
        assert_eq!(detect_terminal_theme(None, None), TerminalTheme::Dark);
    }

    #[test]
    fn probe_batch_has_no_sync_wrapper() {
        let batch = probe_query_batch(true);
        assert!(!batch.windows(8).any(|w| w == b"\x1b[?2026"));
        assert!(batch.windows(4).any(|w| w == b"\x1b[?u"));
        assert!(batch.windows(3).any(|w| w == b"\x1b[c"));
        assert!(batch.windows(4).any(|w| w == b"\x1b[6n"));
    }

    #[test]
    fn osc_11_query_is_background_only() {
        let query = osc_11_query();
        assert_eq!(query, b"\x1b]11;?\x07");
        assert!(!query.windows(3).any(|w| w == b"\x1b[c"));
        assert!(!query.windows(4).any(|w| w == b"\x1b[6n"));
    }

    #[test]
    fn probe_background_from_chunks_classifies_reply() {
        let (dark, events) =
            probe_background_from_chunks([b"\x1b]11;rgb:ffff/ffff/ffff\x07".as_slice()]);
        assert_eq!(dark, Some(false));
        assert!(events.is_empty());

        let (dark, events) = probe_background_from_chunks([b"\x1b]11;rgb:00/00/00\x07".as_slice()]);
        assert_eq!(dark, Some(true));
        assert!(events.is_empty());
    }

    #[test]
    fn probe_background_from_chunks_timeout_is_none() {
        // Incomplete OSC 11 prefix — flush_timeout treats it as user input.
        let (dark, events) = probe_background_from_chunks([b"\x1b]11;rgb:".as_slice()]);
        assert_eq!(dark, None);
        assert!(!events.is_empty());
    }

    #[test]
    fn probe_background_from_chunks_preserves_interleaved_keys() {
        let (dark, events) = probe_background_from_chunks([
            b"a".as_slice(),
            b"\x1b]11;rgb:00/00/00\x07".as_slice(),
            b"b".as_slice(),
        ]);
        assert_eq!(dark, Some(true));
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            UiEvent::Key(k) if k.code == KeyCode::Char('a')
        ));
        assert!(matches!(
            &events[1],
            UiEvent::Key(k) if k.code == KeyCode::Char('b')
        ));
    }

    #[test]
    fn probe_background_from_chunks_empty_is_none() {
        let (dark, events) = probe_background_from_chunks(std::iter::empty::<&[u8]>());
        assert_eq!(dark, None);
        assert!(events.is_empty());
    }
}
