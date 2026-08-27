//! Accessibility gauntlet fixture for TUI-V6 (issue #72).
//!
//! Drives the accessibility-relevant rendering surfaces through the real
//! `Tui` / `Loader` / probe / guard pipeline so the PTY harness can record
//! the canonical settled frames the three automated accessibility
//! invariants consume (notice persistence, static sufficiency,
//! anti-chatter):
//!
//! - `notice`             — railed transient notice (product `push_notice`
//!                          shape: rail + `[export]` label + text) present
//!                          in two settled frames whose non-notice content
//!                          differs (a scripted content change)
//! - `spinner-working`    — real [`Loader`], pinned frame, product
//!                          `status_message` shape with kind + elapsed +
//!                          cancel hint, elapsed stepping 4s → 5s → 6s
//! - `spinner-retry`      — same shape, kind `Retrying…`, 2s → 3s
//! - `spinner-compaction` — same shape, kind `Compacting context…`, 7s → 8s
//!
//! Every status line mirrors `crates/pi/src/modes/interactive/status.rs`
//! (`status_message`): `"{kind}{elapsed} · {key} to cancel"` with the
//! interrupt hint derived from the keybinding registry (rebind-proof).
//! Elapsed seconds advance by script, never wall-clock, so k>=3 canonical
//! transcripts stay byte-identical. Stage-3 writes are wrapped with OSC
//! transaction markers identical to `pi_tui_state_matrix_fixture` so the
//! harness recovers write boundaries after kernel-level write coalescing.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::{Loader, LoaderIndicatorOptions, Padded};
use pi_tui::keys::set_kitty_protocol_active;
use pi_tui::terminal::{
    ProbeSession, TerminalCapabilities, TerminalGuard, TerminalInput, Tui, Txn,
    install_panic_emergency_hook, probe_query_batch, write_emergency_restore_bytes,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};

const DRAW_DEADLINE: Duration = Duration::from_secs(8);
const HARD_TIMEOUT: Duration = Duration::from_secs(30);
const VIEWPORT_HEIGHT: u16 = 10;

/// Deterministic loader frame set (frame 0 pinned — never advanced, so no
/// wall-clock bytes can enter canonical transcripts; the invariants never
/// require a motion gate per TUI-G1).
const LOADER_FRAMES: [&str; 1] = ["⠋"];

/// Notice text mirroring the product `push_notice("export", …)` shape
/// (`CustomMessageView { custom_type, text }` rendered as a railed
/// `[export]` label line).
const NOTICE_TEXT: &str = "Session exported to: verification-export.jsonl";

// ---------------------------------------------------------------------------
// Accessibility gauntlet root component
// ---------------------------------------------------------------------------

/// Scripted frame identifiers — one settled frame per step, each rendered
/// as a unique `STATUS`/OSC-999 checkpoint so the harness settles every
/// frame at a real input boundary (the state-matrix stepped design: the
/// fixture never renders ahead of the harness, so no settle can starve).
/// Spinner frames carry their scripted elapsed counter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Notice,
    NoticeTick,
    SpinnerWorking { elapsed: u32 },
    SpinnerRetry { elapsed: u32 },
    SpinnerCompaction { elapsed: u32 },
    Done,
}

impl Phase {
    /// Unique checkpoint label per scripted frame.
    fn label(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::NoticeTick => "notice-tick-1",
            Self::SpinnerWorking { elapsed } => match elapsed {
                4 => "working-4s",
                5 => "working-5s",
                _ => "working-6s",
            },
            Self::SpinnerRetry { elapsed } => match elapsed {
                2 => "retry-2s",
                _ => "retry-3s",
            },
            Self::SpinnerCompaction { elapsed } => match elapsed {
                7 => "compaction-7s",
                _ => "compaction-8s",
            },
            Self::Done => "DONE-MARKER",
        }
    }

    /// Kind label for spinner frames (product `StatusKind` label shapes).
    fn kind_label(self) -> Option<&'static str> {
        match self {
            Self::SpinnerWorking { .. } => Some("Working…"),
            Self::SpinnerRetry { .. } => Some("Retrying…"),
            Self::SpinnerCompaction { .. } => Some("Compacting context…"),
            _ => None,
        }
    }

    fn elapsed(self) -> Option<u32> {
        match self {
            Self::SpinnerWorking { elapsed }
            | Self::SpinnerRetry { elapsed }
            | Self::SpinnerCompaction { elapsed } => Some(elapsed),
            _ => None,
        }
    }

    /// The scripted frame order (the harness steps between frames).
    const ORDER: [Phase; 10] = [
        Phase::Notice,
        Phase::NoticeTick,
        Phase::SpinnerWorking { elapsed: 4 },
        Phase::SpinnerWorking { elapsed: 5 },
        Phase::SpinnerWorking { elapsed: 6 },
        Phase::SpinnerRetry { elapsed: 2 },
        Phase::SpinnerRetry { elapsed: 3 },
        Phase::SpinnerCompaction { elapsed: 7 },
        Phase::SpinnerCompaction { elapsed: 8 },
        Phase::Done,
    ];
}

struct A11yRoot {
    phase: Phase,
    /// Monotonic commit generation (script-deterministic).
    generation: u64,
}

impl A11yRoot {
    fn new() -> Self {
        Self {
            phase: Phase::Notice,
            generation: 0,
        }
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Frame lines. Both notice frames keep the railed notice line stable
    /// while the tick frame adds the tick line — the notice persists
    /// across a content change as canonical content, and the two frames'
    /// announcements differ (anti-chatter).
    fn stage_lines(&self) -> Vec<String> {
        match self.phase {
            Phase::Notice => vec![format!("│ [export] {NOTICE_TEXT}")],
            Phase::NoticeTick => vec![
                format!("│ [export] {NOTICE_TEXT}"),
                "NOTICE-TICK 1 (notice persists)".to_owned(),
            ],
            Phase::SpinnerWorking { .. }
            | Phase::SpinnerRetry { .. }
            | Phase::SpinnerCompaction { .. } => Vec::new(),
            Phase::Done => vec!["A11Y-GAUNTLET COMPLETE".to_owned()],
        }
    }

    /// Spinner-status line mirroring the product `status_message` builder
    /// (`crates/pi/src/modes/interactive/status.rs`): the cancel hint is
    /// part of the message string and derives from the keybinding registry.
    fn status_line(&self) -> Option<String> {
        let kind = self.phase.kind_label()?;
        let elapsed = self.phase.elapsed().unwrap_or(0);
        Some(format!(
            "{kind} {elapsed}s · {} to cancel",
            pi_tui::keybindings::key_text("app.interrupt")
        ))
    }

    /// Real `Loader` for spinner stages, pinned at frame 0 (mirrors the
    /// product `build_status` construction: `Padded` restores the column
    /// the loader self-indents).
    fn build_loader(&self) -> Option<Box<dyn Component>> {
        let message = self.status_line()?;
        let mut loader = Loader::new(
            str::to_owned,
            str::to_owned,
            message,
            Some(LoaderIndicatorOptions {
                frames: Some(LOADER_FRAMES.iter().map(|s| (*s).to_owned()).collect()),
                interval_ms: None,
            }),
        );
        loader.set_frame_index(0);
        let mut padded = Padded::with_padding(1, 0);
        padded.add_child(loader);
        Some(Box::new(padded))
    }
}

impl Component for A11yRoot {
    fn measure(&mut self, _width: u16) -> u16 {
        VIEWPORT_HEIGHT
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let bottom = area.y.saturating_add(area.height);
        let mut row = area.y;

        let put_line = |line: &str, buf: &mut Buffer, row: &mut u16| {
            if *row >= bottom {
                return;
            }
            for (col, ch) in line.chars().take(width).enumerate() {
                let x = area
                    .x
                    .saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
                let ch = if ch.is_control() { ' ' } else { ch };
                buf[(x, *row)].set_char(ch);
            }
            *row = row.saturating_add(1);
        };

        put_line(&format!("STATUS {}", self.phase.label()), buf, &mut row);
        for line in &self.stage_lines() {
            put_line(line, buf, &mut row);
        }
        if let Some(loader) = self.build_loader()
            && row < bottom
        {
            let remaining = bottom.saturating_sub(row);
            let mut loader = loader;
            loader.render(
                Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: remaining,
                },
                buf,
            );
            let used = loader.measure(area.width).min(remaining);
            row = row.saturating_add(used);
        }
        put_line(&format!("GEN {}", self.generation), buf, &mut row);
        put_line("FOOTER pi-tui-a11y-gauntlet", buf, &mut row);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {}
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
            let _ = writeln!(io::stderr(), "pi_tui_a11y_fixture error: {err}");
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
        writeln!(io::stdout(), "pi_tui_a11y_fixture (stepped; no arguments)")?;
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

/// Stage walk order. Every stage: checkpoint marker, one commit per
/// scripted sub-frame, then one harness step (space) advances the stage.
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
        let _ =
            probe.feed(b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R");
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

    // Stepped gauntlet: every scripted frame renders behind its own unique
    // checkpoint marker, then the fixture blocks on one harness step event
    // so the PTY harness settles each frame at a real input boundary — the
    // fixture never renders ahead of the harness, so no settle can starve.
    // Consecutive frames change content (notice tick, elapsed step), so
    // consecutive settled announcements are never identical.
    let mut input = TerminalInput::spawn();
    let mut root = A11yRoot::new();

    for phase in Phase::ORDER {
        root.phase = phase;
        {
            let marker = format!("\x1b]999;PI_TUI_STAGE={}\x07", phase.label());
            let mut out = io::stdout();
            out.write_all(marker.as_bytes())?;
            out.flush()?;
        }
        commit(&mut tui, Txn::Frame, &mut root, started)?;
        if phase != Phase::Done {
            wait_for_step(&mut input).await?;
        }
    }
    input.shutdown();

    // Publish write accounting on the wire (outside stage-3).
    {
        let log = write_log
            .lock()
            .map_err(|_| io::Error::other("write log poisoned"))?;
        let summary = format!("\x1b]999;PI_TUI_TXN_COUNT={}\x07", log.len());
        let mut out = io::stdout();
        out.write_all(summary.as_bytes())?;
        out.flush()?;
    }

    guard.restore();
    Ok(ExitCode::SUCCESS)
}

fn commit(
    tui: &mut Tui<StdoutOwner>,
    txn: Txn,
    root: &mut A11yRoot,
    started: Instant,
) -> io::Result<()> {
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard a11y-gauntlet timeout",
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
/// controls stage transitions. Fails loudly on step starvation.
async fn wait_for_step(input: &mut TerminalInput) -> io::Result<()> {
    let receiver = input.receiver_mut();
    match tokio::time::timeout(Duration::from_secs(20), receiver.recv()).await {
        Ok(Some(_event)) => Ok(()),
        Ok(None) => Err(io::Error::other("input channel closed before step")),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "step starvation: harness did not advance the a11y gauntlet",
        )),
    }
}
