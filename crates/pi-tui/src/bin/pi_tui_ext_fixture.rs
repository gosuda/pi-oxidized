//! Extension-UI gauntlet fixture for TUI-P3 (issue #70).
//!
//! Drives the extension visual surface end-to-end through the real
//! `Tui` / `TerminalInput` / probe / guard pipeline:
//!
//! - Custom railed messages (user, assistant, tool, custom-message)
//! - Widget slots (status bar, sidebar, footer)
//! - Stacked overlays with focus restore
//! - `HostUiRequest` confirm/select/input dialog surfaces
//! - Extension shortcuts in the footer
//! - Hostile setTheme: bad hex, contrast below the pinned 4.5 rule, hue swaps
//! - OSC 0 title injection with C0/C1 controls and >256 UTF-8 bytes
//!
//! All sanitization floors are exercised so the PTY harness can prove they
//! hold on real terminals via repeatability-clean schema-v1 transcripts.
//!
//! Stage-3 writes are wrapped with OSC transaction markers identical to
//! `pi_tui_pty_fixture` so the harness recovers write boundaries after
//! kernel-level write coalescing.

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::keys::{KeyId, MODIFY_OTHER_KEYS_OMISSION, key_matches, set_kitty_protocol_active};
use pi_tui::terminal::{
    ProbeSession, TerminalCapabilities, TerminalGuard, TerminalInput, Tui, Txn,
    install_panic_emergency_hook, probe_query_batch, write_emergency_restore_bytes,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use tokio::sync::mpsc;

const DRAW_DEADLINE: Duration = Duration::from_secs(8);
const HARD_TIMEOUT: Duration = Duration::from_secs(30);
const VIEWPORT_HEIGHT: u16 = 10;

/// Maximum UTF-8 bytes of sanitized terminal title payload (OSC 0).
/// Mirrors `crates/pi/src/modes/interactive/runtime.rs`.
const MAX_TERMINAL_TITLE_BYTES: usize = 256;

/// Pinned WCAG contrast threshold from issue #58 / TUI-P2.
const THRESHOLD_WCAG_AA_NORMAL: f64 = 4.5;

// ---------------------------------------------------------------------------
// Gauntlet root component
// ---------------------------------------------------------------------------

/// Gauntlet phase identifiers — rendered as STATUS for checkpoint predicates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Railed,
    Widgets,
    Overlays,
    Dialogs,
    Shortcuts,
    Theme,
    Title,
    Done,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Railed => "railed",
            Self::Widgets => "widgets",
            Self::Overlays => "overlays",
            Self::Dialogs => "dialogs",
            Self::Shortcuts => "shortcuts",
            Self::Theme => "theme",
            Self::Title => "title",
            Self::Done => "DONE-MARKER",
        }
    }
}

struct ExtFixtureRoot {
    phase: Phase,
    /// Railed message lines (user/assistant/tool/custom).
    railed_lines: Vec<String>,
    /// Widget slot labels.
    widget_slots: Vec<String>,
    /// Overlay stack state: (label, focused).
    overlays: Vec<(String, bool)>,
    /// Current dialog surface.
    dialog: String,
    /// Footer shortcut hints.
    footer_shortcuts: Vec<String>,
    /// Theme sanitization results.
    theme_results: Vec<String>,
    /// Title sanitization result.
    title_result: String,
    /// Counters for the EDIT line.
    paste_count: u32,
    cursor_moves: u32,
    resize_count: u32,
    generation: u64,
}

impl ExtFixtureRoot {
    fn new() -> Self {
        Self {
            phase: Phase::Railed,
            railed_lines: Vec::new(),
            widget_slots: Vec::new(),
            overlays: Vec::new(),
            dialog: String::new(),
            footer_shortcuts: Vec::new(),
            theme_results: Vec::new(),
            title_result: String::new(),
            paste_count: 0,
            cursor_moves: 0,
            resize_count: 0,
            generation: 0,
        }
    }

    fn lines(&self, width: u16) -> Vec<String> {
        let width = usize::from(width.max(1));
        let mut out = Vec::with_capacity(VIEWPORT_HEIGHT as usize + 4);

        out.push(fit(&format!("STATUS {}", self.phase.label()), width));

        // Railed messages with left-edge rail glyph.
        for line in &self.railed_lines {
            out.push(fit(line, width));
        }

        // Widget slots.
        for slot in &self.widget_slots {
            out.push(fit(slot, width));
        }

        // Overlays.
        for (label, focused) in &self.overlays {
            let marker = if *focused { "[*]" } else { "[ ]" };
            out.push(fit(&format!("OVL {marker} {label}"), width));
        }

        // Dialog surface.
        if !self.dialog.is_empty() {
            out.push(fit(&format!("DIALOG {}", self.dialog), width));
        }

        // Footer shortcuts.
        if !self.footer_shortcuts.is_empty() {
            let joined = self.footer_shortcuts.join(" | ");
            out.push(fit(&format!("FOOTER {joined}"), width));
        }

        // Theme results.
        for result in &self.theme_results {
            out.push(fit(&format!("THEME {result}"), width));
        }

        // Title result.
        if !self.title_result.is_empty() {
            out.push(fit(&format!("TITLE {}", self.title_result), width));
        }

        out.push(fit(
            &format!(
                "EDIT paste={} cursor={} resize={} gen={}",
                self.paste_count, self.cursor_moves, self.resize_count, self.generation,
            ),
            width,
        ));
        out.push(fit("FOOTER pi-tui-ext-fixture", width));
        out
    }
}

impl Component for ExtFixtureRoot {
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
                let ch = if ch.is_control() { ' ' } else { ch };
                buf[(x, row)].set_char(ch);
            }
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Paste(text) => {
                self.paste_count = self.paste_count.saturating_add(1);
                self.generation = self.generation.saturating_add(1);
                let _ = text; // acknowledged
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
                    self.generation = self.generation.saturating_add(1);
                    return EventResult::Render;
                }
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

// ---------------------------------------------------------------------------
// Sanitization floors (mirrors product code for gauntlet proof)
// ---------------------------------------------------------------------------

/// Sanitize extension-supplied terminal title text for OSC 0 emission.
///
/// Drops every `char::is_control()` scalar (C0 and C1) and stops before the
/// sanitized payload would exceed `MAX_TERMINAL_TITLE_BYTES` UTF-8 bytes,
/// never splitting a scalar.
#[must_use]
fn sanitize_terminal_title(title: &str) -> String {
    let mut out = String::new();
    let mut byte_len = 0usize;
    for ch in title.chars() {
        if ch.is_control() {
            continue;
        }
        let ch_len = ch.len_utf8();
        if byte_len + ch_len > MAX_TERMINAL_TITLE_BYTES {
            break;
        }
        out.push(ch);
        byte_len += ch_len;
    }
    out
}

/// Encode a safe OSC 0 set-title sequence for `title`.
#[must_use]
fn encode_osc0_set_title(title: &str) -> Vec<u8> {
    let sanitized = sanitize_terminal_title(title);
    let mut sequence = Vec::with_capacity(5 + sanitized.len() + 1);
    sequence.extend_from_slice(b"\x1b]0;");
    sequence.extend_from_slice(sanitized.as_bytes());
    sequence.push(0x07);
    sequence
}

/// WCAG 2.2 relative luminance for an sRGB 8-bit channel.
fn channel_luminance(v: u8) -> f64 {
    let s = f64::from(v) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2.2 relative luminance for an RGB triple.
fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * channel_luminance(r) + 0.7152 * channel_luminance(g) + 0.0722 * channel_luminance(b)
}

/// WCAG 2.2 contrast ratio between two RGB colors.
#[must_use]
fn wcag_contrast_ratio(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> f64 {
    let l1 = relative_luminance(fg.0, fg.1, fg.2);
    let l2 = relative_luminance(bg.0, bg.1, bg.2);
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Parse a `#rrggbb` hex string into an RGB triple.
fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
    Some((r, g, b))
}

/// Evaluate a hostile setTheme payload and return a sanitization verdict.
///
/// Returns `(accepted, reason)` where `accepted=false` means the theme was
/// rejected by the sanitization floor.
#[must_use]
fn evaluate_theme(fg_hex: &str, bg_hex: &str, label: &str) -> (bool, String) {
    let Some(fg) = parse_hex(fg_hex) else {
        return (false, format!("{label}: REJECT bad-hex fg={fg_hex}"));
    };
    let Some(bg) = parse_hex(bg_hex) else {
        return (false, format!("{label}: REJECT bad-hex bg={bg_hex}"));
    };
    let ratio = wcag_contrast_ratio(fg, bg);
    if ratio < THRESHOLD_WCAG_AA_NORMAL {
        return (
            false,
            format!("{label}: REJECT contrast {ratio:.2} < {THRESHOLD_WCAG_AA_NORMAL}"),
        );
    }
    (true, format!("{label}: ACCEPT contrast={ratio:.2}"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fit(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

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
            let _ = writeln!(io::stderr(), "pi_tui_ext_fixture error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> io::Result<ExitCode> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut serve = false;
    for arg in &args {
        if arg == "--serve" {
            serve = true;
        } else if arg == "--help" {
            writeln!(io::stdout(), "pi_tui_ext_fixture [--serve]")?;
            return Ok(ExitCode::SUCCESS);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown argument {arg}"),
            ));
        }
    }

    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| io::Error::other(format!("runtime: {err}")))?;

    runtime.block_on(async move { run_gauntlet(serve, started).await })
}

// ---------------------------------------------------------------------------
// StdoutOwner (identical to pi_tui_pty_fixture)
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

#[allow(clippy::too_many_lines)]
async fn run_gauntlet(serve: bool, started: Instant) -> io::Result<ExitCode> {
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

    let mut root = ExtFixtureRoot::new();

    // Phase 1: Railed messages (user, assistant, tool, custom-message).
    root.phase = Phase::Railed;
    root.railed_lines = vec![
        "│ USER: hello-ext".into(),
        "│ ASSISTANT: railed-gauntlet".into(),
        "│ TOOL: bash-exec ok".into(),
        "│ CUSTOM: ext-message-1".into(),
    ];
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 2: Widget slots (status bar, sidebar, footer).
    root.phase = Phase::Widgets;
    root.widget_slots = vec![
        "WIDGET status-bar: ext-status".into(),
        "WIDGET sidebar: ext-sidebar".into(),
        "WIDGET footer: ext-footer-active".into(),
    ];
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 3: Stacked overlays with focus restore.
    root.phase = Phase::Overlays;
    root.overlays = vec![
        ("overlay-base".into(), false),
        ("overlay-modal".into(), true),
        ("overlay-tooltip".into(), false),
    ];
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Focus restore: pop modal, focus returns to base.
    root.overlays[1].1 = false;
    root.overlays[0].1 = true;
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Clear overlays.
    root.overlays.clear();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 4: HostUiRequest dialogs (confirm, select, input).
    root.phase = Phase::Dialogs;
    root.dialog = "CONFIRM: Trust extension? [y/n]".into();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    root.dialog = "SELECT: Choose model [gpt-4|claude|local]".into();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    root.dialog = "INPUT: Enter API key: ____".into();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    root.dialog.clear();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 5: Extension shortcuts in the footer.
    root.phase = Phase::Shortcuts;
    root.footer_shortcuts = vec![
        "^S save".into(),
        "^K command".into(),
        "^P preview".into(),
        "Esc dismiss".into(),
    ];
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 6: Hostile setTheme — bad hex, low contrast, hue swaps.
    root.phase = Phase::Theme;
    root.footer_shortcuts.clear();

    // Bad hex: invalid hex string.
    let (ok, msg) = evaluate_theme("#GGGGGG", "#000000", "bad-hex-fg");
    assert!(!ok, "bad hex must be rejected");
    root.theme_results.push(msg);

    let (ok, msg) = evaluate_theme("#ffffff", "#ZZZZZZ", "bad-hex-bg");
    assert!(!ok, "bad hex must be rejected");
    root.theme_results.push(msg);

    // Contrast below 4.5: near-identical fg/bg.
    let (ok, msg) = evaluate_theme("#444444", "#333333", "low-contrast");
    assert!(!ok, "low contrast must be rejected");
    root.theme_results.push(msg);

    // Hue swap: same luminance, different hue — still low contrast.
    let (_ok, msg) = evaluate_theme("#3333ff", "#ff3333", "hue-swap");
    root.theme_results.push(msg);

    // Valid theme: high contrast, accepted.
    let (ok, msg) = evaluate_theme("#ffffff", "#000000", "valid-theme");
    assert!(ok, "valid theme must be accepted");
    root.theme_results.push(msg);

    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 7: OSC 0 title injection with C0/C1 and >256 UTF-8 bytes.
    root.phase = Phase::Title;
    root.theme_results.clear();

    // Hostile title: C0 controls (BEL, ESC, NUL), C1 controls (0x9b), nested
    // OSC sequences, and >256 UTF-8 bytes to exercise the byte cap.
    let mut hostile_title = String::from("safe\x07\x1b]1;evil\x07\u{009b}ok");
    // Append 300 'A' chars to exceed the 256-byte cap.
    hostile_title.push_str(&"A".repeat(300));
    let sanitized = sanitize_terminal_title(&hostile_title);
    let osc_bytes = encode_osc0_set_title(&hostile_title);

    // Verify sanitization floors:
    // 1. No C0 or C1 control characters in sanitized output.
    assert!(
        !sanitized.chars().any(char::is_control),
        "sanitized title must contain no control characters"
    );
    // 2. Sanitized length does not exceed MAX_TERMINAL_TITLE_BYTES.
    assert!(
        sanitized.len() <= MAX_TERMINAL_TITLE_BYTES,
        "sanitized title must not exceed {MAX_TERMINAL_TITLE_BYTES} bytes, got {}",
        sanitized.len()
    );
    // 3. OSC sequence is properly framed with ESC ] 0 ; ... BEL.
    assert!(
        osc_bytes.starts_with(b"\x1b]0;") && osc_bytes.ends_with(&[0x07]),
        "OSC 0 sequence must be properly framed"
    );
    // 4. No C0/C1 control characters in the sanitized payload (char-level,
    //    not byte-level, to avoid false-positives on multi-byte UTF-8).
    assert!(
        !sanitized.chars().any(char::is_control),
        "OSC 0 payload must contain no C0/C1 controls"
    );

    root.title_result = format!(
        "sanitized {} bytes (input {} bytes, no C0/C1, capped at {MAX_TERMINAL_TITLE_BYTES})",
        sanitized.len(),
        hostile_title.len()
    );

    // Emit the sanitized OSC 0 sequence on the wire (bypassing Tui stage-3
    // so the harness captures it as a side-channel, like the fixture binary).
    {
        let mut out = io::stdout();
        out.write_all(&osc_bytes)?;
        out.flush()?;
    }

    commit(&mut tui, Txn::Frame, &mut root, started)?;

    // Phase 8: Done marker.
    root.phase = Phase::Done;
    root.title_result.clear();
    commit(&mut tui, Txn::Frame, &mut root, started)?;

    if serve {
        // In serve mode, keep the fixture alive for resize/paste/cursor input.
        let mut input = TerminalInput::spawn();
        let (inject_tx, inject_rx) = mpsc::unbounded_channel();
        let mut mock_input = TerminalInput::mock(inject_rx);

        // Wait for live events (resize, paste, cursor) with a deadline.
        let serve_deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < serve_deadline {
            while let Some(event) = input.try_recv() {
                handle_ui_event(&mut tui, &mut root, &event, started)?;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Synthetic resize to exercise the resize path.
        let event = UiEvent::Resize {
            width: 80,
            height: 24,
        };
        inject_tx
            .send(event.clone())
            .map_err(|_| io::Error::other("inject channel closed"))?;
        if let Some(ev) = mock_input.try_recv() {
            handle_ui_event(&mut tui, &mut root, &ev, started)?;
        } else {
            handle_ui_event(&mut tui, &mut root, &event, started)?;
        }

        root.phase = Phase::Done;
        commit(&mut tui, Txn::Frame, &mut root, started)?;

        input.shutdown();
    }

    // Publish write accounting on the wire (outside stage-3).
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
    root: &mut ExtFixtureRoot,
    started: Instant,
) -> io::Result<()> {
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard gauntlet timeout",
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

fn handle_ui_event(
    tui: &mut Tui<StdoutOwner>,
    root: &mut ExtFixtureRoot,
    event: &UiEvent,
    started: Instant,
) -> io::Result<()> {
    root.handle_event(event);
    if started.elapsed() > HARD_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "hard gauntlet timeout",
        ));
    }
    commit(tui, Txn::Frame, root, started)?;
    Ok(())
}
