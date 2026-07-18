//! Capability / cursor probe session with fragmented-reply handling.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::component::UiEvent;
use crate::terminal::caps::{CellDimensions, TerminalCapabilities};

/// Fragment timeout for split capability replies (TS: 150 ms).
pub const PROBE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);

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
    fn probe_batch_has_no_sync_wrapper() {
        let batch = probe_query_batch(true);
        assert!(!batch.windows(8).any(|w| w == b"\x1b[?2026"));
        assert!(batch.windows(4).any(|w| w == b"\x1b[?u"));
        assert!(batch.windows(3).any(|w| w == b"\x1b[c"));
        assert!(batch.windows(4).any(|w| w == b"\x1b[6n"));
    }
}
