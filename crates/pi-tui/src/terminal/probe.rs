//! Capability / cursor probing over the ONE shared terminal reader.
//!
//! Startup probing and mid-session requeries no longer read stdin and no
//! longer parse replies themselves: they write query bytes, then collect
//! typed replies through [`crossterm::event::reply::poll_reply`], which
//! drives the same persistent crossterm parser that the `EventStream` uses.
//! There is no raw stdin ownership shuttle and no second decoder; ordinary
//! keystrokes seen while collecting stay queued in the shared reader and
//! reach the product through the regular event stream, in arrival order.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::reply::{self, TerminalReply};

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
/// the yield flag: the collector must hand the reader back within a few
/// milliseconds of the arm, not at the next full-budget deadline. Slicing is
/// event-driven (each slice is a poll that wakes on bytes or a completed
/// reply), so a quiet responding terminal costs at most a handful of extra
/// wakeups.
const PROBE_YIELD_POLL_SLICE: Duration = Duration::from_millis(3);

/// Terminal background polarity used by automatic theme selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTheme {
    /// A dark terminal background.
    Dark,
    /// A light terminal background.
    Light,
}

/// One query kind the probe can issue.
///
/// Queries are recorded BEFORE their bytes are written; a failed write
/// undoes the record so no reply is ever awaited for a query that never
/// left the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// OSC 11 background-color query.
    Osc11,
    /// Cell size query (`CSI 16 t`).
    CellSize,
    /// Cursor position report query (`CSI 6 n`).
    CursorPosition,
    /// Kitty keyboard enhancement query (`CSI ? u`).
    KittyFlags,
    /// Primary device attributes (`CSI c`).
    DeviceAttributes,
}

/// The record of probe queries issued on the output stream, in write order.
#[derive(Debug, Clone, Default)]
pub struct IssuedQueries {
    kinds: Vec<QueryKind>,
}

impl IssuedQueries {
    /// The full startup batch record.
    #[must_use]
    pub fn startup(include_cell_size: bool) -> Self {
        let mut kinds = vec![QueryKind::KittyFlags, QueryKind::DeviceAttributes];
        if include_cell_size {
            kinds.push(QueryKind::CellSize);
        }
        kinds.push(QueryKind::Osc11);
        kinds.push(QueryKind::CursorPosition);
        Self { kinds }
    }

    /// A single OSC 11 requery record.
    #[must_use]
    pub fn osc11() -> Self {
        Self {
            kinds: vec![QueryKind::Osc11],
        }
    }

    /// Issued kinds, in write order.
    #[must_use]
    pub fn kinds(&self) -> &[QueryKind] {
        &self.kinds
    }

    /// Whether `kind` was issued and not yet answered.
    #[must_use]
    pub fn is_outstanding(&self, kind: QueryKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// Consume the record of `kind` when its reply arrives.
    fn answer(&mut self, kind: QueryKind) {
        if let Some(index) = self.kinds.iter().position(|issued| *issued == kind) {
            self.kinds.remove(index);
        }
    }

    /// Undo the record (the write failed; nothing was issued).
    fn undo(&mut self) {
        self.kinds.clear();
    }
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
pub fn background_from_replies(replies: &[TerminalReply]) -> Option<bool> {
    replies.iter().find_map(|reply| match reply {
        TerminalReply::Osc11(payload) => classify_background(payload),
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
/// Returns the issued-query record, or `None` when stdin is not a terminal —
/// no batch is written and the matching collection completes immediately.
/// The record is created BEFORE the write and undone if the write fails, so
/// no reply is awaited for a query that never left the process.
/// Written outside synchronized output, before
/// [`crate::terminal::TerminalInput`] takes ownership of the reader.
///
/// # Errors
///
/// Returns [`io::Error`] when writing or flushing the probe batch fails.
pub fn probe_write_batch<W: Write>(output: &mut W) -> io::Result<Option<IssuedQueries>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut issued = IssuedQueries::startup(true);
    let write = output
        .write_all(&probe_query_batch(true))
        .and_then(|()| output.flush());
    match write {
        Ok(()) => Ok(Some(issued)),
        Err(error) => {
            issued.undo();
            Err(error)
        }
    }
}

/// Collect the startup probe replies written by [`probe_write_batch`]
/// (phase 2), merging recognized replies into `caps`.
///
/// Blocks for the two-phase reply budget — at most
/// [`PROBE_FIRST_BYTE_TIMEOUT`] on a silent terminal — so callers that
/// painted a first frame speculatively during this window re-derive theme
/// and capability state afterwards and repaint when it changed.
///
/// The returned vector is always empty and kept for call-shape stability:
/// early ordinary keystrokes remain queued in the shared crossterm reader
/// and are delivered by the event stream after it starts, in arrival order.
///
/// # Errors
///
/// Returns [`io::Error`] when the reader fails, including a latched reply
/// protocol error (recognized malformed or oversized framing).
pub fn probe_collect_replies(caps: &mut TerminalCapabilities) -> io::Result<Vec<UiEvent>> {
    probe_collect_replies_with_yield(caps, &AtomicBool::new(false), &IssuedQueries::startup(true))
}

/// Yield-aware variant of [`probe_collect_replies`]: when `yield_now` is
/// armed, the collector stops reading within [`PROBE_YIELD_POLL_SLICE`] once
/// a reply stream exists. Callers that must start the event stream by a
/// deadline (the runtime arms this right before painting the first frame)
/// get the shared reader back promptly; input written at first-paint time is
/// parsed by the same persistent parser the collector used.
///
/// `issued` is the record returned by [`probe_write_batch`]: the collector
/// completes when every issued query class that can complete has answered.
///
/// # Errors
///
/// Returns [`io::Error`] when the reader fails, including a latched reply
/// protocol error.
pub(crate) fn probe_collect_replies_with_yield(
    caps: &mut TerminalCapabilities,
    yield_now: &AtomicBool,
    issued: &IssuedQueries,
) -> io::Result<Vec<UiEvent>> {
    let mut collector = ProbeCollector::default();
    let fragment_deadline = Instant::now() + PROBE_FRAGMENT_TIMEOUT;
    let first_byte_deadline = Instant::now() + PROBE_FIRST_BYTE_TIMEOUT;
    let mut reply_stream_seen = false;
    let mut answered = issued.clone();
    loop {
        if all_outstanding_answered(&answered)
            || collector.is_complete()
            || (reply_stream_seen && yield_now.load(Ordering::Relaxed))
        {
            break;
        }
        let active_deadline = if reply_stream_seen {
            fragment_deadline
        } else {
            first_byte_deadline
        };
        let Some(remaining) = active_deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        // Once a reply stream exists AND the owner armed the yield flag,
        // slice the wait so the reader is handed back promptly.
        let wait = if reply_stream_seen && yield_now.load(Ordering::Relaxed) {
            remaining.min(PROBE_YIELD_POLL_SLICE)
        } else {
            remaining
        };
        match reply::poll_reply(Some(wait)) {
            Ok(Some(out)) => {
                reply_stream_seen = true;
                record_answer(&mut answered, &out);
                collector.record(out);
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
            Err(error) => return Err(error),
        }
    }
    collector.apply_to(caps);
    // Early ordinary keystrokes stay queued in the shared reader; the event
    // stream delivers them after it starts, in arrival order. Nothing is
    // re-injected and nothing is dropped.
    Ok(Vec::new())
}

/// A startup probe is complete when the terminal class answered (DA1 or
/// kitty flags) and the cursor reported; OSC 11 and cell size are best-effort
/// refinements that must not extend the wait alone.
fn all_outstanding_answered(answered: &IssuedQueries) -> bool {
    !answered.is_outstanding(QueryKind::DeviceAttributes)
        && !answered.is_outstanding(QueryKind::KittyFlags)
        && !answered.is_outstanding(QueryKind::CursorPosition)
}

fn record_answer(answered: &mut IssuedQueries, reply: &TerminalReply) {
    match reply {
        TerminalReply::Osc11(_) => answered.answer(QueryKind::Osc11),
        TerminalReply::CellSize { .. } => answered.answer(QueryKind::CellSize),
        TerminalReply::CursorPosition { .. } => answered.answer(QueryKind::CursorPosition),
        TerminalReply::KeyboardEnhancementFlags(_) => answered.answer(QueryKind::KittyFlags),
        TerminalReply::PrimaryDeviceAttributes => answered.answer(QueryKind::DeviceAttributes),
    }
}

/// Mid-session OSC 11 re-probe: emit only the background query and classify a
/// bounded reply.
///
/// Call only while the sole [`crate::terminal::TerminalInput`] `EventStream`
/// is paused so the collector and the stream share the reader serially.
/// `None` means timeout / no-TTY / unparseable reply — the caller keeps its
/// prior classification. Keystrokes typed during the requery stay queued in
/// the shared parser (pending CSI, UTF-8, and paste state included) and are
/// delivered when the stream resumes.
///
/// A latched reply protocol error propagates as [`io::Error`]; the caller
/// recovers via `crossterm::event::reply::recover_protocol_error`.
///
/// # Errors
///
/// Returns [`io::Error`] when writing or flushing the query fails or the
/// reader latches a protocol error.
pub fn probe_background<W: Write>(output: &mut W) -> io::Result<Option<bool>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut issued = IssuedQueries::osc11();
    let write = output
        .write_all(osc_11_query())
        .and_then(|()| output.flush());
    match write {
        Ok(()) => {}
        Err(error) => {
            issued.undo();
            return Err(error);
        }
    }

    let fragment_deadline = Instant::now() + PROBE_FRAGMENT_TIMEOUT;
    let mut dark = None;
    while let Some(remaining) = fragment_deadline.checked_duration_since(Instant::now()) {
        match reply::poll_reply(Some(remaining)) {
            Ok(Some(TerminalReply::Osc11(payload))) => {
                dark = classify_background(&payload);
                break;
            }
            // A late reply to a different query is consumed (the sink is
            // bounded) but must not update adopted background state.
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => break,
            Err(error) => return Err(error),
        }
    }
    Ok(dark)
}

/// Stateful accumulator for probe replies collected through the shared
/// reader, with the capability-merge and deterministic-seed policies.
#[derive(Debug, Default)]
pub struct ProbeCollector {
    replies: Vec<TerminalReply>,
    saw_kitty: bool,
    saw_da1: bool,
    saw_cursor: bool,
}

impl ProbeCollector {
    /// Record one collected reply.
    pub fn record(&mut self, reply: TerminalReply) {
        match &reply {
            TerminalReply::KeyboardEnhancementFlags(_) => self.saw_kitty = true,
            TerminalReply::PrimaryDeviceAttributes => self.saw_da1 = true,
            TerminalReply::CursorPosition { .. } => self.saw_cursor = true,
            TerminalReply::Osc11(_) | TerminalReply::CellSize { .. } => {}
        }
        self.replies.push(reply);
    }

    /// Collected replies in arrival order.
    #[must_use]
    pub fn replies(&self) -> &[TerminalReply] {
        &self.replies
    }

    /// Whether the session has enough replies to stop waiting.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        // DA1 or kitty is the class sentinel; cursor is also required for
        // cache seeding.
        (self.saw_da1 || self.saw_kitty) && self.saw_cursor
    }

    /// Apply collected replies onto a capability cache; returns the reported
    /// cursor position (zero-based) when one arrived.
    pub fn apply_to(&self, caps: &mut TerminalCapabilities) -> Option<(u16, u16)> {
        let mut cursor = None;
        for reply in &self.replies {
            match reply {
                TerminalReply::KeyboardEnhancementFlags(flags) => {
                    caps.set_kitty_keyboard(!flags.is_empty());
                }
                TerminalReply::PrimaryDeviceAttributes => {
                    if !self.saw_kitty {
                        caps.set_kitty_keyboard(false);
                    }
                }
                TerminalReply::CellSize { width, height } => {
                    caps.set_cell_dimensions(*width, *height);
                }
                TerminalReply::Osc11(payload) => {
                    caps.set_dark_background(classify_background(payload));
                }
                TerminalReply::CursorPosition { column, row } => {
                    cursor = Some((*column, *row));
                }
            }
        }
        cursor
    }

    /// Seed deterministic defaults for fixture runs whose harness never
    /// answers: kitty off (DA1 seen), 20x10 cells, dark background, origin
    /// cursor. Marks the collector complete.
    pub fn seed_defaults(&mut self) {
        if !self.saw_kitty && !self.saw_da1 {
            self.replies.push(TerminalReply::PrimaryDeviceAttributes);
            self.saw_da1 = true;
        }
        self.replies.push(TerminalReply::CellSize {
            width: 20,
            height: 10,
        });
        self.replies
            .push(TerminalReply::Osc11("rgb:0000/0000/0000".to_string()));
        self.replies
            .push(TerminalReply::CursorPosition { column: 0, row: 0 });
        self.saw_cursor = true;
    }
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
    fn collector_marks_completion_on_class_and_cursor() {
        let mut collector = ProbeCollector::default();
        assert!(!collector.is_complete());
        collector.record(TerminalReply::KeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ));
        assert!(!collector.is_complete(), "kitty alone is not complete");
        collector.record(TerminalReply::CursorPosition { column: 4, row: 2 });
        assert!(collector.is_complete());
    }

    #[test]
    fn collector_da1_without_kitty_disables_kitty() {
        let mut collector = ProbeCollector::default();
        collector.record(TerminalReply::PrimaryDeviceAttributes);
        collector.record(TerminalReply::CursorPosition { column: 0, row: 0 });
        let mut caps = TerminalCapabilities::default();
        caps.set_kitty_keyboard(true);
        collector.apply_to(&mut caps);
        assert!(
            !caps.kitty_keyboard(),
            "DA1-only terminals must not use kitty"
        );
    }

    #[test]
    fn collector_applies_cell_background_and_cursor() {
        let mut collector = ProbeCollector::default();
        collector.record(TerminalReply::CellSize {
            width: 9,
            height: 18,
        });
        collector.record(TerminalReply::Osc11("rgb:00/00/00".to_string()));
        collector.record(TerminalReply::CursorPosition { column: 3, row: 2 });
        collector.record(TerminalReply::PrimaryDeviceAttributes);
        let mut caps = TerminalCapabilities::default();
        let cursor = collector.apply_to(&mut caps);
        assert_eq!(caps.cell.width, 9);
        assert_eq!(caps.cell.height, 18);
        assert_eq!(caps.dark_background, Some(true));
        assert_eq!(cursor, Some((3, 2)));
    }

    #[test]
    fn seed_defaults_produce_the_documented_fallback() {
        let mut collector = ProbeCollector::default();
        collector.seed_defaults();
        assert!(collector.is_complete());
        let mut caps = TerminalCapabilities::default();
        let cursor = collector.apply_to(&mut caps);
        assert!(!caps.kitty_keyboard());
        assert_eq!(caps.cell.width, 20);
        assert_eq!(caps.cell.height, 10);
        assert_eq!(caps.dark_background, Some(true));
        assert_eq!(cursor, Some((0, 0)));
    }

    #[test]
    fn background_classification_prefers_first_osc() {
        let replies = vec![
            TerminalReply::Osc11("rgb:ffff/ffff/ffff".to_string()),
            TerminalReply::Osc11("rgb:00/00/00".to_string()),
        ];
        assert_eq!(background_from_replies(&replies), Some(false));
    }

    #[test]
    fn issued_queries_track_outstanding_kinds() {
        let mut issued = IssuedQueries::startup(true);
        assert!(issued.is_outstanding(QueryKind::Osc11));
        assert!(issued.is_outstanding(QueryKind::CursorPosition));
        issued.answer(QueryKind::CursorPosition);
        assert!(!issued.is_outstanding(QueryKind::CursorPosition));
        issued.undo();
        assert!(issued.kinds().is_empty());
    }

    #[test]
    fn issued_osc11_record_matches_requery() {
        let issued = IssuedQueries::osc11();
        assert_eq!(issued.kinds(), &[QueryKind::Osc11]);
    }
}
