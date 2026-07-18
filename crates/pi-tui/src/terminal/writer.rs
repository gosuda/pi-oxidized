//! `UiWriter` / `Tui` transaction pipeline and background coalescer.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::component::Component;
use crate::frame::{FrameAnnotations, with_annotations};
use crate::terminal::backend::{
    GuardedBackend, audit_bytes, encode_full_row_prefix, wrap_synchronized,
};
use crate::terminal::caps::{TerminalCapabilities, kitty_delete_id};
use crate::terminal::sink::FrameSink;

const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";

/// Maximum background coalescing window.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(16);

/// Why a re-anchor transaction was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReanchorCause {
    /// Width and/or height changed.
    Resize,
    /// Explicit session replacement (`/new`, `/resume`).
    SessionReplace,
    /// Scrollback invalidation or image scroll-off.
    ScrollbackInvalidate,
    /// Suspend/resume redraw.
    Resume,
}

/// Settled content moved into terminal scrollback via `insert_before`.
#[derive(Debug, Clone)]
pub enum SettledBlock {
    /// Styled lines rendered through Ratatui.
    Lines(Vec<Line<'static>>),
    /// Pre-encoded raw protocol bytes (images) with a text fallback.
    Raw {
        /// Rows to allocate above the viewport.
        rows: u16,
        /// Protocol bytes to emit into the allocated rows.
        bytes: Vec<u8>,
        /// Optional Kitty id for later deletion.
        kitty_id: Option<u32>,
        /// Fallback lines when images are unsupported or rows do not fit.
        fallback: Vec<Line<'static>>,
    },
}

/// Terminal mutation transaction executed by [`Tui::commit`].
#[derive(Debug, Clone)]
pub enum Txn {
    /// Redraw the inline viewport.
    Frame,
    /// Insert settled blocks into scrollback, then redraw the viewport.
    Settle(Vec<SettledBlock>),
    /// Full no-clear re-anchor of the inline viewport.
    Reanchor(ReanchorCause),
    /// Grow/shrink the inline viewport height with scroll preservation.
    SetViewportHeight(u16),
}

/// Capacity-1 latest-value coalescer for background paints.
#[derive(Debug, Default)]
pub struct Coalescer {
    dirty: bool,
    deadline: Option<Instant>,
}

impl Coalescer {
    /// Create an idle coalescer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark background work dirty and arm the deadline on the first mark.
    pub fn mark(&mut self, now: Instant) {
        if !self.dirty {
            self.deadline = Some(now + COALESCE_WINDOW);
        }
        self.dirty = true;
    }

    /// Returns true when a coalesced paint should fire.
    #[must_use]
    pub fn ready(&self, now: Instant) -> bool {
        self.dirty && self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Time remaining until the coalesced paint, if dirty.
    #[must_use]
    pub fn time_until_ready(&self, now: Instant) -> Option<Duration> {
        if !self.dirty {
            return None;
        }
        self.deadline.map(|deadline| {
            if now >= deadline {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(now)
            }
        })
    }

    /// Clear the dirty flag after a paint.
    pub fn clear(&mut self) {
        self.dirty = false;
        self.deadline = None;
    }

    /// Whether any background paint is pending.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// Accounting for the inline viewport and live Kitty images.
#[derive(Debug, Clone)]
struct ViewportState {
    size: Size,
    cursor: Position,
    viewport_height: u16,
    viewport_top: u16,
    live_kitty_ids: HashSet<u32>,
}

impl ViewportState {
    fn new(size: Size, cursor: Position, viewport_height: u16) -> Self {
        let viewport_height = viewport_height.min(size.height).max(1);
        let viewport_top = cursor.y.saturating_sub(viewport_height.saturating_sub(1));
        Self {
            size,
            cursor,
            viewport_height,
            viewport_top,
            live_kitty_ids: HashSet::new(),
        }
    }

    fn viewport_area(&self) -> Rect {
        Rect::new(0, self.viewport_top, self.size.width, self.viewport_height)
    }

    fn bottom_row(&self) -> u16 {
        self.viewport_top
            .saturating_add(self.viewport_height)
            .saturating_sub(1)
    }
}

/// Single stdout owner implementing the three-stage transaction pipeline.
///
/// Generic over the outer writer so unit tests can inject a recorder.
pub struct Tui<W: Write> {
    terminal: Terminal<GuardedBackend<CrosstermBackend<FrameSink>>>,
    composition: Arc<Mutex<Vec<u8>>>,
    outer: W,
    caps: TerminalCapabilities,
    state: ViewportState,
    coalescer: Coalescer,
    write_count: u64,
    last_payload: Vec<u8>,
    hardware_cursor: bool,
}

impl<W: Write> Tui<W> {
    /// Create a `Tui` around an outer writer with known size/cursor/capabilities.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when terminal initialization or flushing fails.
    pub fn new(
        mut outer: W,
        size: Size,
        cursor: Position,
        viewport_height: u16,
        caps: TerminalCapabilities,
    ) -> io::Result<Self> {
        let composition = Arc::new(Mutex::new(Vec::new()));
        let sink = FrameSink::with_shared(Arc::clone(&composition));
        let backend = CrosstermBackend::new(sink);
        let mut guarded = GuardedBackend::new(backend, size, cursor);
        if std::env::var_os("PI_TUI_AUDIT").is_some() {
            guarded.set_byte_audit(true);
        }
        let terminal = Terminal::with_options(
            guarded,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height.min(size.height).max(1)),
            },
        )?;
        outer.flush()?;
        Ok(Self {
            terminal,
            composition,
            outer,
            caps,
            state: ViewportState::new(size, cursor, viewport_height),
            coalescer: Coalescer::new(),
            write_count: 0,
            last_payload: Vec::new(),
            hardware_cursor: std::env::var_os("PI_HARDWARE_CURSOR").is_some(),
        })
    }

    /// Borrow capabilities.
    #[must_use]
    pub fn capabilities(&self) -> &TerminalCapabilities {
        &self.caps
    }

    /// Mutable capabilities (after reprobe).
    pub fn capabilities_mut(&mut self) -> &mut TerminalCapabilities {
        &mut self.caps
    }

    /// Current terminal size cache.
    #[must_use]
    pub fn size(&self) -> Size {
        self.state.size
    }

    /// Current viewport height.
    #[must_use]
    pub fn viewport_height(&self) -> u16 {
        self.state.viewport_height
    }

    /// Background coalescer.
    pub fn coalescer_mut(&mut self) -> &mut Coalescer {
        &mut self.coalescer
    }

    /// Number of stage-3 writes performed.
    #[must_use]
    pub fn write_count(&self) -> u64 {
        self.write_count
    }

    /// Last stage-3 payload (for tests).
    #[must_use]
    pub fn last_payload(&self) -> &[u8] {
        &self.last_payload
    }

    /// Update size cache from a resize event (does not paint).
    pub fn note_resize(&mut self, width: u16, height: u16) {
        self.state.size = Size::new(width, height);
        self.terminal.backend_mut().set_size(self.state.size);
        if self.state.viewport_height > height {
            self.state.viewport_height = height.max(1);
        }
    }

    /// Commit a transaction against `root`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when composing or writing the transaction fails.
    pub fn commit(&mut self, txn: Txn, root: &mut dyn Component) -> io::Result<()> {
        match txn {
            Txn::Frame => self.commit_frame(root, false),
            Txn::Settle(blocks) => self.commit_settle(blocks, root),
            Txn::Reanchor(cause) => self.commit_reanchor(cause, root),
            Txn::SetViewportHeight(height) => self.commit_set_viewport_height(height, root),
        }
    }

    fn commit_frame(&mut self, root: &mut dyn Component, full_rows: bool) -> io::Result<()> {
        let annotations = RefCell::new(FrameAnnotations::new());
        let area = self.state.viewport_area();
        let hardware_cursor = self.hardware_cursor;

        if full_rows {
            self.terminal
                .backend_mut()
                .set_full_row_region(area.y..area.y.saturating_add(area.height));
        } else {
            self.terminal.backend_mut().set_full_rows(false);
        }
        {
            let terminal = &mut self.terminal;
            let draw_result = with_annotations(&annotations, || {
                terminal.draw(|frame| {
                    let frame_area = frame.area();
                    let height = root.measure(frame_area.width).min(frame_area.height);
                    let render_area =
                        Rect::new(frame_area.x, frame_area.y, frame_area.width, height);
                    root.render(render_area, frame.buffer_mut());
                    if hardware_cursor && let Some(pos) = annotations.borrow().cursor() {
                        frame.set_cursor_position(pos);
                    }
                })
            });
            draw_result?;
        }
        self.terminal.backend_mut().set_full_rows(false);

        let (cursor, raw_regions) = annotations.into_inner().into_parts();
        let mut payload = self.take_composition_bytes();

        let mut next_ids = HashSet::new();
        for region in &raw_regions {
            if let Some(id) = region.kitty_id {
                next_ids.insert(id);
            }
        }
        for id in self.state.live_kitty_ids.difference(&next_ids) {
            payload.extend_from_slice(&kitty_delete_id(*id));
        }
        for region in raw_regions {
            payload.extend_from_slice(b"\x1b7");
            let y = region.area.y.saturating_add(1);
            let x = region.area.x.saturating_add(1);
            payload.extend_from_slice(format!("\x1b[{y};{x}H").as_bytes());
            payload.extend_from_slice(&region.bytes);
            payload.extend_from_slice(b"\x1b8");
        }
        self.state.live_kitty_ids = next_ids;

        if let Some(cursor) = cursor {
            self.state.cursor = cursor;
            self.terminal.backend_mut().set_cursor_cache(cursor);
        }

        self.stage3_write(&payload)
    }

    fn commit_settle(
        &mut self,
        blocks: Vec<SettledBlock>,
        root: &mut dyn Component,
    ) -> io::Result<()> {
        for block in blocks {
            match block {
                SettledBlock::Lines(lines) => {
                    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
                    if height == 0 {
                        continue;
                    }
                    let lines_for_draw = lines.clone();
                    self.terminal.insert_before(height, |buf| {
                        render_lines(buf, &lines_for_draw);
                    })?;
                    self.state.viewport_top = self.state.viewport_top.saturating_add(height);
                }
                SettledBlock::Raw {
                    rows,
                    bytes,
                    kitty_id,
                    fallback,
                } => {
                    let use_raw = self.caps.images.is_some()
                        && rows > 0
                        && rows <= self.state.viewport_top
                        && !bytes.is_empty();
                    if use_raw {
                        self.terminal.insert_before(rows, |_buf| {})?;
                        let top = self.state.viewport_top;
                        let dest_row = top.saturating_sub(rows);
                        let mut extra = Vec::new();
                        extra.extend_from_slice(
                            format!("\x1b[{};1H", dest_row.saturating_add(1)).as_bytes(),
                        );
                        extra.extend_from_slice(&bytes);
                        self.push_composition_bytes(&extra);
                        if let Some(id) = kitty_id {
                            self.state.live_kitty_ids.insert(id);
                        }
                        self.state.viewport_top = self.state.viewport_top.saturating_add(rows);
                    } else {
                        let height = u16::try_from(fallback.len()).unwrap_or(u16::MAX);
                        if height > 0 {
                            let fallback_draw = fallback.clone();
                            self.terminal.insert_before(height, |buf| {
                                render_lines(buf, &fallback_draw);
                            })?;
                            self.state.viewport_top =
                                self.state.viewport_top.saturating_add(height);
                        }
                    }
                }
            }
        }

        // insert_before + redraw are inseparable in one stage-3 write.
        self.commit_frame(root, false)
    }

    fn commit_reanchor(
        &mut self,
        _cause: ReanchorCause,
        root: &mut dyn Component,
    ) -> io::Result<()> {
        root.invalidate();
        let height = self
            .state
            .viewport_height
            .min(self.state.size.height)
            .max(1);
        self.state.viewport_height = height;
        self.state.viewport_top = self.state.size.height.saturating_sub(height);
        self.state.cursor = Position {
            x: 0,
            y: self.state.bottom_row(),
        };
        self.terminal.backend_mut().set_size(self.state.size);
        self.terminal
            .backend_mut()
            .set_cursor_cache(self.state.cursor);
        // Explicit buffer resize uses the cached cursor and suppresses bulk clears.
        self.terminal.resize(self.state.viewport_area())?;

        let mut payload_prefix = Vec::new();
        for id in &self.state.live_kitty_ids {
            payload_prefix.extend_from_slice(&kitty_delete_id(*id));
        }
        self.state.live_kitty_ids.clear();
        if !payload_prefix.is_empty() {
            self.push_composition_bytes(&payload_prefix);
        }

        self.commit_frame(root, true)
    }

    fn commit_set_viewport_height(
        &mut self,
        height: u16,
        root: &mut dyn Component,
    ) -> io::Result<()> {
        let height = height.min(self.state.size.height).max(1);
        if height == self.state.viewport_height {
            return self.commit_frame(root, false);
        }

        let old_top = self.state.viewport_top;
        let old_bottom = old_top.saturating_add(self.state.viewport_height);
        if height > self.state.viewport_height {
            let grow = height - self.state.viewport_height;
            let mut scroll = Vec::new();
            for _ in 0..grow {
                scroll.extend_from_slice(b"\r\n");
            }
            self.push_composition_bytes(&scroll);
            self.state.viewport_top = self.state.viewport_top.saturating_add(grow);
        } else {
            // Keep the viewport bottom anchored. Rows above the new viewport
            // must be erased once in the same transaction as the redraw.
            let new_top = old_bottom.saturating_sub(height);
            let mut abandoned = Vec::new();
            for row in old_top..new_top {
                encode_full_row_prefix(&mut abandoned, row);
            }
            self.push_composition_bytes(&abandoned);
            self.state.viewport_top = new_top;
        }
        self.state.viewport_height = height;

        let pending = self.take_composition_bytes();
        let composition = Arc::clone(&self.composition);
        let sink = FrameSink::with_shared(composition);
        let backend = CrosstermBackend::new(sink);
        let anchor = Position {
            x: 0,
            y: self.state.viewport_top,
        };
        let mut guarded = GuardedBackend::new(backend, self.state.size, anchor);
        guarded.set_size(self.state.size);
        guarded.set_cursor_cache(anchor);
        self.terminal = Terminal::with_options(
            guarded,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        // Initialization may emit scroll/cursor bytes. Preserve transaction
        // order: abandoned-row erases precede initialization and redraw.
        let initialization = self.take_composition_bytes();
        self.push_composition_bytes(&pending);
        self.push_composition_bytes(&initialization);
        self.commit_frame(root, true)
    }

    fn push_composition_bytes(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.composition.lock() {
            guard.extend_from_slice(bytes);
        }
    }

    fn take_composition_bytes(&self) -> Vec<u8> {
        self.composition
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    fn stage3_write(&mut self, payload: &[u8]) -> io::Result<()> {
        let report = audit_bytes(payload);
        if report.clear_2j > 0 || report.clear_3j > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "banned clear sequence in transaction payload",
            ));
        }
        let framed = wrap_synchronized(payload, self.caps.sync_output);
        let framed_report = audit_bytes(&framed);
        if framed_report.sync_begin != framed_report.sync_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unbalanced synchronized output markers",
            ));
        }
        write_stage3_frame(
            &mut self.outer,
            &framed,
            self.caps.sync_output && !payload.is_empty(),
        )?;
        self.write_count = self.write_count.saturating_add(1);
        self.last_payload = framed;
        let _ = self.take_composition_bytes();
        Ok(())
    }
}

fn write_stage3_frame<W: Write>(
    writer: &mut W,
    framed: &[u8],
    synchronized: bool,
) -> io::Result<()> {
    if let Err(error) = writer.write_all(framed) {
        if synchronized {
            best_effort_sync_close(writer);
        }
        return Err(error);
    }
    if let Err(error) = writer.flush() {
        if synchronized {
            best_effort_sync_close(writer);
        }
        return Err(error);
    }
    Ok(())
}

fn best_effort_sync_close<W: Write>(writer: &mut W) {
    let _ = writer.write_all(SYNC_OUTPUT_END);
    let _ = writer.flush();
}

fn render_lines(buf: &mut Buffer, lines: &[Line<'static>]) {
    let mut y = buf.area.y;
    for line in lines {
        if y >= buf.area.y.saturating_add(buf.area.height) {
            break;
        }
        let area = Rect::new(buf.area.x, y, buf.area.width, 1);
        line.clone().render(area, buf);
        y = y.saturating_add(1);
    }
}

/// Transaction builder used by unit tests without a real Terminal.
#[derive(Debug, Default)]
pub struct TransactionRecorder {
    /// Recorded stage-3 writes.
    pub writes: Vec<Vec<u8>>,
    caps: TerminalCapabilities,
    size: Size,
    viewport_height: u16,
    live_kitty: HashSet<u32>,
}

impl TransactionRecorder {
    /// Create a recorder with capabilities and size.
    #[must_use]
    pub fn new(size: Size, viewport_height: u16, caps: TerminalCapabilities) -> Self {
        Self {
            writes: Vec::new(),
            caps,
            size,
            viewport_height,
            live_kitty: HashSet::new(),
        }
    }

    /// Simulate a frame / reanchor / settle at the byte level for contract tests.
    pub fn commit_bytes(&mut self, txn: SimulatedTxn) {
        let mut payload = Vec::new();
        match txn {
            SimulatedTxn::Frame { content } => payload.extend_from_slice(&content),
            SimulatedTxn::Settle {
                insert_before,
                redraw,
            } => {
                payload.extend_from_slice(&insert_before);
                payload.extend_from_slice(&redraw);
            }
            SimulatedTxn::Reanchor { rows } => {
                for id in &self.live_kitty {
                    payload.extend_from_slice(&kitty_delete_id(*id));
                }
                self.live_kitty.clear();
                for row in 0..rows.min(self.viewport_height) {
                    encode_full_row_prefix(&mut payload, row);
                    payload.extend_from_slice(b"row");
                }
            }
            SimulatedTxn::SetViewportHeight { height, content } => {
                let next_height = height.min(self.size.height).max(1);
                let old_top = self.size.height.saturating_sub(self.viewport_height);
                let new_top = self.size.height.saturating_sub(next_height);
                if next_height > self.viewport_height {
                    let grow = next_height - self.viewport_height;
                    for _ in 0..grow {
                        payload.extend_from_slice(b"\r\n");
                    }
                } else {
                    for row in old_top..new_top {
                        encode_full_row_prefix(&mut payload, row);
                    }
                }
                self.viewport_height = next_height;
                for row in new_top..self.size.height {
                    encode_full_row_prefix(&mut payload, row);
                    payload.extend_from_slice(b"row");
                }
                payload.extend_from_slice(&content);
            }
        }
        let framed = wrap_synchronized(&payload, self.caps.sync_output);
        self.writes.push(framed);
    }

    /// Track a kitty id as live.
    pub fn track_kitty(&mut self, id: u32) {
        self.live_kitty.insert(id);
    }

    /// All stage-3 bytes concatenated.
    #[must_use]
    pub fn all_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for write in &self.writes {
            out.extend_from_slice(write);
        }
        out
    }
}

/// Byte-level transaction for [`TransactionRecorder`] tests.
#[derive(Debug, Clone)]
pub enum SimulatedTxn {
    /// Single frame payload.
    Frame {
        /// Composed content.
        content: Vec<u8>,
    },
    /// Settle `insert_before` + redraw in one write.
    Settle {
        /// Bytes from `insert_before`.
        insert_before: Vec<u8>,
        /// Bytes from the following inline redraw.
        redraw: Vec<u8>,
    },
    /// Re-anchor with full-row EL2 + content for `rows`.
    Reanchor {
        /// Viewport rows to rewrite.
        rows: u16,
    },
    /// Viewport height change with optional scroll.
    SetViewportHeight {
        /// New height.
        height: u16,
        /// Redraw content.
        content: Vec<u8>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{EventResult, UiEvent};
    use crate::terminal::backend::audit_bytes;
    use crate::terminal::caps::TerminalCapabilities;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::io::Cursor;
    #[derive(Default)]
    struct FailOnceWriter {
        bytes: Vec<u8>,
        fail_after: usize,
        failed: bool,
        fail_flush: bool,
    }

    impl Write for FailOnceWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.failed && self.bytes.len() >= self.fail_after {
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected write failure",
                ));
            }
            let limit = if self.failed {
                buf.len()
            } else {
                self.fail_after
                    .saturating_sub(self.bytes.len())
                    .min(buf.len())
            };
            self.bytes.extend_from_slice(&buf[..limit]);
            Ok(limit)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                self.fail_flush = false;
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected flush failure",
                ));
            }
            Ok(())
        }
    }

    struct StubRoot {
        label: String,
        invalidated: u32,
    }

    impl Component for StubRoot {
        fn measure(&mut self, _width: u16) -> u16 {
            1
        }

        fn render(&mut self, area: Rect, buf: &mut Buffer) {
            if area.width == 0 || area.height == 0 {
                return;
            }
            let chars: Vec<char> = self.label.chars().collect();
            for (idx, ch) in chars.into_iter().enumerate() {
                let x = area.x.saturating_add(u16::try_from(idx).unwrap_or(0));
                if x >= area.x.saturating_add(area.width) {
                    break;
                }
                buf[(x, area.y)].set_char(ch);
            }
        }

        fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
            EventResult::Ignored
        }

        fn invalidate(&mut self) {
            self.invalidated = self.invalidated.saturating_add(1);
        }
    }

    struct RawRegionRoot;

    impl Component for RawRegionRoot {
        fn measure(&mut self, _width: u16) -> u16 {
            1
        }

        fn render(&mut self, area: Rect, buf: &mut Buffer) {
            buf[(area.x, area.y)].set_char('R');
            crate::frame::push_raw_region(crate::frame::RawRegion {
                area: Rect::new(2, 1, 3, 1),
                bytes: b"RAW".to_vec(),
                kitty_id: None,
            });
        }

        fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
            EventResult::Ignored
        }

        fn invalidate(&mut self) {}
    }


    #[test]
    fn coalescer_arms_deadline_once() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.mark(t0);
        let first = c.deadline;
        c.mark(t0 + Duration::from_millis(5));
        assert_eq!(c.deadline, first);
        assert!(!c.ready(t0 + Duration::from_millis(5)));
        assert!(c.ready(t0 + Duration::from_millis(16)));
        c.clear();
        assert!(!c.is_dirty());
    }

    #[test]
    fn settle_is_single_write() {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let mut rec = TransactionRecorder::new(Size::new(80, 24), 4, caps);
        rec.commit_bytes(SimulatedTxn::Settle {
            insert_before: b"INSERT".to_vec(),
            redraw: b"REDRAW".to_vec(),
        });
        assert_eq!(rec.writes.len(), 1);
        let bytes = &rec.writes[0];
        let report = audit_bytes(bytes);
        assert_eq!(report.sync_begin, 1);
        assert_eq!(report.sync_end, 1);
        assert!(report.is_clean());
        assert!(bytes.windows(6).any(|w| w == b"INSERT"));
        assert!(bytes.windows(6).any(|w| w == b"REDRAW"));
    }

    #[test]
    fn reanchor_emits_el2_then_row_without_clears() -> io::Result<()> {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let mut rec = TransactionRecorder::new(Size::new(40, 10), 3, caps);
        rec.track_kitty(9);
        rec.commit_bytes(SimulatedTxn::Reanchor { rows: 3 });
        assert_eq!(rec.writes.len(), 1);
        let bytes = &rec.writes[0];
        let report = audit_bytes(bytes);
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
        assert_eq!(report.sync_begin, 1);
        assert_eq!(report.sync_end, 1);
        let delete = kitty_delete_id(9);
        let delete_pos = find_subslice(bytes, &delete)
            .ok_or_else(|| io::Error::other("missing Kitty delete"))?;
        let el2_pos =
            find_subslice(bytes, b"\x1b[2K").ok_or_else(|| io::Error::other("missing EL2"))?;
        assert!(delete_pos < el2_pos);
        assert!(bytes.windows(6).any(|w| w == b"\x1b[2Kro"));
        Ok(())
    }

    #[test]
    fn nosync_branch_single_write_no_2026() {
        let caps = TerminalCapabilities {
            sync_output: false,
            ..TerminalCapabilities::default()
        };
        let mut rec = TransactionRecorder::new(Size::new(80, 24), 4, caps);
        rec.commit_bytes(SimulatedTxn::Frame {
            content: b"frame".to_vec(),
        });
        assert_eq!(rec.writes.len(), 1);
        let report = audit_bytes(&rec.writes[0]);
        assert_eq!(report.sync_begin, 0);
        assert_eq!(report.sync_end, 0);
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
    }

    #[test]
    fn viewport_shrink_erases_abandoned_rows_before_redraw_in_both_sync_modes() {
        for sync_output in [true, false] {
            let caps = TerminalCapabilities {
                sync_output,
                ..TerminalCapabilities::default()
            };
            let mut rec = TransactionRecorder::new(Size::new(20, 10), 4, caps);
            rec.commit_bytes(SimulatedTxn::SetViewportHeight {
                height: 2,
                content: b"tail".to_vec(),
            });

            let mut expected = Vec::new();
            for row in 6..8 {
                encode_full_row_prefix(&mut expected, row);
            }
            for row in 8..10 {
                encode_full_row_prefix(&mut expected, row);
                expected.extend_from_slice(b"row");
            }
            expected.extend_from_slice(b"tail");

            assert_eq!(rec.writes, [wrap_synchronized(&expected, sync_output)]);
            let report = audit_bytes(&rec.writes[0]);
            assert_eq!(report.clear_2j, 0);
            assert_eq!(report.clear_3j, 0);
            assert_eq!(report.sync_begin, usize::from(sync_output));
            assert_eq!(report.sync_end, usize::from(sync_output));
        }
    }

    #[test]
    fn resize_storm_recorder_stays_clear_free() {
        let caps = TerminalCapabilities::default();
        let mut rec = TransactionRecorder::new(Size::new(80, 24), 6, caps);
        for i in 0..24 {
            let width = if i % 3 == 0 {
                24
            } else if i % 3 == 1 {
                200
            } else {
                80
            };
            rec.size = Size::new(width, if i % 2 == 0 { 8 } else { 30 });
            rec.commit_bytes(SimulatedTxn::Reanchor {
                rows: rec.viewport_height,
            });
        }
        assert_eq!(rec.writes.len(), 24);
        let all = rec.all_bytes();
        let report = audit_bytes(&all);
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
        assert_eq!(report.sync_begin, report.sync_end);
    }

    #[test]
    fn tui_frame_is_exactly_one_balanced_write() -> io::Result<()> {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let outer = Cursor::new(Vec::new());
        let mut tui = Tui::new(outer, Size::new(20, 8), Position::ORIGIN, 3, caps)?;
        let mut root = StubRoot {
            label: "hello".into(),
            invalidated: 0,
        };
        tui.commit(Txn::Frame, &mut root)?;
        assert_eq!(tui.write_count(), 1);
        let report = audit_bytes(tui.last_payload());
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
        assert_eq!(report.sync_begin, 1);
        assert_eq!(report.sync_end, 1);
        assert!(report.is_clean());
        Ok(())
    }

    #[test]
    fn raw_region_transaction_saves_positions_and_restores_cursor() -> io::Result<()> {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let outer = Cursor::new(Vec::new());
        let mut tui = Tui::new(outer, Size::new(20, 8), Position::ORIGIN, 3, caps)?;
        tui.commit(Txn::Frame, &mut RawRegionRoot)?;
        assert!(
            tui.last_payload()
                .windows(b"\x1b7\x1b[2;3HRAW\x1b8".len())
                .any(|bytes| bytes == b"\x1b7\x1b[2;3HRAW\x1b8"),
            "raw region must restore the cursor: {:?}",
            String::from_utf8_lossy(tui.last_payload())
        );
        Ok(())
    }

    #[test]
    fn tui_viewport_shrink_orders_abandoned_erases_before_new_rows() -> io::Result<()> {
        for sync_output in [true, false] {
            let caps = TerminalCapabilities {
                sync_output,
                ..TerminalCapabilities::default()
            };
            let outer = Cursor::new(Vec::new());
            let mut tui = Tui::new(outer, Size::new(20, 10), Position::ORIGIN, 4, caps)?;
            let mut root = StubRoot {
                label: "world".into(),
                invalidated: 0,
            };
            tui.commit(Txn::SetViewportHeight(2), &mut root)?;
            assert_eq!(tui.write_count(), 1);

            let payload = tui.last_payload();
            let prefixes = [
                b"\x1b[1;1H\x1b[2K".as_slice(),
                b"\x1b[2;1H\x1b[2K".as_slice(),
                b"\x1b[3;1H\x1b[2K".as_slice(),
                b"\x1b[4;1H\x1b[2K".as_slice(),
            ];
            let mut previous = 0;
            for prefix in prefixes {
                let position = find_subslice(payload, prefix)
                    .ok_or_else(|| io::Error::other("missing ordered row erase"))?;
                assert!(position >= previous);
                previous = position;
            }
            let content = find_subslice(payload, b"world")
                .ok_or_else(|| io::Error::other("missing reflowed row content"))?;
            let new_first = find_subslice(payload, prefixes[2])
                .ok_or_else(|| io::Error::other("missing new first row"))?;
            let new_second = find_subslice(payload, prefixes[3])
                .ok_or_else(|| io::Error::other("missing new second row"))?;
            assert!(new_first < content && content < new_second);
            let report = audit_bytes(payload);
            assert_eq!(report.clear_2j, 0);
            assert_eq!(report.clear_3j, 0);
            assert_eq!(report.sync_begin, usize::from(sync_output));
            assert_eq!(report.sync_end, usize::from(sync_output));
        }
        Ok(())
    }

    #[test]
    fn tui_reanchor_uses_row_el2_and_invalidates() -> io::Result<()> {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let outer = Cursor::new(Vec::new());
        let mut tui = Tui::new(outer, Size::new(20, 8), Position::ORIGIN, 3, caps)?;
        let mut root = StubRoot {
            label: "world".into(),
            invalidated: 0,
        };
        tui.commit(Txn::Reanchor(ReanchorCause::Resize), &mut root)?;
        assert_eq!(root.invalidated, 1);
        assert_eq!(tui.write_count(), 1);
        let payload = tui.last_payload();
        let report = audit_bytes(payload);
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
        assert!(find_subslice(payload, b"\x1b[2K").is_some());
        Ok(())
    }

    #[test]
    fn stage3_partial_write_best_effort_closes_sync() {
        let framed = wrap_synchronized(b"payload", true);
        let mut writer = FailOnceWriter {
            fail_after: b"\x1b[?2026hpa".len(),
            ..FailOnceWriter::default()
        };

        let result = write_stage3_frame(&mut writer, &framed, true);
        assert!(result.is_err());
        let Err(error) = result else {
            return;
        };

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(writer.bytes.starts_with(b"\x1b[?2026hpa"));
        assert!(writer.bytes.ends_with(SYNC_OUTPUT_END));
        let audit = audit_bytes(&writer.bytes);
        assert_eq!(audit.sync_begin, 1);
        assert_eq!(audit.sync_end, 1);
    }

    #[test]
    fn stage3_flush_error_best_effort_closes_sync_and_returns_original_error() {
        let framed = wrap_synchronized(b"payload", true);
        let mut writer = FailOnceWriter {
            fail_after: usize::MAX,
            fail_flush: true,
            ..FailOnceWriter::default()
        };

        let result = write_stage3_frame(&mut writer, &framed, true);
        assert!(result.is_err());
        let Err(error) = result else {
            return;
        };

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "injected flush failure");
        assert!(writer.bytes.starts_with(&framed));
        assert!(writer.bytes.ends_with(SYNC_OUTPUT_END));
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
