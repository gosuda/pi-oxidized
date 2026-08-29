//! Render-churn benchmark matching the upstream TUI churn workload.
//!
//! Mirrors `.references/pi/packages/tui/test/render-churn-bench.ts`:
//! - 100×30 viewport
//! - 150-line transcript/dock tree: VStack [ ScrollView(transcript), dock
//!   VStack [status, editor, footer] ]
//! - 20 warmups, 300 frames
//! - Two scenarios: static (nothing changes) and editor (one char appended
//!   per frame)
//! - Null-sink terminal (discards output)
//!
//! Reports wall ms/frame and allocated bytes/frame, paired with the upstream
//! numbers for comparison.
//!
//! Allocation is measured with a counting global allocator
//! (`pi-bench-alloc`) that wraps `std::alloc::System` and atomically counts
//! every `alloc`/`realloc` byte — measuring churn, not retention, since
//! deallocated bytes are tracked separately.
//!
//! Run:
//!   cargo run -p pi-tui --release --bin pi_tui_render_churn_bench

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Instant;

use pi_bench_alloc::CountingAllocator;
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::Text;
use pi_tui::components::util::{KeyedLine, paint_lines_keyed};
use pi_tui::terminal::{
    TerminalCapabilities, Tui, Txn, paint_timer_read, paint_timer_reset, set_paint_timer,
};

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};

// ── Global allocator: counts all allocations ──────────────────────────────

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// ── Parameters (diff-checked against render-churn-bench.ts) ───────────────

const COLUMNS: u16 = 100;
const ROWS: u16 = 30;
const WARMUP_FRAMES: usize = 20;
const FRAMES: usize = 300;

// ── Null-sink writer ──────────────────────────────────────────────────────

/// Writer that discards all output, mirroring `NullTerminal` in the upstream
/// benchmark.  Keeps terminal I/O out of the measurement.
struct NullWriter {
    bytes_written: u64,
}

impl NullWriter {
    const fn new() -> Self {
        Self { bytes_written: 0 }
    }
}

impl Write for NullWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes_written += u64::try_from(buf.len()).unwrap_or(u64::MAX);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ── Editor stand-in ───────────────────────────────────────────────────────

/// Editor stand-in that caches lines per (text, width), re-renders when text
/// changes.  Mirrors `EditorSim` in the upstream benchmark.
struct EditorSim {
    text: String,
    cached_text: Option<String>,
    cached_width: Option<u16>,
    cached_lines: Option<Vec<KeyedLine>>,
}

impl EditorSim {
    fn new() -> Self {
        Self {
            text: String::new(),
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    fn append(&mut self, ch: char) {
        self.text.push(ch);
    }
}

impl EditorSim {
    /// Floor-probe helper: rotate the last three characters by the frame
    /// index so the content is unique for 26³ = 17,576 frames — every frame
    /// is a fresh key (the pinned append scenario's path: memo miss, full
    /// derive) — while the length, and therefore the rebuild's format cost,
    /// stays at steady state. Probe-only; the pinned scenario appends.
    fn rotate_unique(&mut self, i: usize) {
        let rot = |i: usize| 97 + u8::try_from(i % 26).unwrap_or(0);
        let n = self.text.len();
        if n < 3 {
            return;
        }
        let mut bytes = self.text.clone().into_bytes();
        bytes[n - 3] = rot(i);
        bytes[n - 2] = rot(i / 26);
        bytes[n - 1] = rot(i / 676);
        self.text = String::from_utf8(bytes).unwrap_or_default();
    }
}

impl Component for EditorSim {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        paint_lines_keyed(area, buf, lines);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }
}

impl EditorSim {
    fn lines_for_width(&mut self, width: u16) -> &[KeyedLine] {
        let cache_hit = self.cached_width == Some(width)
            && self.cached_text.as_deref() == Some(self.text.as_str());
        if !cache_hit {
            let border = format!(
                "\x1b[90m{}\x1b[39m",
                "─".repeat(usize::from(width.max(2).saturating_sub(2)))
            );
            let lines = vec![
                KeyedLine::new(border.clone(), width),
                KeyedLine::new(format!(" > {}▌", self.text), width),
                KeyedLine::new(border, width),
            ];
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(lines);
        }
        self.cached_lines.as_deref().unwrap_or(&[])
    }
}

// ── Transcript container ──────────────────────────────────────────────────

/// Container holding 150 styled transcript lines, mirroring `buildTranscript`.
struct Transcript {
    lines: Vec<String>,
    cached_width: Option<u16>,
    cached_wrapped: Option<Vec<KeyedLine>>,
}

impl Transcript {
    fn new() -> Self {
        let mut lines = Vec::with_capacity(150);
        for i in 0..150 {
            let styled = if i % 3 == 0 {
                format!(
                    "\x1b[1m\x1b[36muser {i}\x1b[39m\x1b[22m message with some \x1b[33mstyled\x1b[39m content padding padding"
                )
            } else {
                format!(
                    "assistant {i} plain response line with enough text to be representative of a transcript row"
                )
            };
            lines.push(styled);
        }
        Self {
            lines,
            cached_width: None,
            cached_wrapped: None,
        }
    }
}

impl Component for Transcript {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        paint_lines_keyed(area, buf, lines);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.cached_width = None;
        self.cached_wrapped = None;
    }
}

impl Transcript {
    fn lines_for_width(&mut self, width: u16) -> &[KeyedLine] {
        if self.cached_width != Some(width) {
            // Wrap each transcript line to width using the same text wrapper;
            // key each wrapped line once at build (Design E).
            let content_width = usize::from(width.max(1));
            let mut wrapped = Vec::with_capacity(self.lines.len() * 2);
            for line in &self.lines {
                let wrapped_lines = pi_tui::text::wrap_text_with_ansi(line, content_width);
                wrapped.extend(
                    wrapped_lines
                        .into_iter()
                        .map(|line| KeyedLine::new(line, width)),
                );
            }
            self.cached_width = Some(width);
            self.cached_wrapped = Some(wrapped);
        }
        self.cached_wrapped.as_deref().unwrap_or(&[])
    }

    /// Floor-probe mutation (PERF-T11 exhaustion record): rotate the last
    /// three characters of the last transcript line by the frame index —
    /// content is unique for 26³ = 17,576 frames (every frame is a fresh
    /// key: memo miss, full derive, exactly the pinned append path) — while
    /// the length, and therefore the wrapped shape, stays constant. Re-wrap
    /// + re-key only that line in the cached window. Probe-only.
    fn poke(&mut self, i: usize) {
        let idx = self.lines.len() - 1;
        let rot = |i: usize| 97 + u8::try_from(i % 26).unwrap_or(0);
        {
            let line = &mut self.lines[idx];
            let n = line.len();
            if n < 3 {
                return;
            }
            let mut bytes = line.clone().into_bytes();
            bytes[n - 3] = rot(i);
            bytes[n - 2] = rot(i / 26);
            bytes[n - 1] = rot(i / 676);
            *line = String::from_utf8(bytes).unwrap_or_default();
        }
        if let (Some(wrapped), Some(width)) = (&mut self.cached_wrapped, self.cached_width) {
            let rewrapped = pi_tui::text::wrap_text_with_ansi(&self.lines[idx], usize::from(width));
            // The poked line is ~85 chars at width 100 — always one wrapped
            // line; the guard keeps the probe total if that ever changes.
            if rewrapped.len() == 1 {
                let last = wrapped.len() - 1;
                wrapped[last] = KeyedLine::new(rewrapped[0].clone(), width);
            }
        }
    }
}

// ── Layout root: VStack [ ScrollView(transcript), dock VStack ] ───────────

/// Root component mirroring the upstream layout:
/// VStack [ ScrollView(transcript), dock VStack [status, editor, footer] ].
///
/// The Rust TUI uses ratatui's `Terminal::draw` which calls `measure` then
/// `render` on the root.  We replicate the VStack layout manually: the
/// scrollview gets the remaining height after the dock (status=1, editor=3,
/// footer=1 → dock=5).
struct BenchRoot {
    transcript: Transcript,
    editor: EditorSim,
    status: Text,
    footer: Text,
}

impl BenchRoot {
    fn new() -> Self {
        Self {
            transcript: Transcript::new(),
            editor: EditorSim::new(),
            status: Text::with_padding("\x1b[2mstatus: idle\x1b[22m", 1, 0),
            footer: Text::with_padding("\x1b[2m~/workspaces/pi  main  100k tokens\x1b[22m", 1, 0),
        }
    }

    /// Dock height: status(1) + editor(3) + footer(1) = 5, but we compute
    /// dynamically from measure to stay faithful.
    fn dock_height(&mut self, width: u16) -> u16 {
        let status_h = self.status.measure(width).max(1);
        let editor_h = self.editor.measure(width).max(3);
        let footer_h = self.footer.measure(width).max(1);
        status_h.saturating_add(editor_h).saturating_add(footer_h)
    }
}

impl Component for BenchRoot {
    fn measure(&mut self, _width: u16) -> u16 {
        // Fill the viewport, as the upstream VStack [ScrollView, dock] root
        // does: the scroll view expands to the remaining height at any
        // terminal size (`commit_frame` caps at `frame_area.height`). The
        // pinned 100×30 workload is unchanged (min(fill, 30) = 30); the
        // floor probe's cross-shape runs previously rendered a fixed 30 rows
        // and rediffed the unpainted tail every frame (~1.6 µs/row of
        // empty-claim row diff — measured, and recorded in the iteration-7
        // log as a below-rendered-height path that never occurs on the
        // pinned shape).
        u16::MAX
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width;
        let dock_h = self.dock_height(width).min(area.height);
        let scroll_h = area.height.saturating_sub(dock_h);

        // Render scrollview (transcript) in the top region.
        if scroll_h > 0 {
            let scroll_area = Rect::new(area.x, area.y, width, scroll_h);
            // ScrollView in follow-end mode shows the last N lines.
            let all_lines = self.transcript.lines_for_width(width);
            let start = all_lines.len().saturating_sub(usize::from(scroll_h));
            let visible = &all_lines[start..];
            paint_lines_keyed(scroll_area, buf, visible);
        }

        // Render dock: status, editor, footer stacked vertically.
        let mut y = area.y.saturating_add(scroll_h);

        let status_h = self
            .status
            .measure(width)
            .max(1)
            .min(area.height.saturating_sub(scroll_h));
        if status_h > 0 {
            self.status
                .render(Rect::new(area.x, y, width, status_h), buf);
            y = y.saturating_add(status_h);
        }
        let remaining_after_status = area.height.saturating_sub(y.saturating_sub(area.y));
        let editor_h = self
            .editor
            .measure(width)
            .max(3)
            .min(remaining_after_status);
        if editor_h > 0 {
            self.editor
                .render(Rect::new(area.x, y, width, editor_h), buf);
            y = y.saturating_add(editor_h);
        }
        let remaining_after_editor = area.height.saturating_sub(y.saturating_sub(area.y));
        let footer_h = self
            .footer
            .measure(width)
            .max(1)
            .min(remaining_after_editor);
        if footer_h > 0 {
            self.footer
                .render(Rect::new(area.x, y, width, footer_h), buf);
        }
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.transcript.invalidate();
        self.editor.invalidate();
        self.status.invalidate();
        self.footer.invalidate();
    }
}

// ── Scenario runner ───────────────────────────────────────────────────────

struct ScenarioResult {
    allocated_bytes: u64,
    elapsed_ms: f64,
    bytes_written: u64,
}

fn run_scenario(
    tui: &mut Tui<NullWriter>,
    root: &mut BenchRoot,
    frame: impl Fn(usize, &mut BenchRoot),
) -> ScenarioResult {
    let mut total_bytes_written: u64 = 0;

    let alloc_before = CountingAllocator::read();
    let start = Instant::now();

    for i in 0..FRAMES {
        frame(i, root);
        if tui.commit(Txn::Frame, root).is_ok() {
            total_bytes_written = total_bytes_written
                .saturating_add(u64::try_from(tui.last_payload().len()).unwrap_or(u64::MAX));
        }
    }

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let alloc_after = CountingAllocator::read();

    ScenarioResult {
        allocated_bytes: alloc_after.bytes_since(alloc_before),
        elapsed_ms,
        bytes_written: total_bytes_written,
    }
}

fn report(name: &str, result: &ScenarioResult) {
    let alloc_f = result.allocated_bytes as f64;
    let per_frame_kib = alloc_f / FRAMES as f64 / 1024.0;
    let total_mib = alloc_f / 1024.0 / 1024.0;
    let ms_per_frame = result.elapsed_ms / FRAMES as f64;
    let written_per_frame = result.bytes_written as f64 / FRAMES as f64;
    println!(
        "{name:8} allocated {total_mib:>7.1} MiB total  \
         {per_frame_kib:>8.1} KiB/frame  \
         {ms_per_frame:>7.3} ms/frame  \
         {written_per_frame:>6.0} written bytes/frame",
    );
}

// ── Floor probe (PERF-T11 exhaustion record) ──────────────────────────────
//
// Instrumented-counter artifact measuring the replacement pipeline's own
// per-line constants, per the PERF-R9 method words ("instrumented counters
// = committed runner/bench artifacts") and G10 Finding 5 (the floor's
// per-line term must be recomputed from the replacement's own measurement,
// never cited from the pre-campaign implementation).
//
// Terms (release, µs, median of PROBE_REPS reps of PROBE_FRAMES production
// frames or PROBE_TIGHT_ITERS tight-loop ops):
//   frameStatic{30,50,60}  static production frames at 100×{30,50,60} — the
//                          per-frame identity walk; the cross-shape slope
//                          isolates the per-visible-line identity cost.
//   framePoke              production frames with one transcript line's
//                          content changed in place (`Transcript::poke`) —
//                          adds exactly one changed line through the
//                          production path (fresh key, claim mismatch,
//                          derive, one-row damage diff, encode, write).
//   wrapKeyPerLine         tight loop: `wrap_text_with_ansi` +
//                          `KeyedLine::new` on one ~85-col styled line —
//                          the workload-side constant for producing a
//                          changed line's wrapped content.
//   editorRebuild          tight loop: `EditorSim` cache-miss rebuild at
//                          the pinned workload's steady text length — the
//                          workload-side editor constant (upstream
//                          re-materializes borders + text row on every
//                          text miss: render-churn-bench.ts:74-87).
//
// Derived: identitySlope = ΔframeStatic / Δvisible; changedLineCommit =
// framePoke − frameStatic30 − wrapKeyPerLine (the commit path's per-changed-
// line cost: derive + one-row damage diff + encode/write, workload-side
// re-wrap subtracted).
//
// Probe reps use 3000 frames (vs the pinned 300) purely for timer stability
// at the ~2 µs/frame scale; frame semantics are identical.

const PROBE_FRAMES: usize = 3000;
const PROBE_WARMUP: usize = 200;
const PROBE_REPS: usize = 5;
const PROBE_TIGHT_ITERS: usize = 20_000;
const PROBE_TIGHT_WARMUP: usize = 2_000;

struct ProbeFrame {
    us_per_frame: f64,
    bytes_per_frame: f64,
    written_per_frame: f64,
}

fn run_probe_frames(
    tui: &mut Tui<NullWriter>,
    root: &mut BenchRoot,
    frames: usize,
    frame: impl Fn(usize, &mut BenchRoot),
) -> ProbeFrame {
    let alloc_before = CountingAllocator::read();
    let mut written: u64 = 0;
    let start = Instant::now();
    for i in 0..frames {
        frame(i, root);
        if tui.commit(Txn::Frame, root).is_ok() {
            written =
                written.saturating_add(u64::try_from(tui.last_payload().len()).unwrap_or(u64::MAX));
        }
    }
    let us = start.elapsed().as_secs_f64() * 1e6;
    let alloc_after = CountingAllocator::read();
    let n = frames as f64;
    ProbeFrame {
        us_per_frame: us / n,
        bytes_per_frame: alloc_after.bytes_since(alloc_before) as f64 / n,
        written_per_frame: written as f64 / n,
    }
}

/// Paint-path share of a production-frame loop (µs/frame), via the
/// PERF-T11 paint-probe instrument: arms the writer's paint timer around
/// the same production frames, so the paint-only figure is measured on
/// the real path (diff + encode + framing + write) rather than derived.
/// Returns `(total µs/frame, diff-phase µs/frame, frames)`.
fn run_paint_probe(
    tui: &mut Tui<NullWriter>,
    root: &mut BenchRoot,
    frames: usize,
    frame: impl Fn(usize, &mut BenchRoot),
) -> (f64, f64, f64) {
    set_paint_timer(true);
    paint_timer_reset();
    for i in 0..frames {
        frame(i, root);
        let _ = tui.commit(Txn::Frame, root);
    }
    let (total, diff, count) = paint_timer_read();
    set_paint_timer(false);
    let n = f64::from(u32::try_from(count).unwrap_or(u32::MAX)).max(1.0);
    (
        f64::from(u32::try_from(total.min(u64::from(u32::MAX))).unwrap_or(u32::MAX)) / n,
        f64::from(u32::try_from(diff.min(u64::from(u32::MAX))).unwrap_or(u32::MAX)) / n,
        n,
    )
}

fn median_of(vals: &mut [f64]) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    if vals.len() % 2 == 1 {
        vals[mid]
    } else {
        (vals[mid - 1] + vals[mid]) / 2.0
    }
}

fn fresh_tui(rows: u16) -> Tui<NullWriter> {
    let caps = TerminalCapabilities::default();
    match Tui::new(
        NullWriter::new(),
        Size::new(COLUMNS, rows),
        Position::ORIGIN,
        rows,
        caps,
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to create Tui: {e}");
            std::process::exit(1);
        }
    }
}

/// Median static-frame cost at a given viewport height (µs/frame, bytes/frame).
fn probe_static_shape(rows: u16) -> (f64, f64, f64) {
    let mut times = Vec::with_capacity(PROBE_REPS);
    let mut allocs = Vec::with_capacity(PROBE_REPS);
    let mut written = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(rows);
        let mut root = BenchRoot::new();
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let r = run_probe_frames(&mut tui, &mut root, PROBE_FRAMES, |_i, _root| {});
        times.push(r.us_per_frame);
        allocs.push(r.bytes_per_frame);
        written.push(r.written_per_frame);
    }
    (
        median_of(&mut times),
        median_of(&mut allocs),
        median_of(&mut written),
    )
}

/// Median paint-only share of a static frame loop at a viewport height
/// (µs/frame, via the paint-probe instrument).
fn probe_paint_static(rows: u16) -> f64 {
    let mut times = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(rows);
        let mut root = BenchRoot::new();
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let (total, _diff, _n) = run_paint_probe(&mut tui, &mut root, PROBE_FRAMES, |_i, _root| {});
        times.push(total);
    }
    median_of(&mut times)
}

/// Median paint-only share of a poke (one changed transcript line) frame
/// loop at the pinned shape: (total µs/frame, diff-phase µs/frame).
fn probe_paint_poke() -> (f64, f64) {
    let mut total_times = Vec::with_capacity(PROBE_REPS);
    let mut diff_times = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(ROWS);
        let mut root = BenchRoot::new();
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let (total, diff, _n) = run_paint_probe(&mut tui, &mut root, PROBE_FRAMES, |i, root| {
            root.transcript.poke(i);
        });
        total_times.push(total);
        diff_times.push(diff);
    }
    (median_of(&mut total_times), median_of(&mut diff_times))
}

/// Median paint-only share of an editor-steady (rotated trailing chars)
/// frame loop at the pinned shape: (total µs/frame, diff-phase µs/frame).
fn probe_paint_editor_steady() -> (f64, f64) {
    let mut total_times = Vec::with_capacity(PROBE_REPS);
    let mut diff_times = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(ROWS);
        let mut root = BenchRoot::new();
        for i in 0..150 {
            root.editor
                .append(char::from(b'a' + u8::try_from(i % 26).unwrap_or(0)));
        }
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let (total, diff, _n) = run_paint_probe(&mut tui, &mut root, PROBE_FRAMES, |i, root| {
            root.editor.rotate_unique(i);
        });
        total_times.push(total);
        diff_times.push(diff);
    }
    (median_of(&mut total_times), median_of(&mut diff_times))
}

/// Median one-changed-line frame cost at the pinned shape (µs/frame, bytes/frame, written bytes/frame).
fn probe_poke() -> (f64, f64, f64) {
    let mut times = Vec::with_capacity(PROBE_REPS);
    let mut allocs = Vec::with_capacity(PROBE_REPS);
    let mut written = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(ROWS);
        let mut root = BenchRoot::new();
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let r = run_probe_frames(&mut tui, &mut root, PROBE_FRAMES, |i, root| {
            root.transcript.poke(i);
        });
        times.push(r.us_per_frame);
        allocs.push(r.bytes_per_frame);
        written.push(r.written_per_frame);
    }
    (
        median_of(&mut times),
        median_of(&mut allocs),
        median_of(&mut written),
    )
}

/// Median editor-frame cost at steady text length (µs/frame, bytes/frame):
/// rotate three of the editor's trailing characters by frame index instead
/// the EditorSim rebuild, the changed text row, the damage diff, and the
/// encode all run at the pinned workload's average text length (~150 chars)
/// with no growth drift. The pinned editor scenario minus this equals the
/// append-growth effect (workload-side).
fn probe_editor_steady() -> (f64, f64) {
    let mut times = Vec::with_capacity(PROBE_REPS);
    let mut allocs = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let mut tui = fresh_tui(ROWS);
        let mut root = BenchRoot::new();
        // Grow to the pinned workload's average editor text length first.
        for i in 0..150 {
            root.editor
                .append(char::from_u32(97 + (i % 26) as u32).unwrap_or('a'));
        }
        for _ in 0..PROBE_WARMUP {
            let _ = tui.commit(Txn::Frame, &mut root);
        }
        let r = run_probe_frames(&mut tui, &mut root, PROBE_FRAMES, |i, root| {
            root.editor.rotate_unique(i);
        });
        times.push(r.us_per_frame);
        allocs.push(r.bytes_per_frame);
    }
    (median_of(&mut times), median_of(&mut allocs))
}

/// Tight loop: wrap + key one ~85-col styled transcript line (µs/op).
fn probe_wrap_key() -> f64 {
    let styled = "\x1b[1m\x1b[36muser 0\x1b[39m\x1b[22m message with some \
                  \x1b[33mstyled\x1b[39m content padding padding";
    let mut sink = 0u64;
    for _ in 0..PROBE_TIGHT_WARMUP {
        let wrapped = pi_tui::text::wrap_text_with_ansi(styled, usize::from(COLUMNS));
        let keyed = KeyedLine::new(wrapped[0].clone(), COLUMNS);
        sink = sink.wrapping_add(u64::try_from(keyed.line().len()).unwrap_or(0));
    }
    let mut times = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let start = Instant::now();
        for _ in 0..PROBE_TIGHT_ITERS {
            let wrapped = pi_tui::text::wrap_text_with_ansi(styled, usize::from(COLUMNS));
            let keyed = KeyedLine::new(wrapped[0].clone(), COLUMNS);
            sink = sink.wrapping_add(u64::try_from(keyed.line().len()).unwrap_or(0));
        }
        times.push(start.elapsed().as_secs_f64() * 1e6 / PROBE_TIGHT_ITERS as f64);
    }
    std::hint::black_box(sink);
    median_of(&mut times)
}

/// Tight loop: EditorSim cache-miss rebuild at steady text length (µs/op).
fn probe_editor_rebuild() -> f64 {
    // Seed to the pinned workload's average text length (~150 chars over
    // 300 appended frames), then rotate the last char per op so content
    // changes (cache miss) while length — and therefore format cost — stays
    // at steady state.
    let mut editor = EditorSim::new();
    for _ in 0..150 {
        editor.append('x');
    }
    let rotate = |i: usize, editor: &mut EditorSim| {
        editor.rotate_unique(i);
    };
    for i in 0..PROBE_TIGHT_WARMUP {
        rotate(i, &mut editor);
        std::hint::black_box(editor.lines_for_width(COLUMNS).len());
    }
    let mut times = Vec::with_capacity(PROBE_REPS);
    for _ in 0..PROBE_REPS {
        let start = Instant::now();
        for i in 0..PROBE_TIGHT_ITERS {
            rotate(i, &mut editor);
            std::hint::black_box(editor.lines_for_width(COLUMNS).len());
        }
        times.push(start.elapsed().as_secs_f64() * 1e6 / PROBE_TIGHT_ITERS as f64);
    }
    median_of(&mut times)
}

fn run_probe() -> ExitCode {
    let (static30, static30_bytes, static30_written) = probe_static_shape(30);
    let (static50, _, _) = probe_static_shape(50);
    let (static60, _, _) = probe_static_shape(60);
    let (poke, poke_bytes, poke_written) = probe_poke();
    let (editor_steady, editor_steady_bytes) = probe_editor_steady();
    let wrap_key = probe_wrap_key();
    let editor_rebuild = probe_editor_rebuild();
    let paint_static30 = probe_paint_static(30);
    let (paint_poke, paint_poke_diff) = probe_paint_poke();
    let (paint_editor, paint_editor_diff) = probe_paint_editor_steady();

    let mut root = BenchRoot::new();
    let dock = root.dock_height(COLUMNS);
    let visible = |rows: u16| f64::from(rows.saturating_sub(dock));
    let slope_30_50 = (static50 - static30) / (visible(50) - visible(30));
    let slope_50_60 = (static60 - static50) / (visible(60) - visible(50));
    let changed_line_commit = poke - static30 - wrap_key;
    // The editor row's own changed-line commit at its true shape (~150-char
    // text row): steady-frame cost minus the static frame minus the
    // workload-side rebuild.
    let editor_row_commit = editor_steady - static30 - editor_rebuild;

    println!(
        "floor probe (reps={PROBE_REPS} × {PROBE_FRAMES} frames; tight {PROBE_TIGHT_ITERS} ops)"
    );
    println!(
        "  frameStatic30   {static30:8.3} µs/frame  ({static30_bytes:6.0} B/frame, {static30_written:5.1} B written)"
    );
    println!("  frameStatic50   {static50:8.3} µs/frame");
    println!("  frameStatic60   {static60:8.3} µs/frame");
    println!(
        "  framePoke       {poke:8.3} µs/frame  ({poke_bytes:6.0} B/frame, {poke_written:5.1} B written)"
    );
    println!(
        "  frameEditorSteady {editor_steady:6.3} µs/frame  ({editor_steady_bytes:6.0} B/frame)"
    );
    println!("  paintStatic30    {paint_static30:7.3} µs/frame  (paint-only, probe)");
    println!(
        "  paintPoke        {paint_poke:7.3} µs/frame  (paint-only; diff phase {paint_poke_diff:.3})"
    );
    println!(
        "  paintEditorSteady {paint_editor:6.3} µs/frame  (paint-only; diff phase {paint_editor_diff:.3})"
    );
    println!("  wrapKeyPerLine  {wrap_key:8.3} µs/line   (workload-side)");
    println!("  editorRebuild   {editor_rebuild:8.3} µs/frame (workload-side)");
    println!("  identitySlope   {slope_30_50:8.4} µs/visible-line (30↔50; 50↔60 {slope_50_60:.4})");
    println!(
        "  changedLineCommit {changed_line_commit:6.3} µs/changed-line (poke − static − wrapKey)"
    );
    println!(
        "  editorRowCommit {editor_row_commit:6.3} µs/changed-line (steady − static − rebuild)"
    );

    let json = format!(
        "\n__PROBE_JSON__\n{{\
         \"framesPerRep\": {PROBE_FRAMES},\
         \"reps\": {PROBE_REPS},\
         \"tightIters\": {PROBE_TIGHT_ITERS},\
         \"measured\": {{\
         \"frameStatic30Us\": {static30:.4},\
         \"frameStatic50Us\": {static50:.4},\
         \"frameStatic60Us\": {static60:.4},\
         \"framePokeUs\": {poke:.4},\
         \"frameEditorSteadyUs\": {editor_steady:.4},\
         \"frameEditorSteadyBytesPerFrame\": {editor_steady_bytes:.1},\
         \"frameStatic30WrittenPerFrame\": {static30_written:.1},\
         \"framePokeWrittenPerFrame\": {poke_written:.1},\
         \"wrapKeyUsPerLine\": {wrap_key:.4},\
         \"editorRebuildUs\": {editor_rebuild:.4},\
         \"paintStatic30Us\": {paint_static30:.4},\
         \"paintPokeUs\": {paint_poke:.4},\
         \"paintPokeDiffUs\": {paint_poke_diff:.4},\
         \"paintEditorSteadyUs\": {paint_editor:.4},\
         \"paintEditorSteadyDiffUs\": {paint_editor_diff:.4}\
         }},\
         \"derived\": {{\
         \"dockHeight\": {dock},\
         \"visibleLines30\": {v30},\
         \"visibleLines50\": {v50},\
         \"visibleLines60\": {v60},\
         \"identitySlopeUsPerLine\": {slope_30_50:.5},\
         \"identitySlope50to60UsPerLine\": {slope_50_60:.5},\
         \"changedLineCommitUs\": {changed_line_commit:.4},\
         \"editorRowCommitUs\": {editor_row_commit:.4}\
         }}\
         }}",
        v30 = visible(30),
        v50 = visible(50),
        v60 = visible(60),
    );
    println!("{json}");
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--probe") {
        return run_probe();
    }

    let caps = TerminalCapabilities::default();
    let outer = NullWriter::new();
    let size = Size::new(COLUMNS, ROWS);

    let tui_result = Tui::new(outer, size, Position::ORIGIN, ROWS, caps);
    let mut tui = match tui_result {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to create Tui: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut root = BenchRoot::new();

    // Warmup
    for _ in 0..WARMUP_FRAMES {
        let _ = tui.commit(Txn::Frame, &mut root);
    }

    // Reset allocator counters after warmup to get clean measurements.
    CountingAllocator::reset();

    // Static scenario: nothing changes between frames.
    let static_result = run_scenario(&mut tui, &mut root, |_i, _root| {});

    // Editor scenario: one character appended per frame.
    let editor_result = run_scenario(&mut tui, &mut root, |i, root| {
        let ch = char::from_u32(97 + (i % 26) as u32).unwrap_or('a');
        root.editor.append(ch);
    });

    // Report
    let transcript_lines = root.transcript.lines_for_width(COLUMNS).len();
    println!("frames={FRAMES} viewport={COLUMNS}x{ROWS} transcript={transcript_lines} lines");
    report("static", &static_result);
    report("editor", &editor_result);

    // Emit structured JSON for the verification runner to consume.
    let static_alloc = static_result.allocated_bytes;
    let static_ms = format!("{:.3}", static_result.elapsed_ms);
    let static_written = static_result.bytes_written;
    let static_mpf = format!("{:.3}", static_result.elapsed_ms / FRAMES as f64);
    let static_kpf = format!("{:.1}", static_alloc as f64 / FRAMES as f64 / 1024.0);
    let editor_alloc = editor_result.allocated_bytes;
    let editor_ms = format!("{:.3}", editor_result.elapsed_ms);
    let editor_written = editor_result.bytes_written;
    let editor_mpf = format!("{:.3}", editor_result.elapsed_ms / FRAMES as f64);
    let editor_kpf = format!("{:.1}", editor_alloc as f64 / FRAMES as f64 / 1024.0);

    println!(
        "\n__BENCH_JSON__\n{{\
         \"frames\": {FRAMES},\
         \"viewport\": {{\"columns\": {COLUMNS}, \"rows\": {ROWS}}},\
         \"warmupFrames\": {WARMUP_FRAMES},\
         \"transcriptLines\": {transcript_lines},\
         \"scenarios\": {{\
         \"static\": {{\
         \"allocatedBytes\": {static_alloc},\
         \"elapsedMs\": \"{static_ms}\",\
         \"bytesWritten\": {static_written},\
         \"msPerFrame\": \"{static_mpf}\",\
         \"kiBPerFrame\": \"{static_kpf}\"\
         }},\
         \"editor\": {{\
         \"allocatedBytes\": {editor_alloc},\
         \"elapsedMs\": \"{editor_ms}\",\
         \"bytesWritten\": {editor_written},\
         \"msPerFrame\": \"{editor_mpf}\",\
         \"kiBPerFrame\": \"{editor_kpf}\"\
         }}\
         }}\
         }}"
    );

    ExitCode::SUCCESS
}
