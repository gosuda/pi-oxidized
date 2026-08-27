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
use pi_tui::terminal::{TerminalCapabilities, Tui, Txn};

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
    cached_lines: Option<Vec<String>>,
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

impl Component for EditorSim {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        pi_tui::components::util::paint_lines(area, buf, lines);
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
    fn lines_for_width(&mut self, width: u16) -> &[String] {
        let cache_hit = self.cached_width == Some(width)
            && self.cached_text.as_deref() == Some(self.text.as_str());
        if !cache_hit {
            let border = format!(
                "\x1b[90m{}\x1b[39m",
                "─".repeat(usize::from(width.max(2).saturating_sub(2)))
            );
            let lines = vec![
                border.clone(),
                format!(" > {}▌", self.text),
                border,
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
    cached_wrapped: Option<Vec<String>>,
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
        pi_tui::components::util::paint_lines(area, buf, lines);
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
    fn lines_for_width(&mut self, width: u16) -> &[String] {
        if self.cached_width != Some(width) {
            // Wrap each transcript line to width using the same text wrapper.
            let content_width = usize::from(width.max(1));
            let mut wrapped = Vec::with_capacity(self.lines.len() * 2);
            for line in &self.lines {
                let wrapped_lines = pi_tui::text::wrap_text_with_ansi(line, content_width);
                wrapped.extend(wrapped_lines);
            }
            self.cached_width = Some(width);
            self.cached_wrapped = Some(wrapped);
        }
        self.cached_wrapped.as_deref().unwrap_or(&[])
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
            footer: Text::with_padding(
                "\x1b[2m~/workspaces/pi  main  100k tokens\x1b[22m",
                1,
                0,
            ),
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
        ROWS
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
            pi_tui::components::util::paint_lines(scroll_area, buf, visible);
        }

        // Render dock: status, editor, footer stacked vertically.
        let mut y = area.y.saturating_add(scroll_h);

        let status_h = self.status.measure(width).max(1).min(area.height.saturating_sub(scroll_h));
        if status_h > 0 {
            self.status.render(Rect::new(area.x, y, width, status_h), buf);
            y = y.saturating_add(status_h);
        }
        let remaining_after_status = area.height.saturating_sub(y.saturating_sub(area.y));
        let editor_h = self.editor.measure(width).max(3).min(remaining_after_status);
        if editor_h > 0 {
            self.editor.render(Rect::new(area.x, y, width, editor_h), buf);
            y = y.saturating_add(editor_h);
        }
        let remaining_after_editor = area.height.saturating_sub(y.saturating_sub(area.y));
        let footer_h = self.footer.measure(width).max(1).min(remaining_after_editor);
        if footer_h > 0 {
            self.footer.render(Rect::new(area.x, y, width, footer_h), buf);
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

fn main() -> ExitCode {
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
    println!(
        "frames={FRAMES} viewport={COLUMNS}x{ROWS} transcript={transcript_lines} lines"
    );
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
