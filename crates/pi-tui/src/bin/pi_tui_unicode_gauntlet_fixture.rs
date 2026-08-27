//! Unicode/width gauntlet fixture for TUI-V3 (issue #81).
//!
//! Renders the 13-probe width corpus from the TUI-R2 survey
//! (`docs/TUI-R2-terminal-width-table-divergence.md` §3) through the real
//! component/paint pipeline so the PTY harness can measure, per probe and
//! per surface, whether rails and table borders stay column-aligned and
//! the cursor drift-free:
//!
//! - `railed`           — real `Rail` + `paint_lines` rows, one per probe,
//!                        closing `│` sentinel per row
//! - `table-1..3`       — real `Markdown` tables with probe cells (grid
//!                        borders are the alignment oracle); P02 (tab) is
//!                        excluded here because GFM table parsing consumes
//!                        raw tabs as cell separators
//! - `editor-Pxx`       — real focused `Input`, cursor parked directly
//!                        after each probe (hardware cursor oracle)
//! - `overlay`          — production `write_overlay_cells` compositing
//!                        probe rows over base rows with a fixed base
//!                        sentinel beyond the overlay region
//! - `paste-*`          — real multiline `Editor` paste events: verbatim
//!                        atomic multi-line paste, whole-paste undo
//!                        (atomicity self-check stamped on screen), and
//!                        the large-paste `[paste #N +N lines]` marker
//!
//! Every rendered hint derives from the keybinding registry; stage-3
//! writes are wrapped with OSC transaction markers identical to the other
//! stepped fixtures so the harness recovers write boundaries.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::editor::Editor;
use pi_tui::components::{
    DefaultTextStyle, Input, Markdown, MarkdownOptions, MarkdownTheme, Rail,
};
use pi_tui::components::util::paint_lines;
use pi_tui::keys::set_kitty_protocol_active;
use pi_tui::overlay::write_overlay_cells;
use pi_tui::terminal::{
    ProbeSession, TerminalCapabilities, TerminalGuard, TerminalInput, Tui, Txn,
    install_panic_emergency_hook, probe_query_batch, write_emergency_restore_bytes,
};
use pi_tui::text::{normalize_terminal_output, visible_width};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};

const DRAW_DEADLINE: Duration = Duration::from_secs(8);
const HARD_TIMEOUT: Duration = Duration::from_secs(60);
const VIEWPORT_HEIGHT: u16 = 20;

/// Single-width ASCII filler placed between probe and closing sentinel.
const FILLER: &str = "abcdef";

/// The 13-probe width corpus (TUI-R2 §3), verbatim inputs.
const CORPUS: [(&str, &str); 13] = [
    ("P01", "OK"),
    ("P02", "\t"),
    ("P03", "\u{b0}\u{b1}\u{25a0}"),
    ("P04", "\u{6f22}\u{5b57}"),
    ("P05", "\u{ff71}\u{ff8f}"),
    ("P06", "\u{ff21}\u{ff01}"),
    ("P07", "e\u{301}"),
    ("P08", "\u{200b}"),
    ("P09", "\u{2764}\u{fe0f}"),
    (
        "P10",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
    ),
    ("P11", "\u{1f1fa}\u{1f1f8}"),
    ("P12", "\u{1f1fa}"),
    ("P13", "\u{e17}\u{e33}\u{e97}\u{eb3}"),
];

/// Markdown table chunks: P02 (raw tab) is excluded — GFM table parsing
/// consumes tabs as cell separators, so its table measurement would test
/// the parser, not the width table.
const TABLE_CHUNKS: [usize; 3] = [5, 5, 3];
/// Indices excluded from every markdown table chunk.
const TABLE_SKIP: [usize; 1] = [1];

/// Overlay compositing geometry: one overlay row per probe at column
/// `OVERLAY_COL`, `OVERLAY_WIDTH` cells wide; base sentinel sits beyond it.
const OVERLAY_COL: u16 = 12;
const OVERLAY_WIDTH: u16 = 30;
/// Column where the base-row sentinel `B9` starts in overlay rows.
const BASE_SENTINEL_COL: usize = 44;

/// Return the probe after the terminal-output normalization applied by
/// the real render path (e.g. tab -> 3 spaces, Thai/Lao AM split). This
/// avoids control characters entering the Ratatui buffer and keeps the
/// visible contract identical to `visible_width` / `grapheme_width`.
fn normalized_probe(index: usize) -> String {
    normalize_terminal_output(CORPUS[index].1)
}

/// One gauntlet row: `{label} {probe} {filler} │` — the trailing `│` is
/// the closing alignment sentinel measured by the harness.
fn gauntlet_row(index: usize) -> String {
    let (label, _probe) = CORPUS[index];
    format!("{label} {} {FILLER} \u{2502}", normalized_probe(index))
}

/// Paste payload lines for the verbatim paste phases.
fn paste_payload(range: std::ops::Range<usize>) -> String {
    range.map(gauntlet_row).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------------------
// Gauntlet root component
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Railed,
    Table(usize),
    Edit(usize),
    Overlay,
    PasteVerbatim(usize),
    PasteAtomic,
    PasteMarker,
    Done,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Railed => "railed",
            Self::Table(chunk) => match chunk {
                0 => "table-1",
                1 => "table-2",
                _ => "table-3",
            },
            Self::Edit(index) => CORPUS[index].0,
            Self::Overlay => "overlay",
            Self::PasteVerbatim(0) => "paste-verbatim-1",
            Self::PasteVerbatim(1) => "paste-verbatim-2",
            Self::PasteVerbatim(_) => "paste-verbatim-x",
            Self::PasteAtomic => "paste-atomic",
            Self::PasteMarker => "paste-marker",
            Self::Done => "DONE-MARKER",
        }
    }
}

/// Fixture-local child that paints pre-wrapped rows through the
/// production `paint_lines` path.
struct LinesChild {
    lines: Vec<String>,
}

impl Component for LinesChild {
    fn measure(&mut self, _width: u16) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let take = self.lines.len().min(usize::from(area.height));
        paint_lines(area, buf, &self.lines[..take]);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {}
}

enum Surface {
    None,
    Rail(Rail),
    Markdown(Markdown),
    Input(Input),
    Editor(Editor),
}

struct GauntletRoot {
    phase: Phase,
    surface: Surface,
    /// Overlay base rows (probe label -> rendered base line with sentinel).
    overlay_base: Vec<String>,
    /// Atomicity self-check result for the paste-atomic phase.
    paste_atomic_verdict: &'static str,
    /// Monotonic commit generation (script-deterministic).
    generation: u64,
}

impl GauntletRoot {
    fn new() -> Self {
        Self {
            phase: Phase::Railed,
            surface: Surface::None,
            overlay_base: Vec::new(),
            paste_atomic_verdict: "",
            generation: 0,
        }
    }

    fn markdown_chunk(chunk: usize) -> String {
        // Sequential chunks of the non-skipped corpus, sized by TABLE_CHUNKS.
        let kept: Vec<usize> = (0..CORPUS.len())
            .filter(|index| !TABLE_SKIP.contains(index))
            .collect();
        let mut chunks: Vec<Vec<usize>> = Vec::new();
        let mut rest = kept.as_slice();
        for size in TABLE_CHUNKS {
            let take = rest.len().min(size);
            let (head, tail) = rest.split_at(take);
            chunks.push(head.to_vec());
            rest = tail;
        }
        if !rest.is_empty() {
            chunks.push(rest.to_vec());
        }
        let selected = chunks.get(chunk).cloned().unwrap_or_default();
        let mut doc = format!(
            "Gauntlet table {} of 3\n\n| probe | glyphs |\n| --- | --- |\n",
            chunk + 1
        );
        for index in selected {
            let (label, _probe) = CORPUS[index];
            doc.push_str(&format!("| {label} {} | {} |\n", normalized_probe(index), index + 1));
        }
        doc
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.surface = match phase {
            Phase::Railed => {
                let mut rail = Rail::with_glyph("\u{2502}", str::to_owned);
                rail.add_child(LinesChild {
                    lines: (0..CORPUS.len()).map(gauntlet_row).collect(),
                });
                Surface::Rail(rail)
            }
            Phase::Table(chunk) => Surface::Markdown(Markdown::new(
                Self::markdown_chunk(chunk),
                0,
                0,
                MarkdownTheme::default(),
                DefaultTextStyle::default(),
                MarkdownOptions::default(),
            )),
            Phase::Edit(index) => {
                let mut input = Input::new();
                input.paste(&normalized_probe(index));
                input.set_focused(true);
                Surface::Input(input)
            }
            Phase::Overlay => {
                let mut base = Vec::with_capacity(CORPUS.len());
                for index in 0..CORPUS.len() {
                    let (label, _probe) = CORPUS[index];
                    let probe = normalized_probe(index);
                    let prefix = format!("{label} {probe} {FILLER}");
                    let pad = BASE_SENTINEL_COL.saturating_sub(visible_width(&prefix));
                    base.push(format!("{prefix}{}B9", " ".repeat(pad)));
                }
                self.overlay_base = base;
                Surface::None
            }
            // Paste phases retain the editor across steps: the first
            // verbatim round builds a fresh focused editor, later rounds
            // and the atomicity check step the same instance.
            Phase::PasteVerbatim(0) | Phase::PasteMarker => {
                let mut editor = Editor::with_defaults();
                editor.set_terminal_rows(24);
                editor.focused = true;
                Surface::Editor(editor)
            }
            Phase::PasteVerbatim(_) | Phase::PasteAtomic => match self.surface {
                Surface::Editor(_) => return,
                _ => {
                    let mut editor = Editor::with_defaults();
                    editor.set_terminal_rows(24);
                    editor.focused = true;
                    Surface::Editor(editor)
                }
            },
            Phase::Done => Surface::None,
        };
    }

    /// Step a retained editor surface (second paste, undo self-check,
    /// large-paste marker) without rebuilding it.
    fn step_editor(&mut self, event: EditorStep) {
        match (event, &mut self.surface) {
            (EditorStep::Paste(round), Surface::Editor(editor)) => {
                let payload = match round {
                    0 => paste_payload(0..8),
                    _ => paste_payload(8..13),
                };
                editor.handle_event(&UiEvent::Paste(payload));
            }
            (EditorStep::Undo, Surface::Editor(editor)) => {
                let undo_key = crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('-'),
                    crossterm::event::KeyModifiers::CONTROL,
                );
                editor.handle_event(&UiEvent::Key(undo_key));
            }
            (EditorStep::AtomicCheck, Surface::Editor(editor)) => {
                let after_two = editor.get_text();
                // One undo per paste: two undos must restore the pre-paste
                // buffer exactly (whole-segment atomicity).
                self.paste_atomic_verdict = if after_two.is_empty() {
                    "PASTE-ATOMIC ok"
                } else {
                    "PASTE-ATOMIC FAIL"
                };
            }
            (EditorStep::LargePaste, Surface::Editor(editor)) => {
                let payload = paste_payload(0..CORPUS.len());
                editor.handle_event(&UiEvent::Paste(payload));
            }
            _ => {}
        }
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

enum EditorStep {
    Paste(usize),
    Undo,
    AtomicCheck,
    LargePaste,
}

impl Component for GauntletRoot {
    fn measure(&mut self, _width: u16) -> u16 {
        VIEWPORT_HEIGHT
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one sequential gauntlet walk; splitting would scatter the checkpoint order"
    )]
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let bottom = area.y.saturating_add(area.height);
        let mut row = area.y;

        let status = match self.phase {
            Phase::Edit(index) => format!("STATUS editor-{}", CORPUS[index].0),
            Phase::PasteAtomic => format!("STATUS {}", self.paste_atomic_verdict),
            other => format!("STATUS {}", other.label()),
        };
        put_line(&status, buf, &mut row, width, bottom);

        match &mut self.surface {
            Surface::None => {
                if self.phase == Phase::Overlay {
                    // Base rows first, then the production overlay painter
                    // composites one overlay row per probe on top.
                    for (index, base) in self.overlay_base.iter().enumerate() {
                        if row >= bottom {
                            break;
                        }
                        let y = row;
                        put_line(base, buf, &mut row, width, bottom);
                        let overlay_line = gauntlet_row(index);
                        let overlay_area = Rect::new(
                            OVERLAY_COL,
                            y,
                            OVERLAY_WIDTH,
                            1,
                        );
                        write_overlay_cells(buf, overlay_area, &overlay_line);
                    }
                }
            }
            Surface::Rail(rail) => {
                let remaining = bottom.saturating_sub(row);
                let height = rail.measure(area.width).min(remaining);
                rail.render(Rect::new(area.x, row, area.width, height), buf);
                row = row.saturating_add(height);
            }
            Surface::Markdown(markdown) => {
                let remaining = bottom.saturating_sub(row);
                let height = markdown.measure(area.width).min(remaining);
                markdown.render(Rect::new(area.x, row, area.width, height), buf);
                row = row.saturating_add(height);
            }
            Surface::Input(input) => {
                let remaining = bottom.saturating_sub(row);
                let height = input.measure(area.width).min(remaining);
                input.render(Rect::new(area.x, row, area.width, height), buf);
                row = row.saturating_add(height);
            }
            Surface::Editor(editor) => {
                let remaining = bottom.saturating_sub(row);
                let height = editor.measure(area.width).min(remaining);
                editor.render(Rect::new(area.x, row, area.width, height), buf);
                row = row.saturating_add(height);
            }
        }

        put_line(
            &format!("GEN {}", self.generation),
            buf,
            &mut row,
            width,
            bottom,
        );
        put_line("FOOTER pi-tui-unicode-gauntlet", buf, &mut row, width, bottom);
        if self.phase == Phase::Done {
            put_line("UNICODE GAUNTLET COMPLETE", buf, &mut row, width, bottom);
        }
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {}
}

fn put_line(line: &str, buf: &mut Buffer, row: &mut u16, width: usize, bottom: u16) {
    if *row >= bottom {
        return;
    }
    let mut col = 0usize;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(line, true) {
        if col >= width {
            break;
        }
        let gw = visible_width(grapheme).max(1);
        if col + gw > width {
            break;
        }
        let x = u16::try_from(col).unwrap_or(u16::MAX);
        let symbol = if grapheme.chars().any(|c| c.is_control()) {
            " "
        } else {
            grapheme
        };
        buf[(x, *row)].set_symbol(symbol);
        if gw == 2 {
            if let Some(cell) = buf.cell_mut((x.saturating_add(1), *row)) {
                cell.set_symbol("");
            }
        }
        col += gw;
    }
    *row = row.saturating_add(1);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Non-blocking stdin read for probe replies. Returns `None` when no data is ready.
fn read_stdin_nonblocking() -> io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::fd::AsFd;

        let stdin = io::stdin();
        let fd = stdin.as_fd();
        let mut fds = [nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN)];
        let n = nix::poll::poll(&mut fds, 0u8)
            .map_err(|err| io::Error::other(format!("poll stdin: {err}")))?;
        if n == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 512];
        let mut handle = stdin.lock();
        match handle.read(&mut buf) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(n) => Ok(Some(buf[..n].to_vec())),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "pi_tui_unicode_gauntlet_fixture error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let args: Vec<String> = env::args().skip(1).collect();
    for arg in &args {
        if arg != "--help" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown argument {arg}"),
            ));
        }
    }
    if args.iter().any(|arg| arg == "--help") {
        writeln!(io::stdout(), "pi_tui_unicode_gauntlet_fixture (stepped; no arguments)")?;
        return Ok(ExitCode::SUCCESS);
    }

    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| io::Error::other(format!("runtime: {err}")))?;

    runtime.block_on(async move { run_gauntlet(started).await })
}

// ---------------------------------------------------------------------------
// StdoutOwner (identical to pi_tui_state_matrix_fixture)
// ---------------------------------------------------------------------------

struct StdoutOwner {
    out: io::Stdout,
    pending: Vec<u8>,
    txn_id: u64,
    write_log: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl StdoutOwner {
    fn new(write_log: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        Self {
            out: io::stdout(),
            pending: Vec::new(),
            txn_id: 0,
            write_log,
        }
    }
}

impl Write for StdoutOwner {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return self.out.flush();
        }
        let id = self.txn_id;
        self.txn_id = self.txn_id.saturating_add(1);
        let payload = std::mem::take(&mut self.pending);
        if let Ok(mut log) = self.write_log.lock() {
            log.push(payload.clone());
        }
        let begin = format!("\x1b]999;PI_TUI_TXN_BEGIN={id}\x07");
        let end = format!("\x1b]999;PI_TUI_TXN_END={id}\x07");
        self.out.write_all(begin.as_bytes())?;
        self.out.write_all(&payload)?;
        self.out.write_all(end.as_bytes())?;
        self.out.flush()
    }
}

// ---------------------------------------------------------------------------
// Gauntlet runner
// ---------------------------------------------------------------------------

async fn run_gauntlet(started: Instant) -> io::Result<ExitCode> {
    let mut guard = TerminalGuard::new(io::stdout());
    let emergency = guard.emergency_flag();
    {
        install_panic_emergency_hook(
            Arc::clone(&emergency),
            Arc::new(move || {
                let mut out = io::stdout();
                let _ = write_emergency_restore_bytes(&mut out);
            }),
        );
    }

    crossterm::terminal::enable_raw_mode()?;

    let probe_bytes = probe_query_batch(true);
    guard.writer_mut().write_all(&probe_bytes)?;
    guard.writer_mut().flush()?;

    let mut probe = ProbeSession::new();
    let probe_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < probe_deadline && !probe.is_complete() {
        if let Some(bytes) = read_stdin_nonblocking()? {
            if bytes.is_empty() {
                break;
            }
            let _ = probe.feed(&bytes);
            continue;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if !probe.is_complete() {
        let _ = probe.feed(b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R");
    }

    let mut caps = tokio::task::spawn_blocking(TerminalCapabilities::detect)
        .await
        .map_err(|err| io::Error::other(format!("capability detection join failed: {err}")))?;
    caps.sync_output = true;
    let cursor = probe.apply_to(&mut caps).unwrap_or((0, 0));
    set_kitty_protocol_active(caps.kitty_keyboard());

    let size = match crossterm::terminal::size() {
        Ok((w, h)) => Size::new(w.max(20), h.max(8)),
        Err(_) => Size::new(80, 24),
    };
    guard.activate(caps.kitty_keyboard())?;
    guard.set_viewport_bottom_row(size.height.saturating_sub(1));

    let write_log = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let outer = StdoutOwner::new(Arc::clone(&write_log));
    let mut tui = Tui::new(
        outer,
        size,
        Position {
            x: cursor.0,
            y: cursor.1,
        },
        VIEWPORT_HEIGHT,
        caps,
    )?;

    let mut input = TerminalInput::spawn();
    let mut root = GauntletRoot::new();

    // Stepped gauntlet: each phase renders, then the fixture blocks on one
    // harness step event so the PTY harness settles per-phase snapshots at
    // real boundaries.
    let mut steps: Vec<(Phase, Vec<EditorStep>)> = Vec::new();
    steps.push((Phase::Railed, Vec::new()));
    for chunk in 0..TABLE_CHUNKS.len() {
        steps.push((Phase::Table(chunk), Vec::new()));
    }
    for index in 0..CORPUS.len() {
        steps.push((Phase::Edit(index), Vec::new()));
    }
    steps.push((Phase::Overlay, Vec::new()));
    // Paste-verbatim: paste-1 builds the editor (enter), paste-2 appends.
    steps.push((
        Phase::PasteVerbatim(0),
        vec![EditorStep::Paste(0)],
    ));
    steps.push((
        Phase::PasteVerbatim(1),
        vec![EditorStep::Paste(1)],
    ));
    // Atomicity: two undos remove both verbatim pastes in one step each,
    // then the self-check stamps the verdict; a fresh large paste renders
    // the marker row.
    steps.push((
        Phase::PasteAtomic,
        vec![EditorStep::Undo, EditorStep::Undo, EditorStep::AtomicCheck],
    ));
    steps.push((Phase::PasteMarker, vec![EditorStep::LargePaste]));

    for (phase, editor_steps) in steps {
        {
            let marker = format!("\x1b]999;PI_TUI_UG={}\x07", phase.label());
            let mut out = io::stdout();
            out.write_all(marker.as_bytes())?;
            out.flush()?;
        }
        root.enter(phase);
        for step in editor_steps {
            root.step_editor(step);
        }
        commit(&mut tui, Txn::Frame, &mut root, started)?;
        wait_for_step(&mut input).await?;
    }
    input.shutdown();

    {
        let marker = "\x1b]999;PI_TUI_UG=DONE-MARKER\x07";
        let mut out = io::stdout();
        out.write_all(marker.as_bytes())?;
        out.flush()?;
    }
    root.enter(Phase::Done);
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    guard.restore();
    Ok(ExitCode::SUCCESS)
}

fn commit(
    tui: &mut Tui<StdoutOwner>,
    txn: Txn,
    root: &mut GauntletRoot,
    started: Instant,
) -> io::Result<()> {
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard unicode-gauntlet timeout",
        ));
    }
    let draw_started = Instant::now();
    root.advance_generation();
    tui.commit(txn, root)?;
    if draw_started.elapsed() > DRAW_DEADLINE {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "draw exceeded hard timeout (possible cursor-query deadlock)",
        ));
    }
    Ok(())
}

/// Blocks until one harness step event arrives (any key) so the PTY harness
/// controls gauntlet transitions. Fails loudly on step starvation.
async fn wait_for_step(input: &mut TerminalInput) -> io::Result<()> {
    let receiver = input.receiver_mut();
    match tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await {
        Ok(Some(_event)) => Ok(()),
        Ok(None) => Err(io::Error::other("input channel closed before step")),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "step starvation: harness did not advance the unicode gauntlet",
        )),
    }
}
