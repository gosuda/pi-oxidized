//! `UiWriter` / `Tui` transaction pipeline and background coalescer.

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell, CellDiffOption, CellWidth};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::component::Component;
use crate::frame::{FrameAnnotations, RawRegion, RowClaim, RowClaims, with_annotations};
use crate::terminal::backend::{
    GuardedBackend, audit_bytes, encode_full_row_prefix, wrap_synchronized, wrap_synchronized_into,
};
use crate::terminal::caps::{TerminalCapabilities, kitty_delete_id};
use crate::terminal::sink::FrameSink;

const SYNC_OUTPUT_END: &[u8] = b"\x1b[?2026l";
// ── PERF-T11 paint-path probe (instrumented counter) ──────────────────────
//
// When armed by the churn-bench `--probe` (`set_paint_timer`), every
// committed frame accumulates the paint transaction — `emit_frame_diff`
// through `stage3_write`, i.e. the terminal-paint ledger unit (diff,
// encode, framing, write) — into these counters. One atomic load per
// frame when disarmed; zero timing work.

static PAINT_TIMER: AtomicBool = AtomicBool::new(false);
static PAINT_NANOS: AtomicU64 = AtomicU64::new(0);
static PAINT_DIFF_NANOS: AtomicU64 = AtomicU64::new(0);
static PAINT_FRAMES: AtomicU64 = AtomicU64::new(0);

fn paint_timer_on() -> bool {
    PAINT_TIMER.load(Ordering::Relaxed)
}

/// Arm/disarm the paint-path probe counter (churn-bench `--probe`).
pub fn set_paint_timer(enabled: bool) {
    PAINT_TIMER.store(enabled, Ordering::Relaxed);
}

/// Read the paint-path probe counters: `(total nanos, diff-phase nanos, frames)`.
///
/// The diff phase covers `emit_frame_diff` (damage diff + grid sync +
/// backend encode issue); the total adds the cursor sequence, composition
/// drain, framing audits, and the stage-3 write.
#[must_use]
pub fn paint_timer_read() -> (u64, u64, u64) {
    (
        PAINT_NANOS.load(Ordering::Relaxed),
        PAINT_DIFF_NANOS.load(Ordering::Relaxed),
        PAINT_FRAMES.load(Ordering::Relaxed),
    )
}

/// Reset the paint-path probe counters.
pub fn paint_timer_reset() {
    PAINT_NANOS.store(0, Ordering::Relaxed);
    PAINT_DIFF_NANOS.store(0, Ordering::Relaxed);
    PAINT_FRAMES.store(0, Ordering::Relaxed);
}

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
    /// An overlay opened over — or dismissed from — rows that hold
    /// unrelated content, so a cell diff against them would fragment the
    /// overlay's paint or leave dismissed remnants on screen.
    OverlayCover,
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
    /// Height requested by the app, floored at one row and *not* clamped to
    /// the terminal. A viewport temporarily clamped by a small terminal keeps
    /// this so it can regrow when the terminal grows back.
    requested_height: u16,
    /// Effective height: `requested_height` clamped into the terminal.
    viewport_height: u16,
    viewport_top: u16,
    live_kitty_ids: HashSet<u32>,
}

impl ViewportState {
    fn new(size: Size, cursor: Position, requested_height: u16) -> Self {
        let requested_height = requested_height.max(1);
        let viewport_height = Self::effective_height(requested_height, size.height);
        let viewport_top = cursor.y.saturating_sub(viewport_height.saturating_sub(1));
        Self {
            size,
            cursor,
            requested_height,
            viewport_height,
            viewport_top,
            live_kitty_ids: HashSet::new(),
        }
    }

    /// Clamp a request into the terminal, keeping at least one row.
    #[must_use]
    fn effective_height(requested_height: u16, terminal_height: u16) -> u16 {
        requested_height.min(terminal_height).max(1)
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
    /// Emitted-state snapshot: the cell grid as last flushed to the wire.
    /// The in-place render buffer is diffed against this per damaged row.
    grid: Buffer,
    prior_claims: Vec<Vec<RowClaim>>,
    /// Pooled frame-side claim table (PERF-T11 Design F): rows retain their
    /// capacity across frames so steady-state claim recording does not
    /// allocate.
    scratch_claims: Vec<Vec<RowClaim>>,
    /// Pooled frame-side changed-column table (PERF-T11 terminal-paint
    /// Design B): the producer-fed per-row damage ranges, reset in place
    /// across frames like the claim pool.
    changes_scratch: Vec<Option<(u16, u16)>>,
    /// Pooled frame update set (PERF-T11 terminal-paint Design A): the
    /// `(x, y, Cell)` list is taken per frame and returned after the
    /// backend encode, so steady-state paint allocates nothing for it.
    updates_scratch: Vec<(u16, u16, Cell)>,
    /// Pooled synchronized-output frame buffer; swaps with `last_payload`
    /// per stage-3 write so framing allocates nothing steady-state.
    frame_scratch: Vec<u8>,
    /// Pooled composition buffer swapped into the sink on each take.
    comp_scratch: Option<Vec<u8>>,
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
        // One clamp for both: the ratatui inline viewport and `state` must start
        // with the same effective height, and `viewport_top` is derived from it.
        let state = ViewportState::new(size, cursor, viewport_height);
        let terminal = Terminal::with_options(
            guarded,
            TerminalOptions {
                viewport: Viewport::Inline(state.viewport_height),
            },
        )?;
        outer.flush()?;
        Ok(Self {
            terminal,
            composition,
            outer,
            caps,
            state,
            coalescer: Coalescer::new(),
            write_count: 0,
            scratch_claims: Vec::new(),
            changes_scratch: Vec::new(),
            updates_scratch: Vec::new(),
            frame_scratch: Vec::new(),
            comp_scratch: None,
            last_payload: Vec::new(),
            grid: Buffer::default(),
            prior_claims: Vec::new(),
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

    /// Borrow the outer writer for mid-session probes (OSC 11 re-query).
    ///
    /// Writes must stay outside synchronized-output frames; the caller is
    /// responsible for not interleaving with an in-flight commit.
    pub fn outer_mut(&mut self) -> &mut W {
        &mut self.outer
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
        // Recompute from the retained request on grow *and* shrink: a viewport
        // clamped by a small terminal regrows when the terminal does.
        self.state.viewport_height =
            ViewportState::effective_height(self.state.requested_height, height);
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

        // Manual draw pipeline (PERF-T11 Design B). `Terminal::draw` resets
        // the render buffer every frame (swap_buffers) and diffs the whole
        // grid; instead, render in place — the current buffer is never reset
        // or swapped, so unchanged rows' cells survive — then diff only the
        // rows whose claim set changed against the emitted-state snapshot.
        // Emitted bytes match the whole-grid diff: cleanly skipped rows are
        // byte-equal to the snapshot by construction.
        self.terminal.autoresize()?;
        let frame_area = self.terminal.get_frame().area();
        if self.grid.area != frame_area {
            // Viewport geometry moved (first frame, resize, settle scroll,
            // viewport-height rebuild): realign the emitted snapshot to the
            // live buffer and drop every row claim.
            let current = std::mem::take(self.terminal.current_buffer_mut());
            self.grid = current.clone();
            *self.terminal.current_buffer_mut() = current;
            // The claim table covers absolute terminal rows
            // `area.y .. area.y + height`.
            self.prior_claims = vec![Vec::new(); usize::from(frame_area.bottom())];
        }
        let row_claims = self.prepare_pooled_claims(frame_area);
        annotations.borrow_mut().install_row_claims(row_claims);
        {
            let terminal = &mut self.terminal;
            with_annotations(&annotations, || {
                let mut frame = terminal.get_frame();
                let frame_area = frame.area();
                let height = root.measure(frame_area.width).min(frame_area.height);
                let render_area = Rect::new(frame_area.x, frame_area.y, frame_area.width, height);
                root.render(render_area, frame.buffer_mut());
            });
        }

        let mut collected = annotations.into_inner();
        let row_claims = collected.take_row_claims();
        let (cursor, raw_regions) = collected.into_parts();
        let (mut prior_table, frame_table, mut changes_table) = row_claims.into_tables();
        let paint_timed = paint_timer_on();
        let paint_t0 = paint_timed.then(Instant::now);
        self.emit_frame_diff(
            &prior_table,
            &frame_table,
            &changes_table,
            frame_area,
            full_rows,
        )?;
        self.prior_claims = frame_table;
        // Design F: the consumed prior table returns to the pool; its rows
        // keep the capacity this frame's recording will reuse next frame.
        for row in &mut prior_table {
            row.clear();
        }
        self.scratch_claims = prior_table;
        changes_table.fill(None);
        self.changes_scratch = changes_table;
        let paint_t1 = paint_timed.then(Instant::now);

        // Cursor sequence mirrors ratatui's `apply_buffer_with_cursor`:
        // updates, then show/set or hide, then the backend flush. Full-row
        // mode stays armed through the draw and clears after it.
        let cursor_position = hardware_cursor.then_some(cursor).flatten();
        match cursor_position {
            Some(position) => {
                self.terminal.show_cursor()?;
                self.terminal.set_cursor_position(position)?;
            }
            None => {
                self.terminal.hide_cursor()?;
            }
        }
        self.terminal.backend_mut().flush()?;
        self.terminal.backend_mut().set_full_rows(false);

        let mut payload = self.take_composition_bytes();
        self.append_frame_regions(&raw_regions, &mut payload);

        if let Some(cursor) = cursor {
            self.state.cursor = cursor;
            self.terminal.backend_mut().set_cursor_cache(cursor);
        }

        let result = self.stage3_write(&payload);
        // Return the drained payload's buffer to the composition pool for
        // the next frame's sink swap.
        payload.clear();
        self.comp_scratch = Some(payload);
        if let Some(t0) = paint_t0 {
            let end = Instant::now();
            PAINT_NANOS.fetch_add(
                u64::try_from(end.duration_since(t0).as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if let Some(t1) = paint_t1 {
                PAINT_DIFF_NANOS.fetch_add(
                    u64::try_from(t1.duration_since(t0).as_nanos()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
            }
            PAINT_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Build pooled `RowClaims` for the frame, reusing scratch tables when
    /// the row count matches and rebuilding them on geometry change.
    fn prepare_pooled_claims(&mut self, frame_area: Rect) -> RowClaims {
        let rows_needed = usize::from(frame_area.bottom());
        let mut frame_table = std::mem::take(&mut self.scratch_claims);
        if frame_table.len() == rows_needed {
            for row in &mut frame_table {
                row.clear();
            }
        } else {
            frame_table = vec![Vec::new(); rows_needed];
        }
        let mut changes_table = std::mem::take(&mut self.changes_scratch);
        if changes_table.len() == rows_needed {
            changes_table.fill(None);
        } else {
            changes_table = vec![None; rows_needed];
        }
        let mut row_claims = RowClaims::default();
        row_claims.install_pooled(
            std::mem::take(&mut self.prior_claims),
            frame_table,
            changes_table,
        );
        row_claims
    }

    /// Append raw regions and Kitty-image bookkeeping to the frame payload.
    fn append_frame_regions(&mut self, raw_regions: &[RawRegion], payload: &mut Vec<u8>) {
        let mut next_ids = HashSet::new();
        for region in raw_regions {
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
    }

    /// Emit the frame's cell updates, scoped to rows whose claim set changed.
    ///
    /// A row is provably unchanged when this frame's claim set equals the
    /// prior frame's and every claim is a keyed line claim: each claimant
    /// either repainted deterministically or skipped against an identical
    /// prior claim, and nothing else owns cells there. Dirty rows blank the
    /// spans of vanished claimants (not covered by a current claim), diff
    /// against the emitted snapshot with ratatui's own per-cell semantics,
    /// and sync the snapshot.
    ///
    /// PERF-T11 terminal-paint Design B: on rows whose every writer is a
    /// recording line painter, the walk window narrows from the claim-span
    /// union to the producer-recorded changed columns (`narrowed_walk_span`);
    /// anything else keeps the Design A span. Debug builds assert full-row
    /// snapshot exactness on narrowed rows, so a missed recording fails
    /// every debug test, not the wire.
    fn emit_frame_diff(
        &mut self,
        prior: &[Vec<RowClaim>],
        frame: &[Vec<RowClaim>],
        changes: &[Option<(u16, u16)>],
        area: Rect,
        force_full_rows: bool,
    ) -> io::Result<()> {
        let width = usize::from(area.width);
        let rows = usize::from(area.height);
        let base = usize::from(area.y);
        // PERF-T11 terminal-paint Design A: the update set is pooled and the
        // emitted-state sync is fused into the diff walk (see
        // `push_row_diff`) — steady-state paint allocates nothing and no
        // full-row snapshot copy runs per damaged row.
        let mut updates = std::mem::take(&mut self.updates_scratch);
        updates.clear();
        if width > 0 {
            // Claim tables are indexed by absolute terminal row (painters
            // record absolute `y`); grid and buffer content slices are
            // row-relative.
            for y_abs in base..base + rows {
                let prior_row = prior.get(y_abs).map_or([].as_slice(), Vec::as_slice);
                let frame_row = frame.get(y_abs).map_or([].as_slice(), Vec::as_slice);
                if !frame_row.is_empty()
                    && claims_equal(prior_row, frame_row)
                    && frame_row.iter().all(RowClaim::is_line)
                {
                    continue;
                }
                let rel = y_abs - base;
                let (start, end) = (rel * width, rel * width + width);
                let row_y = u16::try_from(y_abs).unwrap_or(u16::MAX);
                let current = self.terminal.current_buffer_mut();
                blank_vanished_spans(prior_row, frame_row, row_y, width, current);
                let outer = row_walk_span(prior_row, frame_row, width);
                let change = changes.get(y_abs).copied().flatten();
                // Reanchor/full-row mode: the emitted-state snapshot may be
                // stale for covered rows, so neither claim narrowing nor
                // cell-level skip suppression may drop content — walk the
                // whole row and force every cell out.
                let (from, to) = if force_full_rows {
                    (0, width)
                } else {
                    narrowed_walk_span(prior_row, frame_row, change, outer)
                };
                let prev = &mut self.grid.content[start..end];
                let next = &current.content[start..end];
                push_row_diff(
                    prev,
                    next,
                    row_y,
                    area.x,
                    from,
                    to,
                    force_full_rows,
                    &mut updates,
                );
                #[cfg(debug_assertions)]
                if (from, to) != outer {
                    // Narrowed window: the producer feed claims everything
                    // outside the recorded columns is unchanged, so the
                    // whole row must be snapshot-exact after the walk.
                    for (p, n) in prev.iter().zip(next.iter()) {
                        debug_assert!(
                            p == n,
                            "narrowed walk left row {row_y} snapshot-stale outside {from}..{to}"
                        );
                    }
                }
            }
        }
        let result = self
            .terminal
            .backend_mut()
            .draw(updates.iter().map(|(x, y, cell)| (*x, *y, cell)));
        self.updates_scratch = updates;
        result
    }

    /// Drop all damage-scoping state; the next frame fully repaints and
    /// realigns the emitted snapshot to the live buffer.
    fn invalidate_damage(&mut self) {
        self.grid = Buffer::default();
        self.prior_claims.clear();
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

        // insert_before scrolled the viewport: the emitted snapshot must
        // realign to the shifted buffer before the redraw diffs against it.
        self.invalidate_damage();
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
        let mut payload_prefix = Vec::new();
        for id in &self.state.live_kitty_ids {
            payload_prefix.extend_from_slice(&kitty_delete_id(*id));
        }
        self.state.live_kitty_ids.clear();
        // Park the real cursor on the viewport's first row before rebuilding, so
        // the height-minus-one inline initialization LFs end on the last terminal
        // row without scrolling, wherever reflow left the cursor.
        payload_prefix.extend_from_slice(
            format!("\x1b[{};1H", self.state.viewport_top.saturating_add(1)).as_bytes(),
        );
        self.push_composition_bytes(&payload_prefix);
        // `Viewport::Inline(h)` is immutable and `Terminal::resize` recomputes the
        // origin from cursor offsets, so rebuild the terminal: its inline height,
        // area, and known size must equal `state` before the full-row redraw.
        self.rebuild_terminal()?;
        // Full-row reanchor: drop claims so every row reaches the wire.
        self.invalidate_damage();
        self.commit_frame(root, true)
    }

    fn commit_set_viewport_height(
        &mut self,
        height: u16,
        root: &mut dyn Component,
    ) -> io::Result<()> {
        // Retain the normalized request before any clamp so a height set while
        // the terminal is too small takes effect when the terminal regrows.
        self.state.requested_height = height.max(1);
        let height =
            ViewportState::effective_height(self.state.requested_height, self.state.size.height);
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

        // Rebuild so terminal geometry equals `state`; the scroll / abandoned-row
        // bytes above stay in front of the initialization bytes.
        self.rebuild_terminal()?;
        // Terminal rebuilt with fresh buffers: emitted snapshot restarts.
        self.invalidate_damage();
        self.commit_frame(root, true)
    }

    /// Rebuild the ratatui terminal so its inline geometry equals `self.state`:
    /// `Viewport::Inline(viewport_height)` anchored at `(0, viewport_top)`, hence
    /// `viewport_area() == state.viewport_area()` and the backend's known size is
    /// `state.size` (the `autoresize` in `commit_frame` stays a no-op). Any bytes
    /// already staged in the composition are preserved ahead of the initialization
    /// bytes the constructor may emit.
    fn rebuild_terminal(&mut self) -> io::Result<()> {
        let pending = self.take_composition_bytes();
        let sink = FrameSink::with_shared(Arc::clone(&self.composition));
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
                viewport: Viewport::Inline(self.state.viewport_height),
            },
        )?;
        // Initialization may emit scroll/cursor bytes. Preserve transaction
        // order: staged bytes precede initialization and the redraw.
        let initialization = self.take_composition_bytes();
        self.push_composition_bytes(&pending);
        self.push_composition_bytes(&initialization);
        Ok(())
    }

    fn push_composition_bytes(&self, bytes: &[u8]) {
        if let Ok(mut guard) = self.composition.lock() {
            guard.extend_from_slice(bytes);
        }
    }

    fn take_composition_bytes(&mut self) -> Vec<u8> {
        // PERF-T11 terminal-paint Design A: swap the pooled buffer into the
        // sink instead of leaving it a fresh empty Vec, so the composition
        // growth per frame reuses retained capacity.
        let scratch = self.comp_scratch.take().unwrap_or_default();
        self.composition
            .lock()
            .map(|mut guard| std::mem::replace(&mut *guard, scratch))
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
        let framed = &mut self.frame_scratch;
        wrap_synchronized_into(framed, payload, self.caps.sync_output);
        let framed_report = audit_bytes(framed);
        if framed_report.sync_begin != framed_report.sync_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unbalanced synchronized output markers",
            ));
        }
        write_stage3_frame(
            &mut self.outer,
            framed,
            self.caps.sync_output && !payload.is_empty(),
        )?;
        self.write_count = self.write_count.saturating_add(1);
        // Rotate the pooled frame buffer through `last_payload`: the test
        // surface keeps the exact last stage-3 payload, and the buffer it
        // displaced becomes the next frame's scratch (no steady-state
        // framing allocation).
        std::mem::swap(&mut self.last_payload, &mut self.frame_scratch);
        // Straggler bytes flushed after the payload take keep their prior
        // discard semantics; the pooled sink buffer stays installed with its
        // capacity.
        if let Ok(mut guard) = self.composition.lock() {
            guard.clear();
        }
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

fn claims_equal(a: &[RowClaim], b: &[RowClaim]) -> bool {
    a.len() == b.len() && a.iter().all(|claim| b.contains(claim))
}

/// Column window a damaged row's diff walk must cover.
///
/// PERF-T11 terminal-paint Design A (span feeding): when every claim on
/// both sides is spanned, changed cells can only lie inside the union of
/// the prior and frame claim spans — writers write inside their claims
/// (the Design B writer contract), and vanished-claim blanking stays
/// inside prior spans. Any foreign claim, or a row with no frame claims
/// (unclaimed writers, suspended recording), keeps the full row.
fn row_walk_span(prior: &[RowClaim], frame: &[RowClaim], width: usize) -> (usize, usize) {
    if frame.is_empty() {
        return (0, width);
    }
    let full = (0, width);
    let mut lo = usize::from(u16::MAX);
    let mut hi = 0usize;
    for claim in prior.iter().chain(frame) {
        match claim.span() {
            Some((x, span_width)) => {
                lo = lo.min(usize::from(x));
                hi = hi.max(usize::from(x.saturating_add(span_width)));
            }
            None => return full,
        }
    }
    if hi <= lo {
        return full;
    }
    (lo.min(width), hi.min(width))
}

/// Narrow a damaged row's walk window to the producer-recorded changed
/// columns (PERF-T11 terminal-paint Design B).
///
/// The narrowing is trusted only when the row's writers are exactly the
/// recording painters, so every write this frame went through a
/// compare-before-write:
///
/// - every current claimant is a keyed line claim (`paint_line` paths —
///   opaque and foreign writers do not record, so their rows keep the
///   outer span), and
/// - every vanished prior claimant's span is fully covered by one current
///   line span — the same condition under which `blank_vanished_spans`
///   writes nothing, so the repaint's compare-before-write saw that
///   content and recorded anything it actually changed. An uncovered (or
///   foreign) vanished span keeps the outer span.
///
/// Under those conditions a `None` record means the repaint changed
/// nothing (empty window); a recorded range is clamped into the outer
/// span. `Cell::eq` still runs at every walked column, so an
/// over-record can only cost time, never change the emitted bytes.
/// Debug builds verify the proof per damaged row (`emit_frame_diff`).
fn narrowed_walk_span(
    prior: &[RowClaim],
    frame: &[RowClaim],
    change: Option<(u16, u16)>,
    outer: (usize, usize),
) -> (usize, usize) {
    if frame.is_empty() || outer.0 >= outer.1 {
        return outer;
    }
    if !frame.iter().all(RowClaim::is_line) {
        return outer;
    }
    for claim in prior {
        if frame.contains(claim) {
            continue;
        }
        let Some((x, span_width)) = claim.span() else {
            return outer;
        };
        let from = usize::from(x);
        let to = from.saturating_add(usize::from(span_width));
        let covered = frame.iter().any(|current| {
            current.span().is_some_and(|(cx, cw)| {
                let cfrom = usize::from(cx);
                let cto = cfrom.saturating_add(usize::from(cw));
                cfrom <= from && to <= cto
            })
        });
        if !covered {
            return outer;
        }
    }
    let Some((lo, hi)) = change else {
        return (outer.0, outer.0);
    };
    let from = usize::from(lo);
    let to = usize::from(hi).saturating_add(1);
    (from.clamp(outer.0, outer.1), to.clamp(outer.0, outer.1))
}

/// Blank the spans of vanished claimants on one row.
///
/// A prior claim absent from this frame means its painter went away; the
/// cells it covered must return to default (reset-buffer semantics) unless a
/// current claim's span already accounts for them. Foreign claims cover the
/// whole row.
fn blank_vanished_spans(
    prior: &[RowClaim],
    frame: &[RowClaim],
    y: u16,
    width: usize,
    buf: &mut Buffer,
) {
    if prior.is_empty() {
        return;
    }
    // A foreign current claim covers the whole row; nothing to blank.
    if frame.iter().any(|claim| claim.span().is_none()) {
        return;
    }
    for claim in prior {
        if frame.contains(claim) {
            continue;
        }
        let (from, to) = match claim.span() {
            Some((x, span_width)) => (x, x.saturating_add(span_width)),
            None => (0, u16::try_from(width).unwrap_or(u16::MAX)),
        };
        // Fast path (the churn case: one line claim replaced by another
        // with the same span): a single current span containing the
        // vanished span blanks nothing, without a per-column walk.
        if frame.iter().any(|current| {
            current.span().is_some_and(|(cx, cw)| {
                cx <= from && to <= cx.saturating_add(cw) && cx < cw.saturating_add(cx)
            })
        }) {
            continue;
        }
        'col: for col in from..to {
            for current in frame {
                if let Some((cx, cw)) = current.span()
                    && col >= cx
                    && col < cx.saturating_add(cw)
                {
                    continue 'col;
                }
            }
            if let Some(cell) = buf.cell_mut((col, y)) {
                cell.reset();
            }
        }
    }
}

/// Skip belief for one buffer cell: an explicit opt-out, or the legacy flag
/// with no explicit option set.
#[allow(deprecated)]
fn is_skip_cell(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::Skip)
        || (cell.skip && matches!(cell.diff_option, CellDiffOption::None))
}

/// Fused sync: copy the buffer cell into the snapshot when they differ.
fn sync_snapshot_cell(prev: &mut [Cell], next: &[Cell], j: usize) {
    if prev[j] != next[j] {
        prev[j] = next[j].clone();
    }
}

/// Drain a pending wide-grapheme trailing run: emit the first cell whose
/// symbol changed on screen (or every visited cell on a forced pass) and
/// sync the rest into the snapshot. Returns the re-armed run when emission
/// stopped mid-run, otherwise `None` with the resume column.
#[allow(clippy::too_many_arguments)]
fn drain_trailing_run(
    prev: &mut [Cell],
    next: &[Cell],
    x0: u16,
    y: u16,
    len: usize,
    mut next_index: usize,
    mut end: usize,
    emit_trailing: bool,
    out: &mut Vec<(u16, u16, Cell)>,
) -> (Option<(usize, usize, bool)>, usize) {
    while next_index < end {
        let j = next_index;
        let cell_width = next[j].cell_width().max(1) as usize;
        next_index += cell_width;
        end = end.max(next_index).min(len);
        if !is_skip_cell(&next[j]) && (emit_trailing || prev[j].symbol() != next[j].symbol()) {
            let x = x0.saturating_add(u16::try_from(j).unwrap_or(u16::MAX));
            out.push((x, y, next[j].clone()));
            sync_snapshot_cell(prev, next, j);
            return (Some((next_index, end, emit_trailing)), next_index);
        }
        sync_snapshot_cell(prev, next, j);
    }
    (None, end)
}

/// Row-scoped port of ratatui's `BufferDiff` iterator (`ratatui-core` 0.1.2).
///
/// Emits the same `(x, y, cell)` update stream the whole-grid diff produces
/// for this row: skip cells are never emitted, wide graphemes advance past
/// their continuation columns, VS16 sequences get trailing columns checked,
/// and a replaced wide grapheme whose style was visible on blank trailing
/// cells force-refreshes the trailing range. Painters never place a wide
/// grapheme in a row's final column, so trailing state stays within the row.
///
/// PERF-T11 terminal-paint Design A: the walk is *fused* with the
/// emitted-state sync — every cell the walk finds unequal is copied into
/// `prev` (the snapshot slice) in place, replacing the previous full-row
/// `clone_from_slice` per damaged row, and *windowed* to `[from, to)` (see
/// [`row_walk_span`]); a grapheme starting inside the window still runs its
/// continuation/trailing range to completion exactly as a full-row walk
/// would. `Cell::eq` normalizes `None` and `" "` symbols, so a snapshot
/// cell left un-copied because it compares equal stays observably identical
/// (eq, `symbol()`, `cell_width()` all normalize).
#[allow(clippy::too_many_arguments)]
fn push_row_diff(
    prev: &mut [Cell],
    next: &[Cell],
    y: u16,
    x0: u16,
    from: usize,
    to: usize,
    force: bool,
    out: &mut Vec<(u16, u16, Cell)>,
) {
    /// Modifiers visually apparent on a blank (space) cell.
    const VISIBLE_ON_BLANK: Modifier = Modifier::REVERSED
        .union(Modifier::UNDERLINED)
        .union(Modifier::SLOW_BLINK)
        .union(Modifier::RAPID_BLINK)
        .union(Modifier::CROSSED_OUT);

    let len = prev.len().min(next.len());
    let to = to.min(len);
    let mut pos = from.min(to);
    // Pending trailing cells after a wide character: `(next index, end, force)`.
    let mut trailing: Option<(usize, usize, bool)> = None;
    while pos < to || trailing.is_some() {
        if let Some((next_index, end, trailing_force)) = trailing.take() {
            let (rearmed, resume) =
                drain_trailing_run(prev, next, x0, y, len, next_index, end, trailing_force, out);
            trailing = rearmed;
            if trailing.is_none() {
                pos = resume.max(pos);
            }
            continue;
        }
        if pos >= to {
            break;
        }
        let i = pos;
        pos += 1;
        let current = &next[i];
        let x = x0.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        match current.diff_option {
            CellDiffOption::Skip if !force => {
                sync_snapshot_cell(prev, next, i);
            }
            CellDiffOption::Skip => {
                // Forced (reanchor) emission: the skip flag reflects a
                // same-screen belief that may be stale for covered rows.
                let cell_width = current.cell_width().max(1) as usize;
                out.push((x, y, current.clone()));
                sync_snapshot_cell(prev, next, i);
                pos += cell_width.saturating_sub(1);
                for j in (i + 1)..pos.min(len) {
                    sync_snapshot_cell(prev, next, j);
                }
            }
            _ if is_skip_cell(current) && !force => {
                sync_snapshot_cell(prev, next, i);
            }
            CellDiffOption::ForcedWidth(width) => {
                let emit = force || *current != prev[i];
                pos = pos.saturating_add(width.get().saturating_sub(1) as usize);
                for j in i..pos.min(len) {
                    sync_snapshot_cell(prev, next, j);
                }
                if emit {
                    out.push((x, y, current.clone()));
                }
            }
            CellDiffOption::None | CellDiffOption::AlwaysUpdate => {
                let cell_width = current.cell_width() as usize;
                if !force
                    && matches!(current.diff_option, CellDiffOption::None)
                    && *current == prev[i]
                {
                    // Head is equal (just compared) — sync only the wide
                    // grapheme's continuation columns; the old bulk row copy
                    // paid a full-cell move for every equal cell.
                    for j in (i + 1)..(i + cell_width).min(len) {
                        sync_snapshot_cell(prev, next, j);
                    }
                    pos += cell_width.saturating_sub(1);
                    continue;
                }
                // Prev-derived values are read branch-locally: `sync` takes
                // `prev` mutably, and `cell_width()` is unicode-width work
                // too expensive to pay per equal cell.
                let prev_width = prev[i].cell_width() as usize;
                let prev_visible =
                    prev[i].bg != Color::Reset || prev[i].modifier.intersects(VISIBLE_ON_BLANK);
                let contains_vs16 =
                    cell_width > 1 && current.symbol().chars().any(|c| c == '\u{FE0F}');
                if contains_vs16 {
                    trailing = Some((i + 1, (i + cell_width).min(len), false));
                } else if cell_width > 1 {
                    pos += cell_width.saturating_sub(1);
                    for j in (i + 1)..(i + cell_width).min(len) {
                        sync_snapshot_cell(prev, next, j);
                    }
                } else if prev_width > cell_width && prev_visible {
                    trailing = Some((i + 1, i + prev_width, true));
                }
                out.push((x, y, current.clone()));
                sync_snapshot_cell(prev, next, i);
            }
        }
    }
    #[cfg(debug_assertions)]
    for j in from.min(len)..to {
        debug_assert!(
            prev[j] == next[j],
            "fused diff sync left snapshot column {j} stale on row {y}"
        );
    }
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
    use std::num::NonZeroU16;

    /// PERF-T11 terminal-paint Design A contract: the fused windowed diff
    /// walk must (a) emit exactly the cells a full-row scan of the same
    /// semantics would emit, and (b) leave the snapshot (`prev`) equal to
    /// the buffer row (`next`) over the walked window — the property the
    /// old post-walk full-row `clone_from_slice` provided.
    #[test]
    fn fused_diff_sync_matches_full_row_copy_semantics() {
        let mk = |sym: &'static str| Cell::new(sym);
        // Crafted row covering every walk branch.
        let mut next = vec![mk("x"); 8];
        let mut prev = next.clone();

        // 0: equal plain cell (no emit, no copy needed).
        // 1: changed symbol (emit + sync).
        next[1] = mk("y");
        // 2: skip cell differing from the snapshot: never emitted, but the
        // old full-row copy synced it.
        let mut skip_cell = mk("z");
        skip_cell.set_diff_option(CellDiffOption::Skip);
        next[2] = skip_cell;
        // 3-4: equal wide grapheme: head not emitted, continuation synced.
        next[3] = mk("瓦");
        prev[3] = mk("瓦");
        next[4] = mk("");
        prev[4] = mk("");
        // 5: narrow cell replacing a previous wide grapheme whose trailing
        // cell carried a visible background: force-refreshes the trailing
        // range (emits 5 and 6).
        let mut old_wide = mk("骨");
        old_wide.bg = Color::Red;
        prev[5] = old_wide;
        prev[6] = mk("");
        next[5] = mk("n");
        next[6] = mk("");
        // 7: ForcedWidth cell equal to the snapshot: advances past its span,
        // not emitted.
        let mut forced = mk("f");
        forced.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
        next[7] = forced.clone();
        prev[7] = forced;

        let reference = next.clone();
        let mut updates = Vec::new();
        let mut snapshot = prev.clone();
        push_row_diff(&mut snapshot, &next, 4, 10, 0, 8, false, &mut updates);

        // (a) emission: the changed symbol (x=11), the narrow replacement
        // (x=15), and its force-refreshed trailing cell (x=16).
        let xs: Vec<u16> = updates.iter().map(|(x, _, _)| *x).collect();
        assert_eq!(xs, vec![11, 15, 16]);
        // (b) fused sync: snapshot equals the buffer row over the window.
        assert_eq!(snapshot, reference);

        // Windowed call over [2, 6): the narrow replacement inside the
        // window is emitted, and its force-trailing range runs past the
        // window end to completion (full-row semantics preserved for
        // boundary-crossing graphemes).
        let mut updates2 = Vec::new();
        let mut snapshot2 = prev.clone();
        push_row_diff(&mut snapshot2, &next, 4, 10, 2, 6, false, &mut updates2);
        let xs2: Vec<u16> = updates2.iter().map(|(x, _, _)| *x).collect();
        assert_eq!(xs2, vec![15, 16]);
        for j in 2..7 {
            assert_eq!(snapshot2[j], reference[j], "window column {j} synced");
        }
    }

    /// Forced (reanchor) row emission must emit every cell even when the
    /// emitted snapshot claims the row is already on screen: the gauntlet
    /// wizard dismissal once emitted `typ`, skipped the two cells a stale
    /// snapshot believed present, then `a message`.
    #[test]
    fn forced_row_diff_emits_every_cell_despite_stale_snapshot() {
        fn mk(sym: &str) -> Cell {
            let mut cell = Cell::default();
            cell.set_symbol(sym);
            cell
        }
        let text = "type a message";
        let width = text.chars().count();
        let mut next = vec![mk(" "); width];
        for (j, c) in text.chars().enumerate() {
            next[j].set_symbol(&c.to_string());
        }
        // Stale belief one: the snapshot claims the row is already on
        // screen, and every third cell carries an explicit skip flag.
        let mut believed = next.clone();
        for cell in believed.iter_mut().step_by(3) {
            cell.set_diff_option(CellDiffOption::Skip);
        }
        // Unforced control: an equal snapshot suppresses everything.
        let mut quiet = Vec::new();
        let mut quiet_prev = next.clone();
        push_row_diff(&mut quiet_prev, &next, 1, 0, 0, width, false, &mut quiet);
        assert!(
            quiet.is_empty(),
            "equal snapshot must suppress unforced emission"
        );

        // Forced (reanchor): every column emitted exactly once, in order.
        let mut updates = Vec::new();
        let mut snapshot = believed.clone();
        push_row_diff(&mut snapshot, &next, 1, 0, 0, width, true, &mut updates);
        let xs: Vec<u16> = updates.iter().map(|(x, _, _)| *x).collect();
        let expected: Vec<u16> = (0..u16::try_from(width).unwrap_or(u16::MAX)).collect();
        assert_eq!(xs, expected);
        let rendered: String = updates.iter().map(|(_, _, c)| c.symbol()).collect();
        assert_eq!(rendered, text);
        assert_eq!(snapshot, next);

        // Stale belief two: the snapshot holds unrelated overlay content.
        let mut stale = vec![mk("#"); width];
        let mut updates2 = Vec::new();
        push_row_diff(&mut stale, &next, 1, 0, 0, width, true, &mut updates2);
        let xs2: Vec<u16> = updates2.iter().map(|(x, _, _)| *x).collect();
        assert_eq!(xs2, expected);
        assert_eq!(stale, next);
    }

    /// `row_walk_span` unions prior and frame claim spans; foreign claims or
    /// claim-less frames keep the full row.
    #[test]
    fn row_walk_span_unions_claims_and_falls_back_to_full_row() {
        let line = |x: u16, w: u16| RowClaim::Line {
            x,
            width: w,
            key: 7,
            linked: false,
        };
        assert_eq!(row_walk_span(&[], &[line(4, 8)], 100), (4, 12));
        assert_eq!(row_walk_span(&[line(60, 10)], &[line(4, 8)], 100), (4, 70));
        assert_eq!(
            row_walk_span(&[RowClaim::Foreign], &[line(4, 8)], 100),
            (0, 100)
        );
        assert_eq!(row_walk_span(&[line(4, 8)], &[], 100), (0, 100));
        // Spans clamp to the row width.
        assert_eq!(row_walk_span(&[], &[line(90, 20)], 100), (90, 100));
    }

    /// `narrowed_walk_span` (terminal-paint Design B): recorded columns
    /// clamp into the outer span; opaque/foreign claimants, uncovered
    /// vanished spans, and claim-less frames keep the outer span; a
    /// `None` record yields an empty window.
    #[test]
    fn narrowed_walk_span_trusts_only_recorded_rows() {
        let line = |x: u16, w: u16| RowClaim::Line {
            x,
            width: w,
            key: 7,
            linked: false,
        };
        let opaque = |x: u16, w: u16| RowClaim::Opaque { x, width: w };
        // All-line frame: the recorded range clamps into the outer span.
        assert_eq!(
            narrowed_walk_span(&[], &[line(0, 100)], Some((40, 43)), (0, 100)),
            (40, 44)
        );
        // Over-record beyond the outer span clamps on both edges.
        assert_eq!(
            narrowed_walk_span(&[], &[line(10, 10)], Some((0, 99)), (10, 20)),
            (10, 20)
        );
        // No recorded change: empty window (repaint changed nothing).
        assert_eq!(
            narrowed_walk_span(&[], &[line(0, 100)], None, (0, 100)),
            (0, 0)
        );
        // An opaque frame claimant keeps the outer span.
        assert_eq!(
            narrowed_walk_span(&[], &[opaque(0, 100)], Some((40, 43)), (0, 100)),
            (0, 100)
        );
        // A vanished prior span not covered by a current line span keeps
        // the outer span (blanking may write there unrecorded).
        assert_eq!(
            narrowed_walk_span(&[line(50, 10)], &[line(0, 10)], Some((2, 5)), (0, 10)),
            (0, 10)
        );
        // A vanished prior span fully inside a current line span narrows.
        assert_eq!(
            narrowed_walk_span(&[line(0, 8)], &[line(0, 10)], Some((9, 9)), (0, 10)),
            (9, 10)
        );
        // A vanished foreign prior claim keeps the outer span.
        assert_eq!(
            narrowed_walk_span(
                &[RowClaim::Foreign],
                &[line(0, 100)],
                Some((1, 2)),
                (0, 100)
            ),
            (0, 100)
        );
        // Claim-less frames (suspended recording) keep the outer span.
        assert_eq!(
            narrowed_walk_span(&[], &[], Some((1, 2)), (0, 100)),
            (0, 100)
        );
    }

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
        let delete = b"\x1b_Ga=d,d=I,i=9\x1b\\";
        let delete_pos =
            find_subslice(bytes, delete).ok_or_else(|| io::Error::other("missing Kitty delete"))?;
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
    fn markdown_hyperlink_region_reaches_terminal_payload() -> io::Result<()> {
        use crate::components::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};

        struct MarkdownRoot;
        impl Component for MarkdownRoot {
            fn measure(&mut self, _width: u16) -> u16 {
                1
            }
            fn render(&mut self, area: Rect, buf: &mut Buffer) {
                let mut m = Markdown::new(
                    "[label](https://example.com)",
                    0,
                    0,
                    MarkdownTheme::default(),
                    DefaultTextStyle::default(),
                    MarkdownOptions {
                        hyperlinks: true,
                        ..Default::default()
                    },
                );
                m.render(area, buf);
            }
            fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
                EventResult::Ignored
            }
            fn invalidate(&mut self) {}
        }

        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let outer = Cursor::new(Vec::new());
        let mut tui = Tui::new(outer, Size::new(40, 8), Position::ORIGIN, 3, caps)?;
        tui.commit(Txn::Frame, &mut MarkdownRoot)?;
        let payload = tui.last_payload();
        let text = String::from_utf8_lossy(payload).into_owned();
        // The hyperlink region replays as: save cursor, absolute position,
        // verbatim OSC 8 open + styled label + close (+ reset guard), restore.
        let open = "\u{1b}]8;;https://example.com\u{1b}\\";
        let close = "\u{1b}]8;;\u{1b}\\";
        assert!(
            text.contains(&format!("\u{1b}7\u{1b}[1;1H{open}")),
            "region replay must be cursor-saved, positioned, and open-first: {text:?}"
        );
        let open_at = text
            .find(open)
            .ok_or_else(|| io::Error::other("OSC 8 open missing from payload"))?;
        let close_at = text[open_at..]
            .find(close)
            .ok_or_else(|| io::Error::other("OSC 8 close missing from payload"))?
            + open_at;
        assert!(
            text[open_at..close_at].contains("label"),
            "label rides the region: {text:?}"
        );
        assert!(
            text.contains(&format!("{close}\u{1b}[0m\u{1b}8")),
            "region ends with close + SGR reset + cursor restore: {text:?}"
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

    /// A viewport clamped by a temporarily small terminal keeps the requested
    /// height and regrows with the terminal — including a request changed while
    /// the clamp was in force.
    #[test]
    fn clamped_viewport_regrows_to_retained_request() -> io::Result<()> {
        let caps = TerminalCapabilities {
            sync_output: true,
            ..TerminalCapabilities::default()
        };
        let outer = Cursor::new(Vec::new());
        // Requested 6 rows in a 4-row terminal: the effective height clamps.
        let mut tui = Tui::new(outer, Size::new(20, 4), Position::ORIGIN, 6, caps)?;
        let mut root = StubRoot {
            label: "live".into(),
            invalidated: 0,
        };
        tui.commit(Txn::Frame, &mut root)?;
        assert_eq!(tui.viewport_height(), 4);

        // Regrow: the retained request restores six rows, bottom-anchored.
        tui.note_resize(20, 24);
        assert_eq!(tui.viewport_height(), 6);
        tui.commit(Txn::Reanchor(ReanchorCause::Resize), &mut root)?;
        let payload = tui.last_payload().to_vec();
        let first_row = find_subslice(&payload, b"\x1b[19;1H\x1b[2K")
            .ok_or_else(|| io::Error::other("missing restored first row"))?;
        let last_row = find_subslice(&payload, b"\x1b[24;1H\x1b[2K")
            .ok_or_else(|| io::Error::other("missing restored last row"))?;
        let content = find_subslice(&payload, b"live")
            .ok_or_else(|| io::Error::other("missing regrown viewport content"))?;
        assert!(first_row < content && content < last_row);

        // Shrink again, then change the request while clamped: the effective
        // height cannot move yet, but the new request must survive.
        tui.note_resize(20, 4);
        assert_eq!(tui.viewport_height(), 4);
        tui.commit(Txn::SetViewportHeight(8), &mut root)?;
        assert_eq!(tui.viewport_height(), 4);
        tui.note_resize(20, 24);
        assert_eq!(tui.viewport_height(), 8);
        tui.commit(Txn::Reanchor(ReanchorCause::Resize), &mut root)?;
        let payload = tui.last_payload();
        let report = audit_bytes(payload);
        assert_eq!(report.clear_2j, 0);
        assert_eq!(report.clear_3j, 0);
        assert!(
            find_subslice(payload, b"\x1b[17;1H\x1b[2K").is_some(),
            "eight rows bottom-anchor at row 17: {:?}",
            String::from_utf8_lossy(payload)
        );
        assert!(find_subslice(payload, b"live").is_some());
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
