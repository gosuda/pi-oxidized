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
//! A second wave pushes on parser invariants harder: nested and orphaned
//! paste markers, a VEOT byte as paste payload, `u16::MAX` geometry, un-pumped
//! resize oscillation, unknown CSI forms, SGR mouse/focus floods, CONTROL-key
//! bursts, empty pastes, and a multibyte char split across writes.
//! Later waves push on the edges harder still: truncated/invalid UTF-8,
//! C0-aborted and unterminated CSI/SS3, byte-at-a-time drip feeds (keys,
//! paste, X10 mouse reports), OSC 11 framing recovery, DA2/XTVersion and
//! CPR reply floods, split paste terminators, resizes inside a partial
//! UTF-8 char, CSI parameter overflow, kitty `CSI u`, SGR colon
//! sub-parameters, focus bursts, DCS/SOS/PM/APC strings, triple-ESC
//! desync, and the no-deadline pending-CSI edge.
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
    // Each resize is spaced by a pump, so coalescing cannot merge them: every
    // one of the six degenerate steps plus the final 80x24 restore must be
    // observed — a >= 1 floor would let intermediate drops pass unnoticed.
    assert_eq!(
        report.live_resize,
        Some(7),
        "degenerate-resize: expected all 7 spaced resizes consumed, got {:?}; tail={}",
        report.live_resize,
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
        .as_deref()
        .unwrap_or_else(|| panic!("resize-mid-paste: missing PI_TUI_LIVE_TEXT record"));
    if BYTE_TRANSPARENT_MASTER {
        // No other post-readiness editor input exists, so the whole delta must
        // be exactly the split payload — containment would let a parser that
        // duplicates bytes or leaks the 200~/201~ framing pass.
        assert_eq!(
            live_text,
            "FIRST-HALF-SECOND-HALF",
            "resize-mid-paste: payload not preserved exactly across the resize; tail={}",
            report.tail()
        );
    } else {
        assert!(
            live_text.contains("FIRST-HALF-SECOND-HALF"),
            "resize-mid-paste: payload corrupted across the resize, got {live_text:?}"
        );
    }
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

/// A paste-open marker embedded *inside* an open bracketed paste is payload,
/// not a nested open: the accumulator only ends on `\x1b[201~`, so the inner
/// `200~` must survive into the editor delta (its ESC sanitized to a space)
/// while the paste still counts exactly once.
#[test]
fn adversarial_nested_paste_marker_is_literal() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~A\x1b[200~B\x1b[201~");
    h.pump(Duration::from_millis(120));
    h.send_lossy(b"\x04");
    let report = h.finish();

    report.assert_success_contract("nested-paste-marker");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "nested-paste-marker: expected {EXPECTED_LIVE_PASTE} live paste(s), got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        // The inner 200~ is literal content: sanitize_visible maps its ESC to
        // a space, so the delta must be `A [200~B` verbatim — a parser that
        // re-opened or dropped the inner marker diverges here.
        assert_eq!(
            report.live_text.as_deref(),
            Some("A [200~B"),
            "nested-paste-marker: embedded marker not literal; tail={}",
            report.tail()
        );
    }
}

/// A bare `\x1b[201~` with no open paste is an orphan terminator: it must not
/// conjure a paste event or corrupt the keys that follow it.
#[test]
fn adversarial_orphan_paste_terminator() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[201~x");
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("orphan-paste-terminator");
    assert_eq!(
        report.live_paste,
        Some(0),
        "orphan-paste-terminator: no paste was opened but live_paste={:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    // The orphan terminator produces no editor bytes; 'x' must decode cleanly.
    let live_text = report
        .live_text
        .as_deref()
        .unwrap_or_else(|| panic!("orphan-paste-terminator: missing PI_TUI_LIVE_TEXT record"));
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            live_text,
            "x",
            "orphan-paste-terminator: stray bytes leaked into the editor; tail={}",
            report.tail()
        );
    } else {
        assert!(
            live_text.contains('x'),
            "orphan-paste-terminator: 'x' lost behind the orphan; tail={}",
            report.tail()
        );
    }
}

/// `\x04` inside an open bracketed paste is literal content, never EOF: the
/// paste closes, the byte is sanitized to a space, and a real Ctrl+D still
/// ends the serve cleanly. This pins the wedge case the serve deadline fix
/// was written for.
#[test]
fn adversarial_veot_inside_paste_is_content() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~\x04\x1b[201~");
    h.pump(Duration::from_millis(120));
    h.send_lossy(b"\x04");
    let report = h.finish();

    report.assert_success_contract("veot-in-paste");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_paste,
            Some(1),
            "veot-in-paste: VEOT byte eaten instead of pasted; tail={}",
            report.tail()
        );
        // \x04 is a control char -> sanitized to a single space in the delta.
        assert_eq!(
            report.live_text.as_deref(),
            Some(" "),
            "veot-in-paste: expected sanitized ' ', got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    } else {
        assert_eq!(
            report.live_paste,
            Some(0),
            "veot-in-paste: ConPTY cannot deliver bracketed paste; tail={}",
            report.tail()
        );
    }
}

/// Geometry at the u16 ceiling: `TIOCSWINSZ` accepts 65535x65535 and the
/// fixture's fixed-height viewport must paint it without a blowup, then
/// recover a normal 80x24 frame.
#[test]
fn adversarial_extreme_geometry() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.resize(u16::MAX, u16::MAX);
    h.pump(Duration::from_millis(150));
    h.drain_available();
    let mark = h.mark();
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("extreme-geometry");
    assert_eq!(
        report.live_resize,
        Some(2),
        "extreme-geometry: expected both resizes delivered, got {:?}; tail={}",
        report.live_resize,
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        assert!(
            find_subslice(&report.raw[mark..], b"FOOTER").is_some()
                || find_subslice(&report.raw[mark..], b"STATUS").is_some(),
            "extreme-geometry: no repaint bytes after restoring 80x24; tail={}",
            report.tail()
        );
    }
}

/// Twenty back-to-back resizes with no drain between writes: the kernel may
/// collapse them and the fixture's `try_recv` coalescing may merge the rest —
/// the only contract is that at least one lands and the final 80x24 repaint
/// happens after the storm.
#[test]
fn adversarial_unpumped_resize_oscillation() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for i in 0..20 {
        if i % 2 == 0 {
            h.resize(100, 30);
        } else {
            h.resize(40, 10);
        }
    }
    h.pump(Duration::from_millis(200));
    h.drain_available();
    let mark = h.mark();
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("resize-oscillation");
    assert!(
        report.live_resize.unwrap_or(0) >= 1,
        "resize-oscillation: entire storm evaporated, no Resize handled; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        assert!(
            find_subslice(&report.raw[mark..], b"FOOTER").is_some()
                || find_subslice(&report.raw[mark..], b"STATUS").is_some(),
            "resize-oscillation: final 80x24 restore never repainted; tail={}",
            report.tail()
        );
    }
}

/// Unrecognized CSI and DECSET forms (`\x1b[999z`, `\x1b[?9999h`) plus an
/// orphan `201~` must be ignored without disturbing the keys around them.
#[test]
fn adversarial_unknown_csi_and_orphans() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[999z\x1b[?9999h\x1b[201~ok");
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("unknown-csi");
    assert_eq!(
        report.live_paste,
        Some(0),
        "unknown-csi: orphan terminator opened a paste; tail={}",
        report.tail()
    );
    let live_text = report
        .live_text
        .as_deref()
        .unwrap_or_else(|| panic!("unknown-csi: missing PI_TUI_LIVE_TEXT record"));
    if BYTE_TRANSPARENT_MASTER {
        // The malformed sequences must drop without leaving parser residue:
        // only 'o','k' reach the editor. (Before the vendored `CSI ?` final
        // fix, the whole write was swallowed — the fixture timed out.)
        assert_eq!(
            live_text,
            "ok",
            "unknown-csi: malformed-sequence residue in the editor; tail={}",
            report.tail()
        );
    } else {
        assert!(
            live_text.contains("ok"),
            "unknown-csi: trailing keys lost behind malformed sequences; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// SGR mouse press/release pairs and focus in/out events are decoded events,
/// not bytes — they must land in the Ignored register without leaking into the
/// editor or the cursor/paste counters.
#[test]
fn adversarial_mouse_focus_sgr_flood() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let mut flood = Vec::new();
    for _ in 0..5 {
        flood.extend_from_slice(b"\x1b[<0;10;5M\x1b[<0;10;5m");
    }
    for _ in 0..3 {
        flood.extend_from_slice(b"\x1b[I\x1b[O");
    }
    flood.extend_from_slice(b"z");
    h.send(&flood);
    h.pump(Duration::from_millis(150));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("mouse-focus-flood");
    assert_eq!(
        report.live_paste,
        Some(0),
        "mouse-focus-flood: pointer bytes promoted to a paste; tail={}",
        report.tail()
    );
    assert_eq!(
        report.live_cursor,
        Some(0),
        "mouse-focus-flood: pointer bytes moved the cursor; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("z"),
            "mouse-focus-flood: pointer/focus bytes leaked into the editor; tail={}",
            report.tail()
        );
    } else {
        assert!(
            report.live_text.as_deref().is_some_and(|t| t.contains('z')),
            "mouse-focus-flood: 'z' lost behind the flood; tail={}",
            report.tail()
        );
    }
}

/// CONTROL-modified keys are not printable input: a burst interleaving
/// ctrl-a/ctrl-b/ctrl-e with plain chars must deliver only the plain chars to
/// the editor, in order.
#[test]
fn adversarial_control_char_burst() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x01a\x02b\x05cd");
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("control-char-burst");
    assert_eq!(
        report.live_text.as_deref(),
        Some("abcd"),
        "control-char-burst: control keys leaked into the editor; tail={}",
        report.tail()
    );
}

/// An empty bracketed paste is a real paste: the event still counts and
/// contributes zero editor bytes; the next key decodes normally.
#[test]
fn adversarial_empty_paste() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~\x1b[201~y");
    h.pump(Duration::from_millis(120));
    h.send_lossy(b"\x04");
    let report = h.finish();

    report.assert_success_contract("empty-paste");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "empty-paste: expected {EXPECTED_LIVE_PASTE} live paste(s), got {:?}; tail={}",
        report.live_paste,
        report.tail()
    );
    let live_text = report
        .live_text
        .as_deref()
        .unwrap_or_else(|| panic!("empty-paste: missing PI_TUI_LIVE_TEXT record"));
    assert!(
        live_text.contains('y'),
        "empty-paste: 'y' lost after the empty paste; tail={}",
        report.tail()
    );
}

/// A multibyte char split across two master writes must reassemble into a
/// single decoded char — no replacement char, no drop, no split into two.
#[test]
fn adversarial_split_multibyte_char() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(&[0xC3]);
    h.pump(Duration::from_millis(100));
    h.send(&[0xA9]);
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("split-multibyte");
    if BYTE_TRANSPARENT_MASTER {
        // 'é' escaped: \u{e9}. Anything else (U+FFFD, two chars, empty delta)
        // means the parser dropped or misframed the split.
        assert_eq!(
            report.live_text.as_deref(),
            Some("\\u{e9}"),
            "split-multibyte: é did not reassemble; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// A UTF-8 lead byte plus one continuation that is then abandoned must not
/// eat the byte that proves it truncated. `Ctrl+D` after `\xe9\x81` is not a
/// continuation: without re-feeding the divergence witness the serve
/// terminator is swallowed and the fixture hangs until the hard timeout.
#[test]
fn adversarial_truncated_utf8_then_eof() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(&[0xE9, 0x81]);
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("truncated-utf8-eof");
    assert_eq!(
        report.live_paste,
        Some(0),
        "truncated-utf8-eof: truncated char opened a paste; tail={}",
        report.tail()
    );
}

/// Same truncation, but the witness byte is `ESC` opening a CSI: the
/// re-fed `ESC` must start a real escape sequence, not be swallowed with
/// the bad bytes. Pre-fix the `[999z` tail would type literal `[999z`
/// into the editor; post-fix only `ok` lands.
#[test]
fn adversarial_truncated_utf8_then_csi() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(&[0xE9, 0x81]);
    h.send(b"\x1b[999z");
    h.pump(Duration::from_millis(120));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("truncated-utf8-csi");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "truncated-utf8-csi: sequence residue or re-fed bytes in the editor; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    } else {
        let live_text = report
            .live_text
            .as_deref()
            .unwrap_or_else(|| panic!("truncated-utf8-csi: missing PI_TUI_LIVE_TEXT record"));
        assert!(
            live_text.contains("ok"),
            "truncated-utf8-csi: trailing keys lost; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// Every UTF-8 invalid class — stray continuation, overlong encoding,
/// encoded surrogate, and an out-of-range lead — must drop without
/// surfacing a char or wedging the stream.
#[test]
fn adversarial_invalid_utf8_bytes() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(&[0x80]); // stray continuation, invalid start
    h.send(&[0xC0, 0xAF]); // overlong '/'
    h.send(&[0xED, 0xA0, 0x80]); // encoded surrogate U+D800
    h.send(&[0xF8]); // 5-byte lead, invalid start
    h.pump(Duration::from_millis(120));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("invalid-utf8");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "invalid-utf8: invalid bytes surfaced as editor text; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// A CSI that never receives a final byte must not absorb the C0 bytes
/// that follow it. ECMA-48 executes the control and cancels the sequence;
/// absorbing instead means `Ctrl+D` is eaten as more parameter bytes and
/// the serve loop hangs.
#[test]
fn adversarial_unterminated_csi_then_eof() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[123456");
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("unterminated-csi-eof");
    assert_eq!(
        report.live_paste,
        Some(0),
        "unterminated-csi-eof: cancelled sequence opened a paste; tail={}",
        report.tail()
    );
}

/// The cancelling byte inside the aborted CSI is executed on its own, and
/// the bytes after it resync as ordinary input. `BEL` lands as an Ignored
/// control key and `ok` reaches the editor whole — pre-fix the `o` was
/// consumed as the CSI final.
#[test]
fn adversarial_c0_aborts_csi() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[12\x07");
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("c0-aborts-csi");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "c0-aborts-csi: abort boundary leaked or ate editor text; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// Kernel-reported zero and unit geometries: the PTY layer may reject the
/// degenerate sizes itself, but any that land must not panic the render
/// path — the fixture clamps before `note_resize` and must exit cleanly
/// once geometry recovers.
#[test]
fn adversarial_zero_and_unit_geometry() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for (cols, rows) in [
        (0u16, 0u16),
        (0, 10),
        (10, 0),
        (1, 1),
        (INITIAL_COLS, INITIAL_ROWS),
    ] {
        h.resize(cols, rows);
        h.pump(Duration::from_millis(60));
    }
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("zero-geometry");
    assert!(
        report.live_resize.unwrap_or(0) >= 1,
        "zero-geometry: no resize event survived the degenerate storm; tail={}",
        report.tail()
    );
}

/// Every byte of a paste + tail arrives as its own master write. The
/// parser must carry sequence state across read boundaries with no
/// half-parsed event leaking early.
#[test]
fn adversarial_byte_drip_feed() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for &byte in b"\x1b[200~drip-\xc3\xa9\x1b[201~ok" {
        h.send(&[byte]);
        h.pump(Duration::from_millis(5));
    }
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("byte-drip");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "byte-drip: expected {EXPECTED_LIVE_PASTE} live paste(s); tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        // 'é' escaped: \u{e9}. The drip must not split it or leak markers.
        assert_eq!(
            report.live_text.as_deref(),
            Some("drip-\\u{e9}ok"),
            "byte-drip: drip-fed payload diverged; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// An OSC 11 reply that arrives *after* the 50ms escape-framing deadline
/// is a documented leak: the held ESC already expired into an Esc key, so
/// the reply tail types literal keys. This pins the expiry boundary — a
/// slow terminal cannot smuggle an infinite candidate.
#[test]
fn adversarial_osc11_late_header_leaks_keys() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b");
    h.pump(Duration::from_millis(80));
    h.send(b"]11;rgb:1234/1234/1234\x07");
    h.pump(Duration::from_millis(80));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("osc11-late-header");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("]11;rgb:1234/1234/1234"),
            "osc11-late-header: expired-candidate reply did not leak verbatim; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// `ESC` inside a recognized OSC 11 payload not followed by `\` is
/// malformed framing: the parser latches a protocol error, and the input
/// task recovers in-band — drop the stream, clear the latch, recreate —
/// so decoding resumes with the next bytes instead of wedging or exiting.
/// The poisoned sequence's bytes are discarded with the error; the keys
/// sent afterwards must still land.
#[test]
fn adversarial_osc11_malformed_framing_recovers() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b]11;AAAA\x1bx");
    h.pump(Duration::from_millis(150));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("osc11-malformed");
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report
            .live_text
            .as_deref()
            .unwrap_or_else(|| panic!("osc11-malformed: missing PI_TUI_LIVE_TEXT record"));
        assert!(
            live_text.ends_with("ok"),
            "osc11-malformed: input after protocol recovery was lost; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// A recognized OSC 11 payload past the 64-byte bound latches the same
/// protocol error before any terminator can arrive — an unbounded reply
/// cannot grow the candidate forever — and the same in-band recovery
/// keeps the input stream live afterwards.
#[test]
fn adversarial_osc11_oversized_payload_recovers() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b]11;");
    h.send(&[b'A'; 100]);
    h.pump(Duration::from_millis(150));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("osc11-oversized");
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report
            .live_text
            .as_deref()
            .unwrap_or_else(|| panic!("osc11-oversized: missing PI_TUI_LIVE_TEXT record"));
        assert!(
            live_text.ends_with("ok"),
            "osc11-oversized: input after payload-limit recovery was lost; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// String sequences the parser does not recognize — OSC 52 clipboard and
/// APC (kitty graphics header) — diverge from the OSC 11 reply grammar and
/// decode as their ordinary-key equivalents: `Alt+]`, literal payload
/// chars, and `Alt+\\`. This pins the leak shape so a silent swallow or a
/// wedge would be caught.
#[test]
fn adversarial_unrecognized_strings_leak_keys() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b]52;c;QUJD\x07");
    h.send(b"\x1b_X\x1b\\");
    h.pump(Duration::from_millis(120));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("unrecognized-strings");
    if BYTE_TRANSPARENT_MASTER {
        // `Alt+]` and `Alt+_`/`Alt+\\` are Ignored; `BEL` is a control
        // key. Only the string bodies type text.
        assert_eq!(
            report.live_text.as_deref(),
            Some("52;c;QUJDX"),
            "unrecognized-strings: leaked keys diverged; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// Every C0 byte except Ctrl+D, sent in one burst: each must decode to a
/// control key (Ignored by the editor) or an armed-then-expired ESC —
/// none may surface as editor text or wedge the stream.
#[test]
fn adversarial_c0_control_sweep() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let burst: Vec<u8> = (0x00u8..=0x1f).filter(|b| *b != 0x04).collect();
    h.send(&burst);
    h.pump(Duration::from_millis(150));
    h.send(b"ok");
    h.pump(Duration::from_millis(80));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("c0-sweep");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "c0-sweep: control bytes leaked into the editor; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// REP, DECSCL, and clear-screen CSIs arriving as input are well-formed
/// but unrecognized replies — each drops at its final byte and the stream
/// resyncs for the trailing keys.
#[test]
fn adversarial_rep_decsc_clears_soup() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[40b\x1b[!p\x1b[2J\x1b[3J");
    h.pump(Duration::from_millis(120));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("rep-decsc-soup");
    if BYTE_TRANSPARENT_MASTER {
        // `CSI !` aborts the moment the unrecognized intermediate arrives,
        // so its `p` final resyncs as an ordinary key — the other sequences
        // reach their final byte and drop whole.
        assert_eq!(
            report.live_text.as_deref(),
            Some("pok"),
            "rep-decsc-soup: output-only CSI bytes leaked into the editor; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// Multi-vector: a paste drip-fed one byte per write while geometry
/// oscillates underneath it. The paste accumulator, the resize
/// coalescer, and the editor delta must all stay consistent.
#[test]
fn adversarial_resize_during_drip_paste() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let payload = b"\x1b[200~VECTOR-PASTE-0123456789\x1b[201~ok";
    for (i, &byte) in payload.iter().enumerate() {
        h.send(&[byte]);
        if i % 6 == 0 {
            h.resize(if i % 12 == 0 { 60 } else { INITIAL_COLS }, INITIAL_ROWS);
        }
        h.pump(Duration::from_millis(5));
    }
    h.pump(Duration::from_millis(80));
    h.send_ctrl_d();
    let report = h.finish();

    report.assert_success_contract("resize-drip-paste");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "resize-drip-paste: expected {EXPECTED_LIVE_PASTE} live paste(s); tail={}",
        report.tail()
    );
    assert!(
        report.live_resize.unwrap_or(0) >= 1,
        "resize-drip-paste: resize storm produced no live resize event; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("VECTOR-PASTE-0123456789ok"),
            "resize-drip-paste: payload diverged under geometry churn; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// X10 normal-mouse reports (`ESC [ M` + three raw report bytes) use a
/// value+32 encoding that legitimately exceeds `0x7e`: the C0-abort must
/// exempt those bytes or the report is destroyed and its tail leaks as
/// editor text. A C0 byte is never a valid report byte, so an incomplete
/// report still aborts and delivers the control key.
#[test]
fn adversarial_x10_mouse_raw_report_bytes() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    // A C0 inside an incomplete report still aborts the sequence —
    // Ctrl+C lands as a key (Ignored), it is not swallowed into the buffer.
    h.send(b"\x1b[M\x20\x03");
    h.pump(Duration::from_millis(60));
    // Complete report: Cb=0x20, Cx=0xc0, Cy=0x61 — the last two must be
    // report bytes, not editor text (0x61 would land as 'a' if shredded).
    h.send(b"\x1b[M\x20\xc0a");
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "x10-mouse: report bytes leaked as text; tail={}",
            report.tail()
        );
    }
    report.assert_success_contract("x10-mouse");
}

// ---- wave 4: probe replies, drip splits, and degenerate framing ----

/// `CSI > Pp;Pv;Pc c` (DA2/XTVersion) replies decode since the vendored
/// secondary-attributes patch — a well-formed reply mid-serve is consumed,
/// an unknown `>`-final drops without latching, and a C0 still aborts the
/// pending `CSI >`.
#[test]
fn adversarial_da2_xtversion_final_sweep() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[>2;10;1c"); // well-formed XTVersion reply
    h.pump(Duration::from_millis(60));
    h.send(b"\x1b[>c"); // empty-params final
    h.pump(Duration::from_millis(60));
    h.send(b"\x1b[>99x"); // unknown '>'-final — drops whole sequence
    h.pump(Duration::from_millis(60));
    h.send(b"\x1b[>5\x03"); // C0 aborts a pending CSI >
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("da2-xtversion");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "da2-xtversion: reply params leaked as keys; tail={}",
            report.tail()
        );
    }
}

/// An X10 mouse report delivered one byte at a time — including the raw
/// `>0x7e` report bytes the abort guard must not shred mid-accumulation.
#[test]
fn adversarial_x10_report_drip_feed() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for &byte in b"\x1b[M\xff\xff\xff" {
        h.send(&[byte]);
        h.pump(Duration::from_millis(30));
    }
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("x10-drip");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "x10-drip: report bytes leaked as text; tail={}",
            report.tail()
        );
    }
}

/// The paste terminator `\x1b[201~` split mid-sequence across writes —
/// `ESC [ 2 0 1` buffered, `~` arriving in the next read must still close
/// the paste exactly once.
#[test]
fn adversarial_paste_terminator_split() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[200~SPLIT-BODY\x1b[201");
    h.pump(Duration::from_millis(60));
    h.send(b"~");
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("terminator-split");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "terminator-split: split terminator broke the paste; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report.live_text.as_deref().unwrap_or_default();
        assert!(
            live_text.contains("SPLIT-BODY") && live_text.ends_with("ok"),
            "terminator-split: payload/text diverged; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// A SIGWINCH between a UTF-8 lead byte and its continuation: the
/// incomplete char must not be dropped or merged with the resize event.
#[test]
fn adversarial_resize_inside_utf8_char() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(&[0xc3]); // é lead byte
    h.resize(97, 31);
    h.pump(Duration::from_millis(60));
    h.send(&[0xa9]); // continuation
    h.pump(Duration::from_millis(60));
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("resize-inside-utf8");
    assert!(
        report.live_resize.is_some_and(|n| n >= 1),
        "resize-inside-utf8: resize lost behind the partial char; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("\\u{e9}ok"),
            "resize-inside-utf8: char corrupted by resize; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// Parameter fields far past integer range — `~` finals that overflow
/// any accumulation must drop or clamp, never panic or latch.
#[test]
fn adversarial_csi_param_overflow() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for seq in [
        b"\x1b[999999999999999999999999999999~".as_slice(),
        b"\x1b[18446744073709551616;1~".as_slice(),
        b"\x1b[-1~-0~+1~".as_slice(),
        b"\x1b[;;;;;;~".as_slice(),
    ] {
        h.send(seq);
        h.pump(Duration::from_millis(40));
    }
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("csi-param-overflow");
    if BYTE_TRANSPARENT_MASTER {
        // `+`/`-` are CSI intermediate bytes (0x20..=0x2f): `\x1b[-` aborts
        // at len 3 and its tail leaks as ordinary keys, and the all-`;`
        // sequence leaks on its abort too. The huge-`~`-param sequences
        // drop cleanly. Pin the exact leak so drift is loud.
        assert_eq!(
            report.live_text.as_deref(),
            Some("1~-0~+1~;;;;;~ok"),
            "csi-param-overflow: leak shape diverged; got {:?}; tail={}",
            report.live_text,
            report.tail()
        );
    }
}

/// Kitty keyboard `CSI u` encodings — Unicode codepoint, modifier, and
/// event-type subfields the fixture doesn't implement must resolve or
/// drop, never latch or leak.
#[test]
fn adversarial_kitty_csi_u() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for seq in [
        b"\x1b[57358u".as_slice(),         // caps-lock codepoint
        b"\x1b[13;5u".as_slice(),          // ctrl+enter
        b"\x1b[97;1:3u".as_slice(),        // 'a' release event
        b"\x1b[57441;5;57399u".as_slice(), // shifted f-key w/ text
    ] {
        h.send(seq);
        h.pump(Duration::from_millis(40));
    }
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("kitty-csi-u");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "kitty-csi-u: params leaked as keys; tail={}",
            report.tail()
        );
    }
}

/// A flood of cursor-position replies (`ESC [ r ; c R`) — the shape a
/// CPR-querying terminal or a pasted transcript might inject mid-serve.
#[test]
fn adversarial_cpr_reply_flood() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for row in 1..=64u32 {
        h.send(format!("\x1b[{row};{row}R").as_bytes());
    }
    h.pump(Duration::from_millis(120));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("cpr-flood");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "cpr-flood: report params leaked as keys; tail={}",
            report.tail()
        );
    }
}

/// Focus-gained/lost bursts — `ESC [ I` / `ESC [ O` alternating: every
/// one decodes as a `FocusEvent` the fixture ignores, and the trailing
/// text still lands.
#[test]
fn adversarial_focus_event_burst() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    let mut flood = Vec::new();
    for _ in 0..32 {
        flood.extend_from_slice(b"\x1b[I\x1b[O");
    }
    h.send(&flood);
    h.pump(Duration::from_millis(120));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("focus-burst");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "focus-burst: focus bytes leaked as keys; tail={}",
            report.tail()
        );
    }
}

/// Mouse reports with out-of-range coordinates: SGR params past `u16`,
/// SGR coordinate 0, and an X10 report byte of 0x20 — the coordinate-0
/// values used to `u16`-underflow inside the vendored parsers, panicking
/// the input task (the panic hook's emergency restore is what the extra
/// `?2026l` on the wire was). All must clamp or drop, never panic.
#[test]
fn adversarial_mouse_coord_overflow() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[<0;99999;99999M");
    h.pump(Duration::from_millis(40));
    h.send(b"\x1b[<65;0;0m");
    h.pump(Duration::from_millis(40));
    h.send(b"\x1b[M\x20\x20\x20");
    h.pump(Duration::from_millis(40));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("mouse-coord-overflow");
    if BYTE_TRANSPARENT_MASTER {
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "mouse-coord-overflow: coords leaked as keys; tail={}",
            report.tail()
        );
    }
}

/// A pending `ESC [` has NO completion deadline in the vendored parser:
/// the final byte resolves the CSI whenever it arrives. Pin it — a lone
/// `ESC [` followed 300ms later by `A` is one arrow-up, never a literal
/// 'a' (and a C0 aborts the wait, per the wave-3 fix).
#[test]
fn adversarial_pending_csi_no_deadline() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[");
    h.pump(Duration::from_millis(300));
    h.send(b"A"); // completes to arrow-up despite the gap
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("pending-csi-no-deadline");
    if BYTE_TRANSPARENT_MASTER {
        assert!(
            report.live_cursor.is_some_and(|n| n >= 1),
            "pending-csi-no-deadline: late final did not complete the CSI; tail={}",
            report.tail()
        );
        assert_eq!(
            report.live_text.as_deref(),
            Some("ok"),
            "pending-csi-no-deadline: 'A' leaked as text; tail={}",
            report.tail()
        );
    }
}

/// All three input vectors in one unpumped flight: text, paste-open,
/// resize, paste payload, paste-close, text — ordering across paths.
#[test]
fn adversarial_interleaved_trinary_flight() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"A\x1b[200~FLIGHT-");
    h.resize(120, 40);
    h.send(b"BODY\x1b[201~B");
    h.pump(Duration::from_millis(120));
    h.resize(INITIAL_COLS, INITIAL_ROWS);
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("trinary-flight");
    assert_eq!(
        report.live_paste,
        Some(EXPECTED_LIVE_PASTE),
        "trinary-flight: paste lost across the resize; tail={}",
        report.tail()
    );
    assert!(
        report.live_resize.is_some_and(|n| n >= 1),
        "trinary-flight: resize lost mid-paste; tail={}",
        report.tail()
    );
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report.live_text.as_deref().unwrap_or_default();
        assert!(
            live_text.contains("FLIGHT-BODY") && live_text.ends_with("Bok"),
            "trinary-flight: text/paste diverged; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// Raw DCS / SOS / PM string bodies — unrecognized string framing leaks
/// payload bytes as keys. Pin the suffix so drift is loud, and require
/// the serve loop to still decode the trailing keys.
#[test]
fn adversarial_dcs_sos_pm_strings() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    for seq in [
        b"\x1bP>|payload\x1b\\".as_slice(),  // DCS XTVersion-shaped body
        b"\x1bXsoc-string\x1b\\".as_slice(), // SOS
        b"\x1b^pm-string\x1b\\".as_slice(),  // PM
        b"\x1b_apc-string\x1b\\".as_slice(), // APC
    ] {
        h.send(seq);
        h.pump(Duration::from_millis(40));
    }
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("dcs-sos-pm");
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report.live_text.as_deref().unwrap_or_default();
        assert!(
            live_text.ends_with("ok"),
            "dcs-sos-pm: trailing keys lost; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// SGR colon sub-parameter forms (`4:2`, `38:2:r:g:b`) — colon bytes are
/// not CSI-legal params in the vendored grammar; pin that the sequence
/// drops or resolves without leaking digits as keys.
#[test]
fn adversarial_sgr_colon_subparams() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b[4:2m\x1b[38:2:255:0:0m\x1b[1;31m");
    h.pump(Duration::from_millis(60));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("sgr-colon");
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report.live_text.as_deref().unwrap_or_default();
        assert!(
            live_text.ends_with("ok"),
            "sgr-colon: trailing keys lost; got {live_text:?}; tail={}",
            report.tail()
        );
    }
}

/// `\x1b\x1b\x1bA` — triple-ESC desync: each ESC resolves/aborts in turn
/// and 'A' lands as an Alt-modified key or a plain char; pin the real
/// behavior so a framing drift is loud.
#[test]
fn adversarial_triple_esc_desync() {
    let mut h = Harness::spawn(&["--serve"]);
    h.wait_input_ready();

    h.send(b"\x1b\x1b\x1bA");
    h.pump(Duration::from_millis(200));
    h.send(b"ok");
    h.pump(Duration::from_millis(60));
    h.send_ctrl_d();

    let report = h.finish();
    report.assert_success_contract("triple-esc");
    if BYTE_TRANSPARENT_MASTER {
        let live_text = report.live_text.as_deref().unwrap_or_default();
        assert!(
            live_text.ends_with("ok"),
            "triple-esc: trailing keys lost; got {live_text:?}; tail={}",
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
