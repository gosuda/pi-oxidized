#![allow(
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::match_same_arms,
    clippy::needless_late_init,
    clippy::no_effect_underscore_binding,
    clippy::cast_possible_truncation,
    dead_code,
    unused_assignments
)]
//! Adversarial E2E PTY tests: hostile input against the live `pi_tui_pty_fixture`.
//!
//! Where `pty_no_flicker` proves the well-behaved drive cycle, these tests
//! attack the serve-phase input path with malformed escape soup, mid-stream
//! probe-reply injection, degenerate geometry, oversized/mangled pastes, key
//! bursts, unterminated-paste EOF, and resize-batch contract violations.
//! The fixture under test is the real `Tui`/`TerminalInput` pipeline, so every
//! case asserts the same floor: the child exits inside the hard timeout, the
//! byte stream keeps the no-clear / balanced-sync / restore-on-exit contract,
//! and post-attack well-formed input is still decoded exactly.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use pi_tui::terminal::audit_bytes;
use pi_tui::terminal::guard::{EMERGENCY_REGULAR_RESTORE_BYTES, EMERGENCY_RESTORE_BYTES};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

/// Harness ceiling; comfortably above the fixture's own 20s hard timeout so a
/// wedged serve loop still resolves as fixture error-exit, never a hung test.
const HARD_TIMEOUT: Duration = Duration::from_secs(45);
const READ_IDLE: Duration = Duration::from_millis(300);
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const INPUT_READY: &[u8] = b"\x1b]999;PI_TUI_INPUT_READY=1\x07";
const RAW_DIAG_TAIL: usize = 4096;
/// POSIX masters hand back child bytes verbatim; `ConPTY` re-synthesizes them,
/// so byte-level audit/restore assertions apply off Windows only.
const BYTE_TRANSPARENT_MASTER: bool = cfg!(not(windows));
/// `ConPTY` re-synthesizes master writes as key records: bracketed-paste
/// markers are dropped and the payload arrives as character presses, so the
/// live Paste-event witness is POSIX-only (same convention as
/// `pty_no_flicker`).
const EXPECTED_LIVE_PASTE: u32 = if cfg!(windows) { 0 } else { 1 };

struct Harness {
    master: Box<dyn portable_pty::MasterPty>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    reader: Option<thread::JoinHandle<()>>,
    raw: Vec<u8>,
    started: Instant,
}

struct Report {
    raw: Vec<u8>,
    exit_code: Option<u32>,
    exited_cleanly: bool,
    finished_within_timeout: bool,
    clear_2j: usize,
    clear_3j: usize,
    sync_begin: usize,
    sync_end: usize,
    txn_count: Option<u32>,
    live_paste: Option<u32>,
    live_cursor: Option<u32>,
    live_resize: Option<u32>,
    live_text: Option<String>,
    cursor_restored: bool,
    modes_restored: bool,
}

impl Harness {
    fn spawn(extra_args: &[&str]) -> Self {
        let binary = fixture_binary();
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap_or_else(|err| panic!("openpty failed: {err}"));

        let mut cmd = CommandBuilder::new(&binary);
        cmd.arg("--exit=success");
        for arg in extra_args {
            cmd.arg(*arg);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("PI_TUI_AUDIT", "1");
        cmd.env_remove("PI_HARDWARE_CURSOR");

        let child = pair
            .slave
            .spawn_command(cmd)
            .unwrap_or_else(|err| panic!("spawn fixture failed: {err}"));
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .unwrap_or_else(|err| panic!("pty writer: {err}"));
        let mut reader = pair
            .master
            .try_clone_reader()
            .unwrap_or_else(|err| panic!("pty reader: {err}"));

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut harness = Self {
            master: pair.master,
            writer: Some(writer),
            child,
            rx,
            reader: Some(reader),
            raw: Vec::new(),
            started: Instant::now(),
        };
        // Answer the startup probe batch: kitty query, DA, cell-size, OSC 11, CPR.
        harness.send(b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R");
        harness
    }

    /// Drain the reader channel into `raw` for `dur`.
    fn pump(&mut self, dur: Duration) {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            let mut progressed = false;
            while let Ok(chunk) = self.rx.try_recv() {
                self.raw.extend_from_slice(&chunk);
                progressed = true;
            }
            if !progressed {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn drain_available(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.raw.extend_from_slice(&chunk);
        }
    }

    fn wait_input_ready(&mut self) {
        while self.started.elapsed() < HARD_TIMEOUT {
            self.drain_available();
            if find_subslice(&self.raw, INPUT_READY).is_some() {
                return;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            find_subslice(&self.raw, INPUT_READY).is_some(),
            "fixture never published input readiness; raw_len={} head={:?}",
            self.raw.len(),
            String::from_utf8_lossy(&self.raw[..self.raw.len().min(200)])
        );
    }

    fn send(&mut self, bytes: &[u8]) {
        let writer = self
            .writer
            .as_mut()
            .unwrap_or_else(|| panic!("send called after writer dropped"));
        writer
            .write_all(bytes)
            .unwrap_or_else(|err| panic!("pty write failed: {err}"));
        writer
            .flush()
            .unwrap_or_else(|err| panic!("pty flush failed: {err}"));
    }

    /// Write without asserting delivery: after a contract violation the child
    /// may already have exited, so the closing fence write is cleanup only —
    /// EIO there is expected, not a harness failure.
    fn send_lossy(&mut self, bytes: &[u8]) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Chunked write for large hostile payloads so the PTY master buffer is
    /// drained incrementally instead of assuming one giant nonblocking write.
    fn send_chunked(&mut self, bytes: &[u8], chunk: usize) {
        for part in bytes.chunks(chunk.max(1)) {
            self.send(part);
            self.pump(Duration::from_millis(10));
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        // A degenerate geometry may be rejected by the host PTY layer itself;
        // that is harness noise, not fixture behaviour, so tolerate it.
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn send_ctrl_d(&mut self) {
        self.send(b"\x04");
    }

    /// The portable-pty master-EOF stand-in: dropping the writer emits
    /// newline+VEOT toward the slave, which reaches raw mode as Ctrl+D.
    fn close_input(&mut self) {
        drop(self.writer.take());
    }

    fn mark(&self) -> usize {
        self.raw.len()
    }

    fn raw_since(&self, mark: usize) -> &[u8] {
        &self.raw[mark.min(self.raw.len())..]
    }

    fn finish(mut self) -> Report {
        let mut exited = false;
        while self.started.elapsed() < HARD_TIMEOUT {
            self.drain_available();
            if self.child.try_wait().ok().flatten().is_some() {
                exited = true;
                let drain_until = Instant::now() + READ_IDLE;
                while Instant::now() < drain_until {
                    self.drain_available();
                    thread::sleep(Duration::from_millis(10));
                }
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }

        let mut exit_code = None;
        if exited {
            exit_code = self
                .child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.exit_code());
        } else {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        drop(self.writer.take());
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.drain_available();

        let finished_within_timeout = exited && self.started.elapsed() <= HARD_TIMEOUT;
        let audit = audit_bytes(&self.raw);
        let raw = &self.raw;
        Report {
            clear_2j: audit.clear_2j,
            clear_3j: audit.clear_3j,
            sync_begin: audit.sync_begin,
            sync_end: audit.sync_end,
            txn_count: parse_sidechannel_u32(raw, b"PI_TUI_TXN_COUNT="),
            live_paste: parse_sidechannel_u32(raw, b"PI_TUI_LIVE_PASTE="),
            live_cursor: parse_sidechannel_u32(raw, b"PI_TUI_LIVE_CURSOR="),
            live_resize: parse_sidechannel_u32(raw, b"PI_TUI_LIVE_RESIZE="),
            live_text: parse_sidechannel_text(raw, b"PI_TUI_LIVE_TEXT="),
            // Activate unconditionally hides the cursor (RestoreStep::
            // CursorHidden) so every restore emits `?25h` — the emergency
            // constants embed it too, so it alone is the cursor witness.
            // `?2004l` proves the mode-disable step landed but can never
            // substitute for cursor-show.
            cursor_restored: find_subslice(raw, b"\x1b[?25h").is_some(),
            modes_restored: find_subslice(raw, b"\x1b[?2004l").is_some()
                || raw
                    .windows(EMERGENCY_REGULAR_RESTORE_BYTES.len())
                    .any(|window| window == EMERGENCY_REGULAR_RESTORE_BYTES)
                || raw
                    .windows(EMERGENCY_RESTORE_BYTES.len())
                    .any(|window| window == EMERGENCY_RESTORE_BYTES),
            exit_code,
            exited_cleanly: exited && exit_code == Some(0),
            finished_within_timeout,
            raw: self.raw,
        }
    }
}

impl Report {
    /// Diagnostic tail used in every assertion message.
    fn tail(&self) -> String {
        String::from_utf8_lossy(&self.raw[self.raw.len().saturating_sub(RAW_DIAG_TAIL)..])
            .escape_default()
            .collect()
    }

    fn assert_wire_contract(&self, what: &str) {
        assert!(
            self.finished_within_timeout,
            "{what}: fixture did not exit within the hard timeout; tail={}",
            self.tail()
        );
        if BYTE_TRANSPARENT_MASTER {
            assert_eq!(self.clear_2j, 0, "{what}: CSI 2J must never appear");
            assert_eq!(self.clear_3j, 0, "{what}: CSI 3J must never appear");
            // Balanced sync holds on every exit path, not just success: an
            // error unwind between the 2026h open and its close leaves the
            // terminal swallowing everything that follows.
            assert_eq!(
                self.sync_begin, self.sync_end,
                "{what}: synchronized output markers must balance ({}h vs {}l)",
                self.sync_begin, self.sync_end
            );
            assert!(
                self.cursor_restored,
                "{what}: no cursor-show (\\x1b[?25h) bytes on exit; tail={}",
                self.tail()
            );
            assert!(
                self.modes_restored,
                "{what}: no mode-restore (\\x1b[?2004l or emergency) bytes on exit; tail={}",
                self.tail()
            );
        }
    }

    /// Success-path addition: exit code 0.
    fn assert_success_contract(&self, what: &str) {
        self.assert_wire_contract(what);
        assert!(
            self.exited_cleanly,
            "{what}: expected exit code 0, got {:?}; tail={}",
            self.exit_code,
            self.tail()
        );
    }
}

/// After `INPUT_READY`, flood the input with malformed and half-finished escape
/// framing, then prove the parser still delivers a well-formed paste exactly
/// and Ctrl+D still terminates the serve loop.
#[test]
fn adversarial_escape_soup_then_valid_paste() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    // Lone/half CSI, dangling private-mode opener, terminated stray OSC,
    // a DSR request as input, NUL/DEL, double-ESC, and a bogus CPR record.
    // None contain a bracketed-paste delimiter, so none can wedge the paste
    // accumulator; the contract under test is recovery, not classification.
    h.send(
        b"\x1b\x1b[\x1b[?\x1b[?25\x1b]8;;https://bogus.invalid\x07\x1b[6n\x00\x7f\x1b\x1b\x1b[999;1R",
    );
    h.pump(Duration::from_millis(150));

    h.send(b"\x1b[200~POST-SOUP-PAYLOAD\x1b[201~");
    h.pump(Duration::from_millis(100));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("escape-soup");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "escape-soup: expected {EXPECTED_LIVE_PASTE} live paste(s) after malformed input, got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    let live_text = report
        .live_text
        .unwrap_or_else(|| panic!("escape-soup: missing PI_TUI_LIVE_TEXT record"));
    assert!(
        live_text.contains("POST-SOUP-PAYLOAD"),
        "escape-soup: pasted payload did not reach the fixture intact, got {live_text:?}"
    );
}

/// Inject the exact byte shapes of terminal probe replies mid-serve — the
/// sequence the input task's protocol-error recovery path exists for — plus
/// one malformed OSC 11 reply, then require ordinary input to keep flowing.
#[test]
fn adversarial_probe_reply_flood_during_serve() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(
        b"\x1b[?1;2c\x1b[?0u\x1b]11;rgb:ffff/0000/ffff\x07\x1b]11;rgb:not-a-color\x07\x1b[12;34R\x1b[6;10;20t\x1b[4;1;1t",
    );
    h.pump(Duration::from_millis(150));

    h.send(b"\x1b[200~AFTER-REPLY-FLOOD\x1b[201~");
    h.send(b"\x1b[D\x1b[C");
    h.pump(Duration::from_millis(100));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("reply-flood");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "reply-flood: expected {EXPECTED_LIVE_PASTE} live paste(s) after injected replies, got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    assert_eq!(
        report.live_cursor,
        Some(2),
        "reply-flood: expected exactly two cursor moves after the flood, got {:?}",
        report.live_cursor
    );
    let live_text = report
        .live_text
        .unwrap_or_else(|| panic!("reply-flood: missing PI_TUI_LIVE_TEXT record"));
    assert!(
        live_text.contains("AFTER-REPLY-FLOOD"),
        "reply-flood: pasted payload corrupted or dropped, got {live_text:?}"
    );
}

/// Drive the live serve loop through degenerate geometries — 0x0, 1x1, 1xN,
/// Nx1, 2x2 — then back to a full size. The fixture clamps to 1x1 minimum;
/// the render path must not panic, wedge, or emit clears at any geometry.
#[test]
fn adversarial_degenerate_resize_geometry() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for (cols, rows) in [(0u16, 0u16), (1, 1), (1, 24), (80, 1), (2, 2), (1, 1)] {
        h.resize(cols, rows);
        h.pump(Duration::from_millis(60));
    }
    // Drain earlier storm output, then mark immediately before the final
    // restore so only the 80x24 reanchor commit can satisfy the repaint
    // assertion — an earlier wide step must not mask a lost final resize.
    h.pump(Duration::from_millis(150));
    let mark = h.mark();
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("degenerate-resize");
    assert!(
        report.live_resize.unwrap_or(0) >= 1,
        "degenerate-resize: no post-readiness Resize event consumed; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        // The post-storm reanchor commit must repaint the viewport at 80x24:
        // after shrinking every line to a handful of columns, restoring full
        // width rewrites the fixed FOOTER/STATUS rows.
        assert!(
            find_subslice(&report.raw[mark..], b"FOOTER").is_some()
                || find_subslice(&report.raw[mark..], b"STATUS").is_some(),
            "degenerate-resize: no viewport repaint bytes after the final 80x24 restore; tail={}",
            report.tail()
        );
    }
}

/// One paste event carrying ~48KiB: a long ASCII run, NFD combining marks,
/// embedded C0 bytes, and a CRLF pair. The fixture sanitizes controls to
/// spaces and reports the post-normalization editor delta through the escaped
/// side channel, so the whole payload must round-trip without splitting the
/// event or truncating the accounting.
#[test]
fn adversarial_giant_multibyte_paste() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let mut payload = String::from("GHOST-START-");
    payload.push_str(&"x".repeat(32 * 1024));
    payload.push_str("-MID-");
    payload.push_str(&"\u{00E9}\u{0301}\u{0303}".repeat(1024));
    payload.push_str("-NFD-");
    payload.push_str("e\u{301}o\u{308}");
    payload.push('\u{1}');
    payload.push('\u{2}');
    payload.push_str("\r\n");
    payload.push_str("-GHOST-END");
    let mut wire = Vec::with_capacity(payload.len() + 8);
    wire.extend_from_slice(b"\x1b[200~");
    wire.extend_from_slice(payload.as_bytes());
    wire.extend_from_slice(b"\x1b[201~");
    h.send_chunked(&wire, 4096);
    h.pump(Duration::from_millis(150));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("giant-paste");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "giant-paste: expected {EXPECTED_LIVE_PASTE} live paste(s) for the 48KiB payload, got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    let live_text = report
        .live_text
        .unwrap_or_else(|| panic!("giant-paste: missing PI_TUI_LIVE_TEXT record"));
    assert!(
        live_text.contains("GHOST-START-"),
        "giant-paste: payload head missing, got prefix {:?}",
        &live_text[..live_text.len().min(64)]
    );
    if BYTE_TRANSPARENT_MASTER {
        // Full-delta oracle, not head/tail sampling: replay the fixture's own
        // normalization chain (paste CRLF -> LF, then control bytes -> space,
        // then escape_default) and compare the complete editor delta, so any
        // dropped chunk boundary or mangled byte in the middle fails.
        let normalized = payload.replace("\r\n", "\n").replace('\r', "\n");
        let sanitized: String = normalized
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let expected: String = sanitized.escape_default().collect();
        assert_eq!(
            live_text, expected,
            "giant-paste: editor delta diverged from normalize+sanitize+escape oracle"
        );
    } else {
        assert!(
            live_text.contains("GHOST-START-") && live_text.contains("-GHOST-END"),
            "giant-paste: payload bounds missing on ConPTY, got {live_text:?}"
        );
    }
}

/// A single write holding hundreds of printable characters interleaved with
/// complete arrow-key CSI records. Every arrow must decode as exactly one
/// cursor move and every character must land in the editor verbatim — no
/// splitting, merging, or parser desync under burst pressure.
#[test]
fn adversarial_key_burst() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let alphabet: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut burst = Vec::new();
    for i in 0..384usize {
        burst.push(alphabet[i % alphabet.len()]);
        if i % 32 == 31 {
            burst.extend_from_slice(b"\x1b[A");
        }
    }
    let expected_arrows = u32::try_from(384usize / 32).unwrap_or(u32::MAX);
    h.send(&burst);
    h.pump(Duration::from_millis(150));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("key-burst");
    assert_eq!(
        report.live_cursor,
        Some(expected_arrows),
        "key-burst: expected {expected_arrows} cursor moves, got {:?}",
        report.live_cursor
    );
    let live_text = report
        .live_text
        .unwrap_or_else(|| panic!("key-burst: missing PI_TUI_LIVE_TEXT record"));
    // Every position contributes its character; the arrow CSI at i%32==31 is
    // interleaved after it, not instead of it.
    let typed_chars: String = (0..384usize)
        .map(|i| char::from(alphabet[i % alphabet.len()]))
        .collect();
    // Exact equality, not containment: every sent char is printable ASCII so
    // the escaped editor delta must be the burst verbatim — a phantom prefix,
    // suffix, or duplicated run is as much a parser regression as a dropped
    // key.
    assert_eq!(
        live_text, typed_chars,
        "key-burst: editor delta diverged from the sent burst"
    );
    assert_eq!(
        report.live_paste,
        Some(0),
        "key-burst: no paste was sent but live_paste={:?}",
        report.live_paste
    );
}

/// A bracketed paste that never closes, then harness EOF (writer drop →
/// newline+VEOT). Whatever the parser does with the orphaned accumulator, the
/// fixture's hard timeout must bound the outcome: the child exits on its own
/// and the guard still emits restore bytes. A hang here is a real defect.
#[test]
fn adversarial_unterminated_paste_then_eof() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~ORPHAN-PASTE-DATA-never-terminated");
    h.pump(Duration::from_millis(200));
    h.close_input();
    let report = h.finish();

    report.assert_wire_contract("unterminated-paste-eof");
    assert!(
        report.exit_code.is_some(),
        "unterminated-paste-eof: fixture needed a harness kill instead of self-terminating"
    );
    if BYTE_TRANSPARENT_MASTER {
        // The wedged accumulator swallows the EOF stand-in, so the bounded
        // wait must fire: exit code 2 is the io-error path. The code alone
        // cannot distinguish the deadline from an unrelated I/O failure —
        // main maps every io::Error to 2 — so pair it with the stderr
        // self-report, which names the TimedOut branch specifically.
        assert_eq!(
            report.exit_code,
            Some(2),
            "unterminated-paste-eof: expected the hard-timeout error exit (2), got {:?}",
            report.exit_code
        );
        assert!(
            find_subslice(
                &report.raw,
                b"pi_tui_pty_fixture error: hard fixture timeout"
            )
            .is_some(),
            "unterminated-paste-eof: exit 2 without the timeout self-report; tail={}",
            report.tail()
        );
    }
}

/// Resize the PTY in the middle of an open bracketed paste. The paste
/// accumulator and the SIGWINCH-driven Resize event are independent input
/// paths; both must arrive intact and in either order.
#[test]
fn adversarial_resize_mid_paste() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~FIRST-HALF-");
    h.resize(40, 12);
    h.pump(Duration::from_millis(80));
    h.send(b"SECOND-HALF\x1b[201~");
    h.pump(Duration::from_millis(80));
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.pump(Duration::from_millis(80));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("resize-mid-paste");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "resize-mid-paste: expected {EXPECTED_LIVE_PASTE} live paste(s) across the resize, got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    let live_text = report
        .live_text
        .as_ref()
        .unwrap_or_else(|| panic!("resize-mid-paste: missing PI_TUI_LIVE_TEXT record"));
    assert!(
        live_text.contains("FIRST-HALF-SECOND-HALF"),
        "resize-mid-paste: payload corrupted across the resize, got {live_text:?}"
    );
    // Two resizes spaced by full pumps must each land as their own event:
    // a lone '>=1' would also pass when only the post-paste restore resize
    // survived, so the mid-paste resize needs its own seat in the count.
    assert_eq!(
        report.live_resize,
        Some(2),
        "resize-mid-paste: expected both resizes delivered, got {:?}; tail={}",
        report.live_resize,
        report.tail()
    );
}

/// `--resize-batch` completes on the first Resize event plus a Ctrl+D fence;
/// a 20-step storm ending at 37x11 must still resolve to the kernel-reported
/// geometry and a clean success exit.
#[test]
fn adversarial_resize_batch_storm_resolves_kernel_geometry() {
    let mut h = Harness::spawn(&["--resize-batch"]);
    h.wait_input_ready();

    for (cols, rows) in [
        (100u16, 30u16),
        (64, 20),
        (40, 12),
        (24, 8),
        (16, 6),
        (12, 5),
        (10, 4),
        (18, 7),
        (28, 10),
        (48, 16),
        (72, 22),
        (96, 28),
        (60, 18),
        (44, 14),
        (33, 10),
        (25, 9),
        (50, 16),
        (41, 13),
        (39, 12),
        (37, 11),
    ] {
        h.resize(cols, rows);
        h.pump(Duration::from_millis(10));
    }
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("resize-batch-storm");
    // The batch consumes every resize notification but handle_event runs once
    // on the last one, so the live delta is exactly one regardless of storm
    // size.
    assert_eq!(
        report.live_resize,
        Some(1),
        "resize-batch-storm: batch must account exactly one Resize; got {:?}; tail={}",
        report.live_resize,
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        // The batch resolves geometry by kernel query, not by whichever
        // event the coalescing drain happened to see first — the painted
        // status line is the wire witness that the storm's LAST size won.
        assert!(
            find_subslice(&report.raw, b"batch-complete 37x11").is_some(),
            "resize-batch-storm: kernel-reported geometry diverged from the storm's final 37x11; tail={}",
            report.tail()
        );
    }
}

/// Same mode, contaminated stream: a plain key between the resize and the
/// fence violates the batch contract and must produce a prompt, bounded
/// failure — never a hang and never a silently accepted event.
#[test]
fn adversarial_resize_batch_rejects_foreign_input() {
    let mut h = Harness::spawn(&["--resize-batch"]);
    h.wait_input_ready();

    h.resize(50, 15);
    h.pump(Duration::from_millis(60));
    h.send(b"x");
    h.pump(Duration::from_millis(60));
    h.send_lossy(b"\x04");
    let report = h.finish();

    report.assert_wire_contract("resize-batch-contamination");
    assert!(
        !report.exited_cleanly,
        "resize-batch-contamination: foreign input was silently accepted (exit 0); tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        // Any nonzero exit would pass the check above — including the 20s
        // hard timeout if 'x' never arrived — so pin the rejection branch by
        // its stderr self-report and the io-error exit code.
        assert_eq!(
            report.exit_code,
            Some(2),
            "resize-batch-contamination: expected the io-error exit (2), got {:?}",
            report.exit_code
        );
        assert!(
            find_subslice(
                &report.raw,
                b"pi_tui_pty_fixture error: unexpected input in resize batch"
            )
            .is_some(),
            "resize-batch-contamination: exit 2 without the batch rejection self-report; tail={}",
            report.tail()
        );
    }
}

fn parse_sidechannel_u32(raw: &[u8], key: &[u8]) -> Option<u32> {
    let pos = find_subslice(raw, key)?;
    let start = pos + key.len();
    let mut end = start;
    while end < raw.len() && raw[end].is_ascii_digit() {
        end += 1;
    }
    if end == start || raw.get(end) != Some(&b'\x07') {
        return None;
    }
    std::str::from_utf8(&raw[start..end]).ok()?.parse().ok()
}

fn parse_sidechannel_text(raw: &[u8], key: &[u8]) -> Option<String> {
    let pos = find_subslice(raw, key)?;
    let start = pos + key.len();
    let len = raw[start..].iter().position(|byte| *byte == b'\x07')?;
    std::str::from_utf8(&raw[start..start + len])
        .ok()
        .map(str::to_owned)
}

fn find_subslice<T: PartialEq>(haystack: &[T], needle: &[T]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn fixture_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_pty_fixture") {
        return PathBuf::from(path);
    }
    let mut candidates = Vec::new();
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        candidates.push(PathBuf::from(target));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    candidates.push(PathBuf::from("target"));
    for root in candidates {
        for profile in ["debug", "release"] {
            let path = root.join(profile).join(fixture_bin_name());
            if path.exists() {
                return path;
            }
        }
    }
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "pi-tui",
            "--bin",
            "pi_tui_pty_fixture",
            "--quiet",
        ])
        .status()
        .unwrap_or_else(|err| panic!("failed to build fixture: {err}"));
    assert!(status.success(), "fixture build failed");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(fixture_bin_name());
    assert!(
        path.exists(),
        "fixture binary missing after build at {}",
        path.display()
    );
    path
}

fn fixture_bin_name() -> &'static str {
    if cfg!(windows) {
        "pi_tui_pty_fixture.exe"
    } else {
        "pi_tui_pty_fixture"
    }
}
