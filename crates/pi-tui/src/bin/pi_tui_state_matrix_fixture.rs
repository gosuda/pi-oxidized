//! State-matrix conformance fixture for TUI-V1 (issue #76).
//!
//! Drives the eight TUI conversation states through the real
//! `Tui` / probe / guard pipeline so the PTY harness can prove each state
//! renders per the quality bar (no full-screen clears, balanced
//! synchronized-output markers, deterministic k>=3 canonical transcripts):
//!
//! - `empty`        — empty conversation viewport (no messages)
//! - `loading`      — real [`Loader`] component, pinned frame (no timer)
//! - `retry`        — retry copy mirroring `crates/pi` `retry_message`
//! - `queue`        — queued follow-up + steering lines mirroring
//!   `crates/pi` `build_pending` shapes
//! - `streaming`    — real [`Loader`] plus deterministic stream chunks
//! - `error`        — error copy mirroring `crates/pi` `Error: {msg}` shape
//! - `focus-marked` — real [`Input`] component, focused then unfocused
//! - `ext-ui`       — railed extension message, widget slot, overlay marker
//!
//! Every rendered hint derives from the keybinding registry (TUI-T1).
//! Stage-3 writes are wrapped with OSC transaction markers identical to
//! `pi_tui_pty_fixture` / `pi_tui_ext_fixture` so the harness recovers write
//! boundaries after kernel-level write coalescing.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::{Input, Loader, LoaderIndicatorOptions};
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
/// wall-clock bytes can enter canonical transcripts).
const LOADER_FRAMES: [&str; 1] = ["⠋"];

// ---------------------------------------------------------------------------
// State-matrix root component
// ---------------------------------------------------------------------------

/// State identifiers — rendered as `STATUS` checkpoints for settle predicates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Empty,
    Loading,
    Retry,
    Queue,
    Streaming,
    Error,
    FocusMarked,
    ExtUi,
    Done,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Loading => "loading",
            Self::Retry => "retry",
            Self::Queue => "queue",
            Self::Streaming => "streaming",
            Self::Error => "error",
            Self::FocusMarked => "focus-marked",
            Self::ExtUi => "ext-ui",
            Self::Done => "DONE-MARKER",
        }
    }
}

struct StateMatrixRoot {
    phase: Phase,
    /// Deterministic per-state text lines.
    lines: Vec<String>,
    /// Real `Loader` child (loading / streaming states).
    loader: Option<Loader>,
    /// Real `Input` child (focus-marked state).
    input: Option<Input>,
    /// Monotonic commit generation (script-deterministic).
    generation: u64,
}

impl StateMatrixRoot {
    fn new() -> Self {
        Self {
            phase: Phase::Empty,
            lines: Vec::new(),
            loader: None,
            input: None,
            generation: 0,
        }
    }

    /// Deterministic loader pinned at frame 0 (no internal timer).
    fn pinned_loader(message: &str) -> Loader {
        Loader::new(
            str::to_owned,
            str::to_owned,
            message,
            Some(LoaderIndicatorOptions {
                frames: Some(LOADER_FRAMES.iter().map(|s| (*s).to_owned()).collect()),
                interval_ms: None,
            }),
        )
    }

    /// Retry copy mirrors `crates/pi/src/modes/interactive/status.rs`
    /// (`retry_message`): `Retrying (a/m) in Ns… (key to cancel)` with the
    /// interrupt hint derived from the keybinding registry.
    fn retry_line() -> String {
        format!(
            "Retrying (1/3) in 5s… ({} to cancel)",
            pi_tui::keybindings::key_text("app.interrupt")
        )
    }

    /// Per-state lines. The first line is always the bare state label: the
    /// Tui paints per-cell diffs, so a label sharing cells with the previous
    /// state's label can fragment on the wire — a full-line label repaints
    /// contiguously and gives the harness an unambiguous settle marker.
    fn lines_for(phase: Phase) -> Vec<String> {
        let mut lines = vec![phase.label().to_owned()];
        lines.extend(Self::state_lines(phase));
        lines
    }

    fn state_lines(phase: Phase) -> Vec<String> {
        match phase {
            Phase::Empty => vec!["EMPTY no messages".to_owned()],
            Phase::Retry => vec![Self::retry_line(), "RETRY attempt 1 of 3".to_owned()],
            Phase::Queue => vec![
                "→ queued: verification queued follow-up".to_owned(),
                "↳ steer: verification steering note".to_owned(),
                "all queued follow-ups will send after this turn".to_owned(),
            ],
            Phase::Streaming => vec![
                "STREAM chunk verification-stream-0001".to_owned(),
                "STREAM chunk verification-stream-0002".to_owned(),
                "STREAM chunk verification-stream-0003".to_owned(),
            ],
            Phase::Error => {
                vec!["Error: request failed after 3 attempts (verification-provider)".to_owned()]
            }
            Phase::Loading | Phase::FocusMarked => Vec::new(),
            Phase::ExtUi => vec![
                "│ EXT: verification ext-state-message".to_owned(),
                "WIDGET footer: verification-ext-widget".to_owned(),
                "OVL [*] verification-ext-overlay".to_owned(),
            ],
            Phase::Done => vec!["STATE-MATRIX COMPLETE".to_owned()],
        }
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.lines = Self::lines_for(phase);
        self.loader = match phase {
            Phase::Loading => Some(Self::pinned_loader("working")),
            Phase::Streaming => Some(Self::pinned_loader("streaming")),
            _ => None,
        };
        if phase == Phase::FocusMarked {
            let mut input = Input::new();
            input.set_value("verification focus probe");
            input.set_focused(true);
            self.input = Some(input);
        } else {
            self.input = None;
        }
    }

    fn set_input_focused(&mut self, focused: bool) {
        if let Some(input) = self.input.as_mut() {
            input.set_focused(focused);
        }
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

impl Component for StateMatrixRoot {
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
            let mut col = 0usize;
            for ch in line.chars().take(width) {
                let x = area
                    .x
                    .saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
                let ch = if ch.is_control() { ' ' } else { ch };
                buf[(x, *row)].set_char(ch);
                col += 1;
            }
            // Direct cell writer under in-place rendering: blank the rest of
            // the row span (reset-buffer parity) and claim it.
            for tail in col..width {
                let x = area
                    .x
                    .saturating_add(u16::try_from(tail).unwrap_or(u16::MAX));
                buf[(x, *row)].reset();
            }
            pi_tui::frame::claim_opaque_span(ratatui::layout::Rect {
                x: area.x,
                y: *row,
                width: u16::try_from(width).unwrap_or(u16::MAX),
                height: 1,
            });
            *row = row.saturating_add(1);
        };

        put_line(&format!("STATUS {}", self.phase.label()), buf, &mut row);
        for line in &self.lines {
            put_line(line, buf, &mut row);
        }
        if let Some(loader) = self.loader.as_mut()
            && row < bottom
        {
            let remaining = bottom.saturating_sub(row);
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
        if let Some(input) = self.input.as_mut()
            && row < bottom
        {
            let remaining = bottom.saturating_sub(row);
            input.render(
                Rect {
                    x: area.x,
                    y: row,
                    width: area.width,
                    height: remaining,
                },
                buf,
            );
            let used = input.measure(area.width).min(remaining);
            row = row.saturating_add(used);
        }
        put_line(&format!("GEN {}", self.generation), buf, &mut row);
        put_line("FOOTER pi-tui-state-matrix", buf, &mut row);
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
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "Unix arm can return real poll/read I/O errors; callers need one shared io::Result contract across platforms"
    )
)]
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
            let _ = writeln!(io::stderr(), "pi_tui_state_matrix_fixture error: {err}");
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
        writeln!(
            io::stdout(),
            "pi_tui_state_matrix_fixture (stepped; no arguments)"
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| io::Error::other(format!("runtime: {err}")))?;

    runtime.block_on(async move { run_matrix(started).await })
}

// ---------------------------------------------------------------------------
// StdoutOwner (identical to pi_tui_ext_fixture)
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
// Matrix runner
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_lines,
    reason = "one sequential state-matrix walk; splitting would scatter the checkpoint order"
)]
async fn run_matrix(started: Instant) -> io::Result<ExitCode> {
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

    // Stepped state matrix: each state renders, then the fixture blocks on
    // one harness step event so the PTY harness settles per-state snapshots
    // and per-state quality-bar checkpoints at real boundaries.
    let mut input = TerminalInput::spawn();
    let mut root = StateMatrixRoot::new();

    let phases = [
        Phase::Empty,
        Phase::Loading,
        Phase::Retry,
        Phase::Queue,
        Phase::Streaming,
        Phase::Error,
        Phase::FocusMarked,
        Phase::ExtUi,
    ];
    for phase in phases {
        // Wire-level state checkpoint: the Tui paints per-cell diffs, so an
        // in-frame label can fragment on the wire. The OSC 999 checkpoint is
        // written verbatim (outside stage-3, like the TXN markers) and gives
        // the harness an unfragmentable contiguous settle marker.
        {
            let marker = format!("\x1b]999;PI_TUI_STATE={}\x07", phase.label());
            let mut out = io::stdout();
            out.write_all(marker.as_bytes())?;
            out.flush()?;
        }
        root.enter(phase);
        commit(&mut tui, Txn::Frame, &mut root, started)?;
        if phase == Phase::FocusMarked {
            // Focused commit (hardware cursor annotation), then the
            // unfocused commit (cursor absent) under one step.
            root.set_input_focused(false);
            commit(&mut tui, Txn::Frame, &mut root, started)?;
        }
        wait_for_step(&mut input).await?;
    }
    input.shutdown();

    {
        let marker = "\x1b]999;PI_TUI_STATE=DONE-MARKER\x07";
        let mut out = io::stdout();
        out.write_all(marker.as_bytes())?;
        out.flush()?;
    }
    root.enter(Phase::Done);
    commit(&mut tui, Txn::Frame, &mut root, started)?;

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
    root: &mut StateMatrixRoot,
    started: Instant,
) -> io::Result<()> {
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard state-matrix timeout",
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
/// controls state transitions. Fails loudly on step starvation.
async fn wait_for_step(input: &mut TerminalInput) -> io::Result<()> {
    let receiver = input.receiver_mut();
    match tokio::time::timeout(Duration::from_secs(20), receiver.recv()).await {
        Ok(Some(_event)) => Ok(()),
        Ok(None) => Err(io::Error::other("input channel closed before step")),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "step starvation: harness did not advance the state matrix",
        )),
    }
}
