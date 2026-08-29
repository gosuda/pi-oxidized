//! Deterministic release-style TUI fixture for portable-pty no-flicker tests.
//!
//! Exercises the real `Tui` / `TerminalInput` / probe / guard pipeline with a
//! scripted streaming response, tool updates, resizes, paste, settle, plugin
//! frames, and explicit exit outcomes. Not the product binary.
//!
//! Stage-3 writes are wrapped with OSC transaction markers so the PTY harness
//! can prove settle `insert_before` + redraw share one serialized write even
//! after kernel-level write coalescing.

use std::env;
use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::keys::{
    KeyId, MODIFY_OTHER_KEYS_OMISSION, key_matches, key_press, set_kitty_protocol_active,
};
use pi_tui::terminal::{
    ProbeSession, ReanchorCause, SettledBlock, TerminalCapabilities, TerminalGuard, TerminalInput,
    Tui, Txn, install_panic_emergency_hook, probe_query_batch, write_emergency_restore_bytes,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use tokio::sync::mpsc;

const DRAW_DEADLINE: Duration = Duration::from_secs(8);
const HARD_TIMEOUT: Duration = Duration::from_secs(20);
const VIEWPORT_HEIGHT: u16 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExitMode {
    Success,
    Abort,
    ProviderError,
    Panic,
    Sigint,
}

impl ExitMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "success" => Some(Self::Success),
            "abort" => Some(Self::Abort),
            "provider-error" => Some(Self::ProviderError),
            "panic" => Some(Self::Panic),
            "sigint" => Some(Self::Sigint),
            _ => None,
        }
    }
}

struct FixtureRoot {
    status: String,
    stream: String,
    tool: String,
    plugin: String,
    editor: String,
    paste_count: u32,
    cursor_moves: u32,
    resize_count: u32,
    generation: u64,
}

impl FixtureRoot {
    fn new() -> Self {
        Self {
            status: "ready".into(),
            stream: String::new(),
            tool: String::new(),
            plugin: String::new(),
            editor: String::new(),
            paste_count: 0,
            cursor_moves: 0,
            resize_count: 0,
            generation: 0,
        }
    }

    fn lines(&self, width: u16) -> Vec<String> {
        let width = usize::from(width.max(1));
        let mut out = Vec::with_capacity(6);
        out.push(fit(&format!("STATUS {}", self.status), width));
        out.push(fit(&format!("STREAM {}", self.stream), width));
        out.push(fit(&format!("TOOL {}", self.tool), width));
        out.push(fit(&format!("PLUGIN {}", self.plugin), width));
        out.push(fit(
            &format!(
                "EDIT {} | paste={} cursor={} resize={} gen={}",
                self.editor,
                self.paste_count,
                self.cursor_moves,
                self.resize_count,
                self.generation
            ),
            width,
        ));
        out.push(fit("FOOTER pi-tui-pty-fixture", width));
        out
    }
}

impl Component for FixtureRoot {
    fn measure(&mut self, _width: u16) -> u16 {
        VIEWPORT_HEIGHT
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = self.lines(area.width);
        for (idx, line) in lines.into_iter().enumerate() {
            let row = area
                .y
                .saturating_add(u16::try_from(idx).unwrap_or(u16::MAX));
            if row >= area.y.saturating_add(area.height) {
                break;
            }
            for (col_idx, ch) in line.chars().enumerate() {
                let x = area
                    .x
                    .saturating_add(u16::try_from(col_idx).unwrap_or(u16::MAX));
                if x >= area.x.saturating_add(area.width) {
                    break;
                }
                // Ratatui rejects C0 controls in cell width; keep the grid printable.
                let ch = if ch.is_control() { ' ' } else { ch };
                buf[(x, row)].set_char(ch);
            }
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Paste(text) => {
                self.paste_count = self.paste_count.saturating_add(1);
                self.editor.push_str(&sanitize_visible(text));
                self.generation = self.generation.saturating_add(1);
                EventResult::Render
            }
            UiEvent::Key(key) => {
                if key_matches(key, &KeyId::from("left"))
                    || key_matches(key, &KeyId::from("right"))
                    || key_matches(key, &KeyId::from("up"))
                    || key_matches(key, &KeyId::from("down"))
                    || key_matches(key, &KeyId::from("home"))
                    || key_matches(key, &KeyId::from("end"))
                {
                    self.cursor_moves = self.cursor_moves.saturating_add(1);
                    self.generation = self.generation.saturating_add(1);
                    return EventResult::Render;
                }
                if let KeyCode::Char(ch) = key.code
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !ch.is_control()
                {
                    self.editor.push(ch);
                    self.generation = self.generation.saturating_add(1);
                    return EventResult::Render;
                }
                // Documented intentional omission: modifyOtherKeys is never parsed.
                let _ = MODIFY_OTHER_KEYS_OMISSION;
                EventResult::Ignored
            }
            UiEvent::Resize { .. } => {
                self.resize_count = self.resize_count.saturating_add(1);
                self.generation = self.generation.saturating_add(1);
                EventResult::Render
            }
            UiEvent::FocusGained | UiEvent::FocusLost => EventResult::Ignored,
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

fn fit(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

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
        // Use libc-level read through std after confirming readability.
        // Temporary nonblocking would race other threads; POLLIN + short read is enough.
        let mut handle = stdin.lock();
        // There is no safe nonblocking Read on StdinLock without O_NONBLOCK.
        // Fall back to reading only when poll said data is ready; a blocking read
        // here is bounded by the harness writing probe replies immediately.
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

fn sanitize_visible(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "pi_tui_pty_fixture error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut exit_mode = ExitMode::Success;
    let mut sync_output = true;
    let mut serve = false;
    for arg in &args {
        if let Some(mode) = arg.strip_prefix("--exit=") {
            exit_mode = ExitMode::parse(mode).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown exit mode {mode}"),
                )
            })?;
        } else if arg == "--no-sync" {
            sync_output = false;
        } else if arg == "--serve" {
            serve = true;
        } else if arg == "--help" {
            writeln!(
                io::stdout(),
                "pi_tui_pty_fixture [--exit=success|abort|provider-error|panic|sigint] [--no-sync] [--serve]"
            )?;
            return Ok(ExitCode::SUCCESS);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown argument {arg}"),
            ));
        }
    }

    if env::var_os("PI_TUI_NO_SYNC").is_some() {
        sync_output = false;
    }

    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| io::Error::other(format!("runtime: {err}")))?;

    runtime.block_on(async move { run_fixture(exit_mode, sync_output, serve, started).await })
}

#[allow(clippy::too_many_lines)]
async fn run_fixture(
    exit_mode: ExitMode,
    sync_output: bool,
    serve: bool,
    started: Instant,
) -> io::Result<ExitCode> {
    // Do not hold `stdout.lock()` across the lifetime of the fixture: `Tui`
    // stage-3 also writes to process stdout and would deadlock on the same mutex.
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

    // Stage-1 probes must leave the synchronized-output wrapper.
    let probe_bytes = probe_query_batch(true);
    guard.writer_mut().write_all(&probe_bytes)?;
    guard.writer_mut().flush()?;

    // Read probe replies from the real PTY stdin before EventStream ownership.
    crossterm::terminal::enable_raw_mode()?;
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
    // Deterministic seed only if the harness never answered.
    if !probe.is_complete() {
        let _ =
            probe.feed(b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R");
    }

    let mut caps = tokio::task::spawn_blocking(TerminalCapabilities::detect)
        .await
        .map_err(|err| io::Error::other(format!("capability detection join failed: {err}")))?;
    caps.sync_output = sync_output;
    let cursor = probe.apply_to(&mut caps).unwrap_or((0, 0));
    set_kitty_protocol_active(caps.kitty_keyboard());

    let size = match crossterm::terminal::size() {
        Ok((w, h)) => Size::new(w.max(20), h.max(8)),
        Err(_) => Size::new(80, 24),
    };
    // activate() re-enables raw mode (idempotent) and queues terminal modes.
    guard.activate(caps.kitty_keyboard())?;
    guard.set_viewport_bottom_row(size.height.saturating_sub(1));

    // Sole stdout owner after probes: the Tui stage-3 writer with txn markers.
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

    let mut root = FixtureRoot::new();
    root.status = "probed".into();
    commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;

    // Scripted product-like traffic: stream, tools, paste, cursor, settle, plugins.
    root.status = "streaming".into();
    for chunk in 0..12u32 {
        let _ = write!(root.stream, "L{chunk:02}-");
        root.stream.push_str(&"word ".repeat(8));
        commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;
        if started.elapsed() > HARD_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hard fixture timeout",
            ));
        }
    }

    for tool_idx in 0..4u32 {
        root.tool = format!("tool-{tool_idx} running");
        root.status = "tool".into();
        commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;
        root.tool = format!("tool-{tool_idx} done");
        commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;
    }

    // Settle long response into scrollback via insert_before + same-write redraw.
    let settled_lines: Vec<Line<'static>> = (0..8)
        .map(|i| Line::from(format!("SETTLED-ROW-{i:02} long assistant content block")))
        .collect();
    root.status = "settling".into();
    root.stream = "settled-tail".into();
    commit_with_deadline(
        &mut tui,
        Txn::Settle(vec![SettledBlock::Lines(settled_lines)]),
        &mut root,
        started,
    )?;

    // Inline reposition / viewport height change.
    root.status = "reposition".into();
    commit_with_deadline(&mut tui, Txn::SetViewportHeight(4), &mut root, started)?;
    commit_with_deadline(
        &mut tui,
        Txn::SetViewportHeight(VIEWPORT_HEIGHT),
        &mut root,
        started,
    )?;

    // Plugin frames (generation bumps, height-stable content).
    for plugin_gen in 1..=5u32 {
        root.plugin = format!("plugin-frame-{plugin_gen}");
        root.generation = u64::from(plugin_gen);
        root.status = "plugin".into();
        commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;
    }

    // Drive TerminalInput for paste + cursor movement + resizes.
    let mut input = TerminalInput::spawn();
    let (inject_tx, inject_rx) = mpsc::unbounded_channel();
    let mut mock_input = TerminalInput::mock(inject_rx);

    let resize_plan: [(u16, u16); 24] = [
        (80, 24),
        (40, 12),
        (20, 8),
        (12, 6),
        (10, 5),
        (8, 4),
        (16, 10),
        (32, 14),
        (64, 20),
        (100, 30),
        (120, 40),
        (200, 50),
        (24, 8),
        (18, 7),
        (14, 6),
        (11, 5),
        (9, 4),
        (28, 12),
        (48, 16),
        (72, 22),
        (96, 28),
        (160, 36),
        (60, 18),
        (80, 24),
    ];

    for (width, height) in resize_plan {
        while let Some(event) = input.try_recv() {
            handle_ui_event(&mut tui, &mut root, &event, started)?;
        }
        let event = UiEvent::Resize { width, height };
        inject_tx
            .send(event.clone())
            .map_err(|_| io::Error::other("inject channel closed"))?;
        if let Some(ev) = mock_input.try_recv() {
            handle_ui_event(&mut tui, &mut root, &ev, started)?;
        } else {
            handle_ui_event(&mut tui, &mut root, &event, started)?;
        }
        if started.elapsed() > HARD_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "hard fixture timeout",
            ));
        }
    }

    // Prefer live EventStream paste/cursor if the harness injected them; always
    // fall back to synthetic inject so the fixture remains deterministic.
    let paste_deadline = Instant::now() + Duration::from_millis(150);
    let mut saw_live_paste = false;
    let mut saw_live_cursor = false;
    while Instant::now() < paste_deadline {
        while let Some(event) = input.try_recv() {
            match &event {
                UiEvent::Paste(_) => saw_live_paste = true,
                UiEvent::Key(key)
                    if matches!(
                        key.code,
                        KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Home
                            | KeyCode::End
                    ) =>
                {
                    saw_live_cursor = true;
                }
                _ => {}
            }
            handle_ui_event(&mut tui, &mut root, &event, started)?;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    if !saw_live_paste {
        let paste = UiEvent::Paste("PASTED-BLOCK-line1\nline2".into());
        inject_tx
            .send(paste.clone())
            .map_err(|_| io::Error::other("inject channel closed"))?;
        if let Some(ev) = mock_input.try_recv() {
            handle_ui_event(&mut tui, &mut root, &ev, started)?;
        } else {
            handle_ui_event(&mut tui, &mut root, &paste, started)?;
        }
    }

    if !saw_live_cursor {
        for key in [
            key_press(KeyCode::Left, KeyModifiers::empty()),
            key_press(KeyCode::Right, KeyModifiers::empty()),
            key_press(KeyCode::Up, KeyModifiers::empty()),
            key_press(KeyCode::Down, KeyModifiers::empty()),
            key_press(KeyCode::Home, KeyModifiers::empty()),
            key_press(KeyCode::End, KeyModifiers::empty()),
            key_press(KeyCode::Char('x'), KeyModifiers::empty()),
        ] {
            let event = UiEvent::Key(key);
            inject_tx
                .send(event.clone())
                .map_err(|_| io::Error::other("inject channel closed"))?;
            if let Some(ev) = mock_input.try_recv() {
                handle_ui_event(&mut tui, &mut root, &ev, started)?;
            } else {
                handle_ui_event(&mut tui, &mut root, &event, started)?;
            }
        }
    }

    root.status = match exit_mode {
        ExitMode::Success => "success",
        ExitMode::Abort => "abort",
        ExitMode::ProviderError => "provider-error",
        ExitMode::Panic => "panic",
        ExitMode::Sigint => "sigint",
    }
    .into();
    commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;

    root.plugin = "DONE-MARKER".into();
    commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;

    if serve {
        root.status = "serving".into();
        root.plugin = "SERVE-READY".into();
        commit_with_deadline(&mut tui, Txn::Frame, &mut root, started)?;
        serve_live_events(&mut input, &mut tui, &mut root, started).await?;
    }

    // Publish write accounting on the wire for the harness (outside stage-3).
    {
        let log = write_log
            .lock()
            .map_err(|_| io::Error::other("write log poisoned"))?;
        let summary = format!(
            "\x1b]999;PI_TUI_TXN_COUNT={}\x07\x1b]999;PI_TUI_PASTE={}\x07\x1b]999;PI_TUI_CURSOR={}\x07\x1b]999;PI_TUI_RESIZE={}\x07",
            log.len(),
            root.paste_count,
            root.cursor_moves,
            root.resize_count
        );
        // Bypass Tui so accounting stays about stage-3 only; still sole post-probe
        // process stdout, just a harness side-channel.
        let mut out = io::stdout();
        out.write_all(summary.as_bytes())?;
        out.flush()?;
    }

    input.shutdown();

    match exit_mode {
        ExitMode::Success => {
            guard.restore();
            Ok(ExitCode::SUCCESS)
        }
        ExitMode::Abort => {
            guard.restore();
            Ok(ExitCode::from(130))
        }
        ExitMode::ProviderError => {
            guard.restore();
            Ok(ExitCode::from(1))
        }
        ExitMode::Panic => {
            // Restore modes first so the panic hook's emergency sequence is the
            // only extra restore on the wire, then panic for the harness path.
            // The hook uses write_emergency_restore_bytes (includes one 2026l).
            // Emit a balancing open first so the whole stream stays balanced.
            {
                let mut out = io::stdout();
                let _ = out.write_all(b"\x1b[?2026h");
                let _ = out.flush();
            }
            #[allow(clippy::panic)]
            {
                panic!("pi_tui_pty_fixture intentional panic");
            }
        }
        ExitMode::Sigint => {
            #[cfg(unix)]
            {
                // Balanced emergency close: open then close, then mode restore.
                {
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x1b[?2026h");
                    let _ = out.flush();
                }
                {
                    let mut out = io::stdout();
                    let _ = write_emergency_restore_bytes(&mut out);
                }
                guard.restore();
                thread::sleep(Duration::from_millis(30));
                let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT);
                thread::sleep(Duration::from_millis(50));
                Ok(ExitCode::from(130))
            }
            #[cfg(not(unix))]
            {
                {
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x1b[?2026h");
                    let _ = out.flush();
                }
                {
                    let mut out = io::stdout();
                    let _ = write_emergency_restore_bytes(&mut out);
                }
                guard.restore();
                Ok(ExitCode::from(130))
            }
        }
    }
}

async fn serve_live_events(
    input: &mut TerminalInput,
    tui: &mut Tui<StdoutOwner>,
    root: &mut FixtureRoot,
    started: Instant,
) -> io::Result<()> {
    let mut pending = None;
    loop {
        let event = match pending.take() {
            Some(event) => event,
            None => match input.recv().await {
                Some(event) => event,
                None => return Ok(()),
            },
        };

        let UiEvent::Resize { width, height } = event else {
            // portable-pty's UnixMasterWriter Drop sends newline+VEOT as the
            // master-EOF stand-in; in raw mode that arrives as Ctrl+D, not a
            // kernel stdin EOF, so treat it as the serve terminator.
            if let UiEvent::Key(key) = &event
                && key.code == KeyCode::Char('d')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                return Ok(());
            }
            handle_ui_event(tui, root, &event, started)?;
            continue;
        };

        // Coalesce a back-to-back resize storm into one note_resize + Reanchor.
        let mut latest = (width, height);
        let _ = root.handle_event(&UiEvent::Resize { width, height });
        tokio::task::yield_now().await;
        while let Some(next) = input.try_recv() {
            match next {
                UiEvent::Resize { width, height } => {
                    latest = (width, height);
                    let _ = root.handle_event(&UiEvent::Resize { width, height });
                }
                other => {
                    pending = Some(other);
                    break;
                }
            }
        }
        tui.note_resize(latest.0.max(1), latest.1.max(1));
        commit_with_deadline(tui, Txn::Reanchor(ReanchorCause::Resize), root, started)?;
    }
}

fn handle_ui_event(
    tui: &mut Tui<StdoutOwner>,
    root: &mut FixtureRoot,
    event: &UiEvent,
    started: Instant,
) -> io::Result<()> {
    if let UiEvent::Resize { width, height } = event {
        tui.note_resize((*width).max(1), (*height).max(1));
        let result = root.handle_event(event);
        if result.needs_render() || result.is_handled() {
            commit_with_deadline(tui, Txn::Reanchor(ReanchorCause::Resize), root, started)?;
        }
        return Ok(());
    }
    let result = root.handle_event(event);
    if result.needs_render() {
        commit_with_deadline(tui, Txn::Frame, root, started)?;
    }
    Ok(())
}

fn commit_with_deadline(
    tui: &mut Tui<StdoutOwner>,
    txn: Txn,
    root: &mut FixtureRoot,
    started: Instant,
) -> io::Result<()> {
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard fixture timeout",
        ));
    }
    let draw_started = Instant::now();
    tui.commit(txn, root)?;
    if draw_started.elapsed() > DRAW_DEADLINE {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "draw exceeded hard timeout (possible cursor-query deadlock)",
        ));
    }
    Ok(())
}

/// Sole process stdout writer used by `Tui` stage-3.
///
/// Buffers until `flush` (Tui always `write_all` + `flush` per transaction) and
/// wraps each transaction with OSC markers so the harness can recover write
/// boundaries after PTY stream coalescing.
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
        // OSC 999 is ignored by compliant terminals / avt for rendering purposes.
        let begin = format!("\x1b]999;PI_TUI_TXN_BEGIN={id}\x07");
        let end = format!("\x1b]999;PI_TUI_TXN_END={id}\x07");
        self.out.write_all(begin.as_bytes())?;
        self.out.write_all(&payload)?;
        self.out.write_all(end.as_bytes())?;
        self.out.flush()
    }
}
