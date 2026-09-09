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
//! Verification check 6: mandatory no-flicker PTY tests.
//!
//! Spawns the release-style `pi_tui_pty_fixture` under `portable-pty`, drives
//! aggressive resizes / paste / cursor input, parses the byte stream with
//! `avt`, and asserts the no-clear / single-write / probe-before-sync contract.
//!
//! Platform key-matrix coverage documents the intentional legacy
//! `modifyOtherKeys` omission (see test name and
//! [`pi_tui::keys::MODIFY_OTHER_KEYS_OMISSION`]).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use avt::Vt;
use crossterm::event::{KeyCode, KeyEventState, KeyModifiers};
use pi_tui::keys::{
    KeyId, MODIFY_OTHER_KEYS_OMISSION, is_kitty_protocol_active, key_matches, key_press,
    key_press_state, set_kitty_protocol_active,
};
use pi_tui::terminal::guard::{EMERGENCY_REGULAR_RESTORE_BYTES, EMERGENCY_RESTORE_BYTES};
use pi_tui::terminal::{audit_bytes, probe_query_batch};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const HARD_TIMEOUT: Duration = Duration::from_secs(30);
const READ_IDLE: Duration = Duration::from_millis(300);
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
/// Whether the PTY master hands back the child's bytes verbatim. `ConPTY` is a
/// renderer: it consumes the child's control sequences and re-synthesizes its
/// own, so probe/sync/restore byte provenance is only observable on POSIX
/// masters (see docs/REL-R3-conpty-witness-prototype.md §3.4/§3.6). Decoded
/// frames and fixture side-channel counters remain the cross-platform evidence.
const BYTE_TRANSPARENT_MASTER: bool = cfg!(not(windows));
/// Longest raw-transcript tail included in assertion diagnostics.
const RAW_DIAG_TAIL: usize = 4096;

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    exit: &'static str,
    sync: bool,
}

#[test]
fn pty_no_flicker_sync_supported_branch() {
    run_scenario(Scenario {
        name: "sync",
        exit: "success",
        sync: true,
    });
}

#[test]
fn pty_no_flicker_sync_ignored_branch_single_write_no_clear() {
    run_scenario(Scenario {
        name: "nosync",
        exit: "success",
        sync: false,
    });
}

#[test]
fn pty_cursor_restore_after_success_abort_provider_error_panic_and_sigint() {
    for exit in ["success", "abort", "provider-error", "panic", "sigint"] {
        let report = drive_fixture(exit, true, false);
        assert!(
            report.finished_within_timeout,
            "exit={exit}: fixture must terminate within the hard timeout"
        );
        if !BYTE_TRANSPARENT_MASTER {
            continue;
        }
        if exit == "panic" || exit == "sigint" {
            assert_eq!(
                report.emergency_regular_restore_count,
                1,
                "exit={exit}: expected exactly one regular-mode emergency restore sequence; got {} regular / {} alternate-screen in {} output bytes",
                report.emergency_regular_restore_count,
                report.emergency_alternate_screen_restore_count,
                report.raw.len()
            );
            assert_eq!(
                report.emergency_alternate_screen_restore_count,
                0,
                "exit={exit}: alternate-screen emergency restore emitted although the fixture never entered the alternate screen"
            );
        } else {
            assert!(
                report.saw_cursor_show || report.emergency_regular_restore_count > 0,
                "exit={exit}: expected cursor restoration bytes; got {} output bytes",
                report.raw.len()
            );
        }
        let audit = audit_bytes(&report.raw);
        assert_eq!(audit.clear_2j, 0, "exit={exit}: CSI 2J must never appear");
        assert_eq!(audit.clear_3j, 0, "exit={exit}: CSI 3J must never appear");
        assert_eq!(
            audit.sync_begin, audit.sync_end,
            "exit={exit}: synchronized output markers must balance"
        );
    }
}

#[test]
fn pty_final_snapshots_narrow_normal_wide() {
    let report = drive_fixture("success", true, true);
    assert!(
        report.snapshots.iter().any(|(w, _)| *w <= 20),
        "missing narrow snapshot"
    );
    assert!(
        report.snapshots.iter().any(|(w, _)| (60..=100).contains(w)),
        "missing normal snapshot"
    );
    assert!(
        report.snapshots.iter().any(|(w, _)| *w >= 120),
        "missing wide snapshot"
    );
    for (width, text) in &report.snapshots {
        let joined = text.join("\n");
        assert!(
            joined.contains("STATUS") || joined.contains("FOOTER") || joined.contains("STREAM"),
            "width={width}: expected continuous fixture content, got {joined:?}"
        );
        let non_empty = text.iter().filter(|line| !line.trim().is_empty()).count();
        assert!(
            non_empty > 0,
            "width={width}: blank frame detected in snapshot"
        );
    }
}

/// Key matrix is OS-aware. On every host we assert structured Kitty/crossterm
/// matching works and document that legacy `modifyOtherKeys` is intentionally
/// omitted so modified-Enter cannot be distinguished without Kitty.
#[test]
fn key_matrix_linux_macos_windows_legacy_modifyotherkeys_omission() {
    let host = std::env::consts::OS;
    assert!(
        matches!(host, "linux" | "macos" | "windows")
            || cfg!(target_os = "linux")
            || cfg!(target_os = "macos")
            || cfg!(target_os = "windows"),
        "unexpected host OS for key matrix: {host}"
    );

    let cases: &[(&str, crossterm::event::KeyEvent, bool)] = &[
        (
            "ctrl+c",
            key_press(KeyCode::Char('c'), KeyModifiers::CONTROL),
            true,
        ),
        (
            "enter",
            key_press(KeyCode::Enter, KeyModifiers::empty()),
            true,
        ),
        (
            "shift+enter",
            key_press(KeyCode::Enter, KeyModifiers::SHIFT),
            true,
        ),
        (
            "alt+enter",
            key_press(KeyCode::Enter, KeyModifiers::ALT),
            true,
        ),
        (
            "ctrl+enter",
            key_press(KeyCode::Enter, KeyModifiers::CONTROL),
            true,
        ),
        (
            "left",
            key_press(KeyCode::Left, KeyModifiers::empty()),
            true,
        ),
        (
            "ctrl+right",
            key_press(KeyCode::Right, KeyModifiers::CONTROL),
            true,
        ),
        (
            "1",
            key_press_state(
                KeyCode::Char('1'),
                KeyModifiers::empty(),
                KeyEventState::KEYPAD,
            ),
            true,
        ),
    ];

    for (id, event, expected) in cases {
        assert_eq!(
            key_matches(event, &KeyId::from(*id)),
            *expected,
            "os={host} key_id={id}"
        );
    }

    set_kitty_protocol_active(false);
    assert!(!is_kitty_protocol_active());
    let plain = key_press(KeyCode::Enter, KeyModifiers::empty());
    assert!(key_matches(&plain, &KeyId::from("enter")));
    assert!(
        !key_matches(&plain, &KeyId::from("shift+enter")),
        "legacy plain Enter must not satisfy shift+enter without Kitty/modifyOtherKeys"
    );
    assert!(
        MODIFY_OTHER_KEYS_OMISSION.contains("modifyOtherKeys"),
        "omission marker must name modifyOtherKeys"
    );
    assert!(
        MODIFY_OTHER_KEYS_OMISSION.contains("never emitted or parsed"),
        "omission marker must state never emitted/parsed"
    );
    assert!(
        MODIFY_OTHER_KEYS_OMISSION.contains("backslash-Enter"),
        "omission marker must document backslash-Enter workaround"
    );

    match host {
        "linux" => assert!(
            MODIFY_OTHER_KEYS_OMISSION.contains("Legacy non-Kitty"),
            "linux key-matrix omission docs"
        ),
        "macos" => assert!(
            MODIFY_OTHER_KEYS_OMISSION.contains("Legacy non-Kitty"),
            "macos key-matrix omission docs"
        ),
        "windows" => assert!(
            MODIFY_OTHER_KEYS_OMISSION.contains("Legacy non-Kitty"),
            "windows key-matrix omission docs (console modifiers via crossterm, no modifyOtherKeys)"
        ),
        _ => {}
    }
}

#[allow(clippy::too_many_lines)]
fn run_scenario(scenario: Scenario) {
    let report = drive_fixture(scenario.exit, scenario.sync, true);

    assert!(
        report.resize_count >= 20,
        "{}: expected >=20 resizes, got {}",
        scenario.name,
        report.resize_count
    );
    assert!(
        report.paste_count > 0,
        "{}: fixture must observe paste (paste_count={})",
        scenario.name,
        report.paste_count
    );
    assert!(
        report.cursor_moves > 0,
        "{}: fixture must observe cursor movement (cursor_moves={})",
        scenario.name,
        report.cursor_moves
    );
    assert!(
        report.saw_plugin_frame,
        "{}: plugin frames must appear",
        scenario.name
    );
    assert!(
        report.saw_stream_and_tools,
        "{}: long stream + tool updates required",
        scenario.name
    );
    assert!(
        report.continuous_content,
        "{}: content must remain continuous across resizes",
        scenario.name
    );
    assert!(
        report.finished_within_timeout,
        "{}: hard draw/run timeout exceeded",
        scenario.name
    );
    let text = report.final_vt_text.join("\n");
    assert!(
        text.contains("STATUS") || text.contains("FOOTER") || text.contains("DONE"),
        "{}: avt final view missing fixture content: {text:?}",
        scenario.name
    );

    if !BYTE_TRANSPARENT_MASTER {
        return;
    }

    let audit = audit_bytes(&report.raw);

    assert_eq!(audit.clear_2j, 0, "{}: CSI 2J forbidden", scenario.name);
    assert_eq!(audit.clear_3j, 0, "{}: CSI 3J forbidden", scenario.name);
    assert_eq!(
        audit.sync_begin, audit.sync_end,
        "{}: balanced CSI ? 2026 h/l required",
        scenario.name
    );

    if scenario.sync {
        assert!(
            audit.sync_begin > 0,
            "{}: expected synchronized output markers",
            scenario.name
        );
    } else {
        assert_eq!(
            audit.sync_begin, 0,
            "{}: sync-ignored branch must omit 2026 wrappers",
            scenario.name
        );
        assert_eq!(
            audit.sync_end, 0,
            "{}: no 2026 end without begin",
            scenario.name
        );
    }

    let probe = probe_query_batch(true);
    let probe_pos =
        find_subslice(&report.raw, &probe).expect("probe query batch must be present on the wire");
    if scenario.sync {
        let first_sync =
            find_subslice(&report.raw, b"\x1b[?2026h").expect("sync branch must emit CSI ? 2026 h");
        assert!(
            probe_pos < first_sync,
            "{}: probes must precede synchronized output (probe={probe_pos}, sync={first_sync})",
            scenario.name
        );
    } else {
        // Probes still precede any stage-3 transaction markers.
        let first_txn = find_subslice(&report.raw, b"PI_TUI_TXN_BEGIN=")
            .expect("nosync branch still emits transaction markers");
        assert!(
            probe_pos < first_txn,
            "{}: probes must precede stage-3 transactions",
            scenario.name
        );
    }

    assert!(
        report.settle_same_write,
        "{}: settle insert_before + redraw must share one write",
        scenario.name
    );
    assert!(
        report.row_erase_immediate_reflow,
        "{}: row-local erase must be followed immediately by reflowed content",
        scenario.name
    );
    assert!(
        report.no_blank_frame,
        "{}: intermediate blank frames are forbidden",
        scenario.name
    );
    assert!(
        report.sole_stdout_owner,
        "{}: fixture must own stdout exclusively after probes",
        scenario.name
    );
    assert!(
        report.txn_count > 0,
        "{}: expected instrumented stage-3 transactions",
        scenario.name
    );
}

struct DriveReport {
    raw: Vec<u8>,
    snapshots: Vec<(u16, Vec<String>)>,
    final_vt_text: Vec<String>,
    resize_count: u32,
    paste_count: u32,
    cursor_moves: u32,
    txn_count: u32,
    settle_same_write: bool,
    row_erase_immediate_reflow: bool,
    continuous_content: bool,
    no_blank_frame: bool,
    saw_plugin_frame: bool,
    saw_stream_and_tools: bool,
    finished_within_timeout: bool,
    sole_stdout_owner: bool,
    saw_cursor_show: bool,
    emergency_regular_restore_count: usize,
    emergency_alternate_screen_restore_count: usize,
}

#[allow(clippy::too_many_lines)]
fn drive_fixture(exit: &str, sync: bool, capture_width_snapshots: bool) -> DriveReport {
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
    cmd.arg(format!("--exit={exit}"));
    cmd.arg("--serve");
    if !sync {
        cmd.arg("--no-sync");
        cmd.env("PI_TUI_NO_SYNC", "1");
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("PI_TUI_AUDIT", "1");
    cmd.env_remove("PI_HARDWARE_CURSOR");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .unwrap_or_else(|err| panic!("spawn fixture failed: {err}"));
    drop(pair.slave);

    let mut writer = pair
        .master
        .take_writer()
        .unwrap_or_else(|err| panic!("pty writer: {err}"));
    let mut reader = pair
        .master
        .try_clone_reader()
        .unwrap_or_else(|err| panic!("pty reader: {err}"));

    // Prevent the master from echoing harness-injected probe replies into the
    // child's output stream (those would corrupt avt snapshots and audits).
    disable_pty_echo(pair.master.as_ref());

    writer
        .write_all(b"\x1b[?0u\x1b[?1;2c\x1b[6;10;20t\x1b]11;rgb:0000/0000/0000\x07\x1b[1;1R")
        .unwrap_or_else(|err| panic!("probe reply write failed: {err}"));
    writer
        .flush()
        .unwrap_or_else(|err| panic!("probe reply flush failed: {err}"));

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
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

    let started = Instant::now();
    let mut raw = Vec::new();
    let mut vt = Vt::builder()
        .size(usize::from(INITIAL_COLS), usize::from(INITIAL_ROWS))
        .scrollback_limit(10_000)
        .build();
    let mut snapshots = Vec::new();
    let mut resize_count = 0u32;
    let mut last_data = Instant::now();
    let mut continuous_content = true;
    let mut saw_stream_and_tools = false;
    let mut saw_plugin_frame = false;
    let mut painted = false;

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

    // Wait until the fixture has painted real content (not just probes).
    while started.elapsed() < HARD_TIMEOUT && !painted {
        while let Ok(chunk) = rx.try_recv() {
            raw.extend_from_slice(&chunk);
            feed_vt(&mut vt, &chunk);
            last_data = Instant::now();
        }
        let joined = vt_text(&vt).join("\n");
        if joined.contains("STATUS") || find_subslice(&raw, b"STATUS").is_some() {
            painted = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        painted || find_subslice(&raw, b"STATUS").is_some(),
        "fixture never painted STATUS content within timeout; raw_len={} head={:?}",
        raw.len(),
        String::from_utf8_lossy(&raw[..raw.len().min(200)])
    );

    // Input rendezvous: the fixture publishes a complete OSC-999 readiness
    // record only after its scripted prelude and the live-counter baseline.
    // Drain raw output through the same reader/VT path; child exit or hard
    // timeout before readiness is a failure, not a skip.
    let mut ready = false;
    while started.elapsed() < HARD_TIMEOUT {
        while let Ok(chunk) = rx.try_recv() {
            raw.extend_from_slice(&chunk);
            feed_vt(&mut vt, &chunk);
            last_data = Instant::now();
        }
        if find_subslice(&raw, b"\x1b]999;PI_TUI_INPUT_READY=1\x07").is_some() {
            ready = true;
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ready,
        "fixture never published input readiness; raw_len={} head={:?}",
        raw.len(),
        String::from_utf8_lossy(&raw[..raw.len().min(200)])
    );

    // Exactly one paste and six cursor moves, delivered once, after readiness.
    write_stimulus(
        &mut writer,
        child.as_mut(),
        b"\x1b[200~PASTED-BLOCK-line1\nline2\x1b[201~",
        "paste",
    );
    write_stimulus(
        &mut writer,
        child.as_mut(),
        b"\x1b[D\x1b[C\x1b[A\x1b[B\x1b[H\x1b[F",
        "cursor",
    );
    painted = true;

    for (cols, rows) in resize_plan {
        if started.elapsed() > HARD_TIMEOUT {
            break;
        }
        pair.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap_or_else(|err| panic!("resize failed: {err}"));
        resize_count = resize_count.saturating_add(1);
        vt.resize(usize::from(cols), usize::from(rows));

        let slice_deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < slice_deadline {
            let mut progressed = false;
            while let Ok(chunk) = rx.try_recv() {
                raw.extend_from_slice(&chunk);
                feed_vt(&mut vt, &chunk);
                last_data = Instant::now();
                progressed = true;
            }
            if !progressed {
                thread::sleep(Duration::from_millis(5));
            }
        }

        let view = vt_text(&vt);
        let joined = view.join("\n");
        if capture_width_snapshots
            && matches!(cols, 12 | 20 | 80 | 120 | 200)
            && (joined.contains("STATUS")
                || joined.contains("STREAM")
                || joined.contains("FOOTER")
                || find_subslice(&raw, b"STATUS").is_some())
        {
            // Prefer avt view after content has been fed; if the VT view is still
            // empty due to incomplete sequences, rebuild a one-shot VT from raw.
            let snap = if joined.contains("STATUS")
                || joined.contains("STREAM")
                || joined.contains("FOOTER")
            {
                view.clone()
            } else {
                snapshot_from_raw(&raw, cols, rows)
            };
            snapshots.push((cols, snap));
        }
        if joined.contains("STATUS") {
            painted = true;
        }
        if joined.contains("STREAM") && joined.contains("TOOL") {
            saw_stream_and_tools = true;
        }
        if joined.contains("plugin-frame") || joined.contains("PLUGIN") {
            saw_plugin_frame = true;
        }
        if painted
            && !joined.contains("STATUS")
            && !joined.contains("FOOTER")
            && !joined.contains("STREAM")
            && !joined.contains("TOOL")
            && !joined.contains("PLUGIN")
        {
            continuous_content = false;
        }
    }

    // End the ordered input stream: Ctrl+D is the fixture's explicit serve
    // terminator, sent through the still-owned master writer before waiting
    // on child completion (writer Drop would only fire after the wait).
    write_stimulus(&mut writer, child.as_mut(), b"\x04", "ctrl+d");

    while started.elapsed() < HARD_TIMEOUT {
        while let Ok(chunk) = rx.try_recv() {
            raw.extend_from_slice(&chunk);
            feed_vt(&mut vt, &chunk);
            last_data = Instant::now();
            let joined = vt_text(&vt).join("\n");
            if joined.contains("STREAM") && joined.contains("TOOL") {
                saw_stream_and_tools = true;
            }
            if joined.contains("plugin-frame") || joined.contains("PLUGIN") {
                saw_plugin_frame = true;
            }
        }

        if child.try_wait().ok().flatten().is_some() {
            let drain_until = Instant::now() + READ_IDLE;
            while Instant::now() < drain_until {
                while let Ok(chunk) = rx.try_recv() {
                    raw.extend_from_slice(&chunk);
                    feed_vt(&mut vt, &chunk);
                    last_data = Instant::now();
                }
                thread::sleep(Duration::from_millis(10));
            }
            break;
        }

        thread::sleep(Duration::from_millis(15));
    }

    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(writer);
    let _ = reader_thread.join();
    while let Ok(chunk) = rx.try_recv() {
        raw.extend_from_slice(&chunk);
        feed_vt(&mut vt, &chunk);
    }

    let _ = last_data.elapsed();
    let finished_within_timeout = started.elapsed() <= HARD_TIMEOUT;
    // Intermediate blank frames are defined as screen clears; row-local erase is allowed.
    let no_blank_frame = {
        let a = audit_bytes(&raw);
        a.clear_2j == 0 && a.clear_3j == 0
    };
    let audit = audit_bytes(&raw);
    let txns = extract_transactions(&raw);
    let settle_same_write = txns.iter().any(|txn| {
        find_subslice(txn, b"SETTLED-ROW").is_some()
            && (find_subslice(txn, b"STATUS").is_some()
                || find_subslice(txn, b"STREAM").is_some()
                || find_subslice(txn, b"settled-tail").is_some())
    });
    let row_erase_immediate_reflow = detect_row_erase_immediate_reflow(&raw, &txns);

    let paste_count = parse_sidechannel_u32(&raw, b"PI_TUI_PASTE=").unwrap_or(0);
    let cursor_moves = parse_sidechannel_u32(&raw, b"PI_TUI_CURSOR=").unwrap_or(0);
    let txn_count = parse_sidechannel_u32(&raw, b"PI_TUI_TXN_COUNT=")
        .unwrap_or(0)
        .max(u32::try_from(txns.len()).unwrap_or(u32::MAX));
    let fixture_resize = parse_sidechannel_u32(&raw, b"PI_TUI_RESIZE=").unwrap_or(0);

    // Live-input provenance: the serve-phase deltas must be present, complete,
    // and exact — a missing or malformed record is never a successful zero.
    let live_paste = parse_sidechannel_u32(&raw, b"PI_TUI_LIVE_PASTE=");
    let live_cursor = parse_sidechannel_u32(&raw, b"PI_TUI_LIVE_CURSOR=");
    let live_text = parse_sidechannel_text(&raw, b"PI_TUI_LIVE_TEXT=");
    // ConPTY re-synthesizes master writes as key records: the bracketed-paste
    // markers are dropped and the payload is delivered as character presses,
    // so on Windows the live witness is the pasted text, not a Paste event.
    let expected_live_paste = u32::from(!cfg!(windows));
    assert_eq!(
        live_paste,
        Some(expected_live_paste),
        "expected exactly {expected_live_paste} live paste after readiness, got {live_paste:?} \
         (live_cursor={live_cursor:?}, live_text={live_text:?}, raw_len={}, raw_tail={})",
        raw.len(),
        String::from_utf8_lossy(&raw[raw.len().saturating_sub(RAW_DIAG_TAIL)..])
            .escape_default()
            .collect::<String>()
    );
    let live_text = live_text.unwrap_or_else(|| panic!("missing live text record"));
    assert!(
        live_text.contains("PASTED-BLOCK-line1") && live_text.contains("line2"),
        "expected the pasted payload to reach the fixture after readiness, got {live_text:?}"
    );
    assert_eq!(
        live_cursor,
        Some(6),
        "expected exactly six live cursor moves after readiness, got {live_cursor:?}"
    );

    let sole_stdout_owner = find_subslice(&raw, &probe_query_batch(true)).is_some()
        && audit.clear_2j == 0
        && audit.clear_3j == 0
        && audit.sync_begin == audit.sync_end
        && !txns.is_empty();

    let saw_cursor_show = find_subslice(&raw, b"\x1b[?25h").is_some();
    // The full alternate-screen sequence contains the regular-mode bytes as a
    // subsequence (it inserts CSI ? 1049 l between CSI ? 7 h and CSI < u), so
    // a subsequence search for one variant would conflate them. Exact-window
    // equality keeps the counts disjoint: neither sequence is a contiguous
    // window of the other, so each emitted restore registers in exactly one
    // counter.
    let emergency_regular_restore_count = raw
        .windows(EMERGENCY_REGULAR_RESTORE_BYTES.len())
        .filter(|window| *window == EMERGENCY_REGULAR_RESTORE_BYTES)
        .count();
    let emergency_alternate_screen_restore_count = raw
        .windows(EMERGENCY_RESTORE_BYTES.len())
        .filter(|window| *window == EMERGENCY_RESTORE_BYTES)
        .count();

    if !saw_stream_and_tools {
        saw_stream_and_tools =
            find_subslice(&raw, b"STREAM").is_some() && find_subslice(&raw, b"TOOL").is_some();
    }
    if !saw_plugin_frame {
        saw_plugin_frame = find_subslice(&raw, b"PLUGIN").is_some()
            || find_subslice(&raw, b"plugin-frame").is_some();
    }
    if !continuous_content {
        continuous_content =
            find_subslice(&raw, b"STATUS").is_some() && find_subslice(&raw, b"STREAM").is_some();
    }

    // Final width snapshots from the complete byte stream after the child exits
    // so avt sees settled content rather than mid-resize partial frames.
    if capture_width_snapshots {
        let mut rebuilt = Vec::new();
        for &(cols, rows) in &[
            (12u16, 6u16),
            (80u16, 24u16),
            (120u16, 40u16),
            (200u16, 50u16),
        ] {
            let snap = snapshot_from_raw(&raw, cols, rows);
            let joined = snap.join("\n");
            if joined.contains("STATUS")
                || joined.contains("STREAM")
                || joined.contains("FOOTER")
                || joined.contains("PLUGIN")
            {
                rebuilt.push((cols, snap));
            }
        }
        if !rebuilt.is_empty() {
            snapshots = rebuilt;
        }
    }

    DriveReport {
        raw,
        snapshots,
        final_vt_text: vt_text(&vt),
        resize_count: resize_count.max(fixture_resize),
        paste_count,
        cursor_moves,
        txn_count,
        settle_same_write,
        row_erase_immediate_reflow,
        continuous_content,
        no_blank_frame,
        saw_plugin_frame,
        saw_stream_and_tools,
        finished_within_timeout,
        sole_stdout_owner,
        saw_cursor_show,
        emergency_regular_restore_count,
        emergency_alternate_screen_restore_count,
    }
}

/// Write harness input (paste / cursor keys / serve terminator) to the live
/// fixture. Delivery is mandatory: readiness was already observed, so a
/// write/flush failure or an already-exited child is a defect, not a race.
fn write_stimulus(
    writer: &mut impl Write,
    child: &mut dyn portable_pty::Child,
    bytes: &[u8],
    what: &str,
) {
    assert!(
        child.try_wait().ok().flatten().is_none(),
        "{what} write skipped: fixture exited before input delivery"
    );
    writer
        .write_all(bytes)
        .unwrap_or_else(|err| panic!("{what} write failed: {err}"));
    writer
        .flush()
        .unwrap_or_else(|err| panic!("{what} flush failed: {err}"));
}

fn disable_pty_echo(master: &dyn portable_pty::MasterPty) {
    // Best-effort: portable-pty's get_termios is read-only from the trait.
    // Clearing ECHO requires platform termios writes; when unavailable we rely
    // on waiting for STATUS paint and raw-byte assertions instead of echo-free
    // guarantees. The fixture also seeds probe replies itself.
    let _ = master.get_size();
    let _ = master;
}
fn snapshot_from_raw(raw: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let mut vt = Vt::builder()
        .size(usize::from(cols.max(1)), usize::from(rows.max(1)))
        .scrollback_limit(10_000)
        .build();
    feed_vt(&mut vt, raw);
    // Prefer full line buffer (scrollback + view) so settled insert_before rows
    // remain visible after aggressive resizes shrink the viewport.
    let mut lines: Vec<String> = vt
        .lines()
        .map(|line| line.text().trim_end().to_owned())
        .collect();
    if lines.iter().all(|line| line.trim().is_empty()) {
        lines = vt_text(&vt);
    }
    // Fallback: if avt lost printable content (resize edge), surface raw markers.
    let joined = lines.join("\n");
    if !(joined.contains("STATUS") || joined.contains("STREAM") || joined.contains("FOOTER")) {
        let lossy = String::from_utf8_lossy(raw);
        let mut markers = Vec::new();
        for key in [
            "STATUS",
            "STREAM",
            "TOOL",
            "PLUGIN",
            "FOOTER",
            "SETTLED-ROW",
        ] {
            if lossy.contains(key) {
                markers.push(key.to_owned());
            }
        }
        if !markers.is_empty() {
            return markers;
        }
    }
    lines
}

fn feed_vt(vt: &mut Vt, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let _ = vt.feed_str(&text);
}

fn vt_text(vt: &Vt) -> Vec<String> {
    vt.view()
        .map(|line| line.text().trim_end().to_owned())
        .collect()
}

fn extract_transactions(raw: &[u8]) -> Vec<Vec<u8>> {
    let begin_pat = b"\x1b]999;PI_TUI_TXN_BEGIN=";
    let end_pat = b"\x1b]999;PI_TUI_TXN_END=";
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some(rel) = find_subslice(&raw[idx..], begin_pat) {
        let start_at = idx + rel;
        let after_begin_tag = start_at + begin_pat.len();
        // Skip id + BEL
        let Some(bel_rel) = raw[after_begin_tag..].iter().position(|b| *b == 0x07) else {
            break;
        };
        let payload_start = after_begin_tag + bel_rel + 1;
        let Some(end_rel) = find_subslice(&raw[payload_start..], end_pat) else {
            break;
        };
        let payload_end = payload_start + end_rel;
        out.push(raw[payload_start..payload_end].to_vec());
        idx = payload_end + end_pat.len();
    }
    out
}

fn detect_row_erase_immediate_reflow(raw: &[u8], txns: &[Vec<u8>]) -> bool {
    // Full-row redraws emit CUP + EL2 then either printable content or the next
    // row's CUP. Both are valid as long as no screen clear appears between erase
    // and reflow.
    let mut saw_el2 = false;
    let sources: Vec<&[u8]> = if txns.is_empty() {
        vec![raw]
    } else {
        txns.iter().map(Vec::as_slice).collect()
    };
    for bytes in sources {
        let mut idx = 0usize;
        while let Some(rel) = find_subslice(&bytes[idx..], b"\x1b[2K") {
            saw_el2 = true;
            let after = idx + rel + b"\x1b[2K".len();
            let window = &bytes[after..bytes.len().min(after.saturating_add(128))];
            if find_subslice(window, b"\x1b[2J").is_some()
                || find_subslice(window, b"\x1b[3J").is_some()
            {
                return false;
            }
            if window.is_empty() {
                idx = after;
                continue;
            }
            let b0 = window[0];
            let ok =
                b0.is_ascii_graphic() || b0 == b' ' || b0 == b'\n' || b0 == b'\r' || b0 == 0x1b;
            if !ok {
                return false;
            }
            idx = after;
        }
    }
    if saw_el2 {
        return true;
    }
    audit_bytes(raw).clear_2j == 0 && audit_bytes(raw).clear_3j == 0
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

#[cfg(windows)]
mod windows_raw_record {
    use std::cell::Cell;
    use std::io::{self, Write, stdout};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use crossterm_winapi::{
        Console, ConsoleMode, ControlKeyState, EventFlags, Handle, InputRecord,
    };
    use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
    use serde::{Deserialize, Serialize};

    use super::{
        HARD_TIMEOUT, INITIAL_COLS, INITIAL_ROWS, READ_IDLE, find_subslice, write_stimulus,
    };

    const NOT_RAW_MASK: u32 = 0x0007;
    const VT_INPUT: u32 = 0x0200;
    const CHILD_RECORD_LIMIT: usize = 4096;
    const TRANSCRIPT_LIMIT: usize = 1_048_576;
    const CHILD_POLL_INTERVAL_MS: u64 = 5;
    const DEFAULT_CHILD_DEADLINE_MS: u64 = 15000;

    const RECORD_PREFIX: &[u8] = b"\x1b]999;PI_TUI_RAW_RECORD=";
    const READY_PREFIX: &[u8] = b"\x1b]999;PI_TUI_RAW_RECORD_READY";

    // crossterm_winapi 0.9.1's From<INPUT_RECORD> impl discards the raw
    // WINDOW_BUFFER_SIZE_RECORD dwSize and substitutes the live screen-buffer
    // size at read time, so resize coordinates cannot be captured verbatim
    // through the approved safe wrapper. These notes keep the report honest
    // about that derived provenance instead of claiming raw fidelity.
    const RESIZE_FIDELITY_NOTE: &str = "unavailable: crossterm_winapi 0.9.1 replaces WindowBufferSizeEvent dwSize with the live screen-buffer size at read time; observed_screen_x/y are read-time screen values, not the record's original coordinates";
    const STIMULUS_INJECTION_NOTE: &str = "direct master-writer injection; bypasses the Windows Terminal clipboard-paste relay that an outward \x1b[?2004h would arm; no manual-clipboard-paste claim";
    const FULL_LOSSLESS_NOTE: &str = "unproven: raw resize coordinate fidelity is unavailable through the approved safe wrapper (see resize_coordinate_fidelity); a demonstrated verdict covers raw key-record paste-delimiter retention only, not full lossless-record feasibility";
    const NEGOTIATION_NOTE: &str = "no \x1b[?2004h or \x1b[?9001h was emitted by the child and none was observed from the host; a teardown \x1b[?9001l would not prove \x1b[?9001h was negotiated; no negotiation is invented";

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    pub struct RecordEntry {
        pub idx: usize,
        pub variant: String,
        pub key_down: Option<bool>,
        pub repeat_count: Option<u16>,
        pub virtual_key_code: Option<u16>,
        pub virtual_scan_code: Option<u16>,
        pub u_char: Option<u16>,
        pub control_key_state: Option<u32>,
        pub mouse_x: Option<i16>,
        pub mouse_y: Option<i16>,
        pub button_state: Option<i32>,
        pub mouse_control_key_state: Option<u32>,
        pub event_flags: Option<u32>,
        pub observed_screen_x: Option<i16>,
        pub observed_screen_y: Option<i16>,
        pub focus_set: Option<bool>,
        pub menu_command_id: Option<u32>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct LifecycleEntry {
        pub stage: String,
        pub arm: String,
        pub original: u32,
        pub baseline: u32,
        pub requested: u32,
        pub active: Option<u32>,
        pub restored: Option<u32>,
        pub error: Option<String>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct TerminationEntry {
        pub cause: String,
        pub record_count: usize,
        pub message: Option<String>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(tag = "type")]
    pub enum Event {
        #[serde(rename = "record")]
        Record(RecordEntry),
        #[serde(rename = "lifecycle")]
        Lifecycle(LifecycleEntry),
        #[serde(rename = "termination")]
        Termination(TerminationEntry),
    }

    #[derive(Clone, Debug, Serialize)]
    pub struct ArmReport {
        pub arm: String,
        pub completed: bool,
        pub original: Option<u32>,
        pub baseline: Option<u32>,
        pub requested: Option<u32>,
        pub active: Option<u32>,
        pub restored: Option<u32>,
        pub record_count: usize,
        pub records: Vec<RecordEntry>,
        pub transcript_bytes: usize,
        pub transcript_limit_exceeded: bool,
        pub record_limit_exceeded: bool,
        pub child_deadline_exceeded: bool,
        pub child_terminated_normally: bool,
        pub termination_cause: Option<String>,
        pub termination_message: Option<String>,
        pub opener_found: bool,
        pub closer_found: bool,
        pub payload_found: bool,
        pub opener_position: Option<usize>,
        pub payload_position: Option<usize>,
        pub closer_position: Option<usize>,
        pub delimiter_order_verified: bool,
        pub navigation_inputs_identified: usize,
        pub resize_requests_sent: usize,
        pub resize_records_observed: usize,
        pub resize_coordinate_fidelity: &'static str,
        pub bracketed_paste_2004_observed: bool,
        pub mode_9001_observed: bool,
        pub line_break_codepoint: Option<String>,
        pub stop_cause: Option<String>,
        // Full lossless-record feasibility: never above "inconclusive"
        // because raw resize-coordinate fidelity is unavailable.
        pub feasibility: String,
        // Separately gated exact key-record evidence: "demonstrated" only
        // when every key-boundary check passed; never a stand-in for the
        // full feasibility verdict.
        pub exact_key_evidence: String,
        // Explicit answer to "is the raw record stream proven lossless?":
        // "refuted" when information loss was demonstrated, "inconclusive"
        // when fully assessed but raw resize-coordinate fidelity is
        // unavailable, "not_applicable" for the idle arm, "not_reached" when
        // environmental/setup failures prevented assessment.
        pub full_lossless_feasibility: String,
    }

    #[derive(Clone, Debug, Serialize)]
    pub struct FinalReport {
        pub arms: Vec<ArmReport>,
        pub cross_arm_baseline_consistent: Option<bool>,
        pub bracketed_paste_2004_emitted: bool,
        pub mode_9001_emitted: bool,
        pub negotiation_note: &'static str,
        pub limitations: Vec<&'static str>,
    }

    const RESIZE_PLAN: [(u16, u16); 24] = [
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

    struct ModeGuard {
        cm: ConsoleMode,
        original: u32,
        restored: Cell<bool>,
    }

    impl ModeGuard {
        fn new(cm: ConsoleMode, original: u32) -> Self {
            Self {
                cm,
                original,
                restored: Cell::new(false),
            }
        }

        fn set(&self, mode: u32) -> io::Result<()> {
            self.cm.set_mode(mode)
        }

        fn mode(&self) -> io::Result<u32> {
            self.cm.mode()
        }

        fn restore(&self) -> io::Result<u32> {
            if self.restored.get() {
                return self.cm.mode();
            }
            self.cm.set_mode(self.original)?;
            let m = self.cm.mode()?;
            self.restored.set(true);
            Ok(m)
        }
    }

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn control_key_state_value(state: ControlKeyState) -> u32 {
        (0..32).fold(0u32, |acc, bit| {
            let mask = 1u32 << bit;
            if state.has_state(mask) {
                acc | mask
            } else {
                acc
            }
        })
    }

    fn event_flags_value(flags: EventFlags) -> u32 {
        match flags {
            EventFlags::PressOrRelease => 0x0000,
            EventFlags::MouseMoved => 0x0001,
            EventFlags::DoubleClick => 0x0002,
            EventFlags::MouseWheeled => 0x0004,
            EventFlags::MouseHwheeled => 0x0008,
            EventFlags::Unknown => 0x0021,
        }
    }

    fn convert_record(record: InputRecord, idx: usize) -> RecordEntry {
        match record {
            InputRecord::KeyEvent(k) => RecordEntry {
                idx,
                variant: "KeyEvent".into(),
                key_down: Some(k.key_down),
                repeat_count: Some(k.repeat_count),
                virtual_key_code: Some(k.virtual_key_code),
                virtual_scan_code: Some(k.virtual_scan_code),
                u_char: Some(k.u_char),
                control_key_state: Some(control_key_state_value(k.control_key_state)),
                ..RecordEntry::default()
            },
            InputRecord::MouseEvent(m) => RecordEntry {
                idx,
                variant: "MouseEvent".into(),
                mouse_x: Some(m.mouse_position.x),
                mouse_y: Some(m.mouse_position.y),
                button_state: Some(m.button_state.state()),
                mouse_control_key_state: Some(control_key_state_value(m.control_key_state)),
                event_flags: Some(event_flags_value(m.event_flags)),
                ..RecordEntry::default()
            },
            InputRecord::WindowBufferSizeEvent(w) => RecordEntry {
                idx,
                variant: "WindowBufferSizeEvent".into(),
                // crossterm_winapi 0.9.1 overwrites the record's dwSize with
                // the live screen-buffer size at read time; these are observed
                // screen values, not the record's original coordinates.
                observed_screen_x: Some(w.size.x),
                observed_screen_y: Some(w.size.y),
                ..RecordEntry::default()
            },
            InputRecord::FocusEvent(f) => RecordEntry {
                idx,
                variant: "FocusEvent".into(),
                focus_set: Some(f.set_focus),
                ..RecordEntry::default()
            },
            InputRecord::MenuEvent(m) => RecordEntry {
                idx,
                variant: "MenuEvent".into(),
                menu_command_id: Some(m.command_id),
                ..RecordEntry::default()
            },
        }
    }

    fn is_terminator(entry: &RecordEntry) -> bool {
        entry.variant == "KeyEvent" && entry.key_down == Some(true) && entry.u_char == Some(0x0004)
    }

    fn write_osc999_line(prefix: &[u8], body: &[u8]) {
        let mut out = stdout().lock();
        out.write_all(prefix).expect("write prefix");
        out.write_all(body).expect("write body");
        out.write_all(b"\x07").expect("write bel");
        out.flush().expect("flush");
    }

    fn emit_event(event: &Event, prefix: &[u8]) {
        let json = serde_json::to_string(event).expect("serialize event");
        write_osc999_line(prefix, json.as_bytes());
    }

    fn emit_ready(arm: &str, active: u32) {
        let body = format!("=1;arm={arm};active={active}");
        write_osc999_line(READY_PREFIX, body.as_bytes());
    }

    fn emit_lifecycle_and_finish(
        arm: &str,
        original: u32,
        baseline: u32,
        requested: u32,
        guard: &ModeGuard,
        termination: TerminationEntry,
    ) {
        let mut termination = termination;
        let mut errors: Vec<String> = Vec::new();

        let restored = match guard.restore() {
            Ok(m) => Some(m),
            Err(e) => {
                errors.push(format!("restore failed: {e}"));
                None
            }
        };

        // The post-restore mode is observed evidence: a failed read is
        // recorded as an error and reported as `None`, never fabricated
        // as a mode word.
        let active = match guard.mode() {
            Ok(m) => Some(m),
            Err(e) => {
                errors.push(format!("read post-restore mode failed: {e}"));
                None
            }
        };

        if let Some(m) = restored
            && m != original
        {
            errors.push(format!("restored {m:#06x} != original {original:#06x}"));
        }

        let error = if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        };
        if let Some(e) = &error {
            termination.message = Some(match termination.message.take() {
                Some(prev) => format!("{prev}; {e}"),
                None => e.clone(),
            });
        }

        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "teardown".into(),
                arm: arm.into(),
                original,
                baseline,
                requested,
                active,
                restored,
                error,
            }),
            RECORD_PREFIX,
        );
        emit_event(&Event::Termination(termination), RECORD_PREFIX);
    }

    pub fn run_child() {
        let arm = std::env::var("PI_TUI_RAW_RECORD_ARM")
            .unwrap_or_else(|e| panic!("PI_TUI_RAW_RECORD_ARM must be set to A, B, or IDLE: {e}"));

        assert!(
            matches!(arm.as_str(), "A" | "B" | "IDLE"),
            "PI_TUI_RAW_RECORD_ARM must be A, B, or IDLE, got {arm}"
        );

        let deadline_ms: u64 = match std::env::var("PI_TUI_RAW_RECORD_DEADLINE_MS") {
            Ok(s) => s.parse().unwrap_or_else(|e| {
                panic!("PI_TUI_RAW_RECORD_DEADLINE_MS {s:?} is not a u64: {e}")
            }),
            Err(std::env::VarError::NotPresent) => DEFAULT_CHILD_DEADLINE_MS,
            Err(e) => panic!("PI_TUI_RAW_RECORD_DEADLINE_MS is not readable: {e}"),
        };
        let deadline = Duration::from_millis(deadline_ms);

        let in_handle = match Handle::current_in_handle() {
            Ok(h) => h,
            Err(e) => {
                emit_event(
                    &Event::Termination(TerminationEntry {
                        cause: "error".into(),
                        record_count: 0,
                        message: Some(format!("current_in_handle failed: {e}")),
                    }),
                    RECORD_PREFIX,
                );
                return;
            }
        };

        let cm = ConsoleMode::from(in_handle.clone());
        let console = Console::from(in_handle);

        let original = match cm.mode() {
            Ok(m) => m,
            Err(e) => {
                emit_event(
                    &Event::Termination(TerminationEntry {
                        cause: "error".into(),
                        record_count: 0,
                        message: Some(format!("read original mode failed: {e}")),
                    }),
                    RECORD_PREFIX,
                );
                return;
            }
        };

        let baseline = original & !NOT_RAW_MASK;
        let requested = match arm.as_str() {
            "B" => baseline | VT_INPUT,
            _ => baseline,
        };

        let guard = ModeGuard::new(cm, original);

        let mut termination = TerminationEntry {
            cause: "inconclusive".into(),
            record_count: 0,
            message: None,
        };

        if let Err(e) = guard.set(requested) {
            termination.cause = "error".into();
            termination.message = Some(format!("set_mode({requested:#06x}) failed: {e}"));
            emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
            return;
        }

        let active = match guard.mode() {
            Ok(m) => m,
            Err(e) => {
                termination.cause = "error".into();
                termination.message = Some(format!("read active mode failed: {e}"));
                emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
                return;
            }
        };

        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "setup".into(),
                arm: arm.clone(),
                original,
                baseline,
                requested,
                active: Some(active),
                restored: None,
                error: if active == requested {
                    None
                } else {
                    Some(format!(
                        "active {active:#06x} != requested {requested:#06x}"
                    ))
                },
            }),
            RECORD_PREFIX,
        );

        if active != requested {
            termination.cause = "inconclusive".into();
            termination.message = Some(format!(
                "mode setup mismatch: active {active:#06x} != requested {requested:#06x}"
            ));
            emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
            return;
        }

        emit_ready(&arm, active);
        emit_event(
            &Event::Lifecycle(LifecycleEntry {
                stage: "ready".into(),
                arm: arm.clone(),
                original,
                baseline,
                requested,
                active: Some(active),
                restored: None,
                error: None,
            }),
            RECORD_PREFIX,
        );

        let started = Instant::now();
        let mut record_count: usize = 0;
        while started.elapsed() < deadline {
            let count = match console.number_of_console_input_events() {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(CHILD_POLL_INTERVAL_MS));
                    continue;
                }
                Ok(c) => c,
                Err(e) => {
                    termination.cause = "error".into();
                    termination.message =
                        Some(format!("number_of_console_input_events failed: {e}"));
                    break;
                }
            };

            for _ in 0..count {
                if started.elapsed() >= deadline {
                    break;
                }
                if record_count >= CHILD_RECORD_LIMIT {
                    termination.cause = "record_limit".into();
                    termination.message =
                        Some(format!("reached record limit {CHILD_RECORD_LIMIT}"));
                    break;
                }

                match console.read_single_input_event() {
                    Ok(record) => {
                        record_count += 1;
                        let entry = convert_record(record, record_count);
                        let term = is_terminator(&entry);
                        emit_event(&Event::Record(entry), RECORD_PREFIX);
                        if term {
                            termination.cause = "normal".into();
                            termination.message =
                                Some(format!("saw Ctrl+D terminator at record {record_count}"));
                            break;
                        }
                    }
                    Err(e) => {
                        termination.cause = "error".into();
                        termination.message = Some(format!("read_single_input_event failed: {e}"));
                        break;
                    }
                }
            }

            if !matches!(termination.cause.as_str(), "inconclusive") {
                break;
            }

            thread::sleep(Duration::from_millis(CHILD_POLL_INTERVAL_MS));
        }

        if termination.cause == "inconclusive" {
            termination.cause = "deadline".into();
            termination.message = Some(format!(
                "reached child deadline {} ms",
                deadline.as_millis()
            ));
        }
        termination.record_count = record_count;

        emit_lifecycle_and_finish(&arm, original, baseline, requested, &guard, termination);
    }

    fn drain_pending(rx: &mpsc::Receiver<Vec<u8>>, raw: &mut Vec<u8>) {
        while let Ok(chunk) = rx.try_recv() {
            raw.extend_from_slice(&chunk);
        }
    }

    const NAV_VK_CODES: [u16; 6] = [0x25, 0x27, 0x26, 0x28, 0x24, 0x23];
    const NAV_CSI_FINALS: [u16; 6] = [0x44, 0x43, 0x41, 0x42, 0x48, 0x46];

    struct KeyEvidence {
        opener_pos: Option<usize>,
        payload_pos: Option<usize>,
        closer_pos: Option<usize>,
        line_break: Option<String>,
        navigation_identified: usize,
    }

    fn analyze_key_records(records: &[RecordEntry]) -> KeyEvidence {
        let key_chars: Vec<u16> = records
            .iter()
            .filter(|r| r.variant == "KeyEvent" && r.key_down == Some(true))
            .filter_map(|r| r.u_char)
            .collect();

        let opener = [0x001Bu16, 0x005B, 0x0032, 0x0030, 0x0030, 0x007E];
        let closer = [0x001B, 0x005B, 0x0032, 0x0030, 0x0031, 0x007E];

        let opener_pos = find_subslice(&key_chars, &opener);
        let closer_pos = find_subslice(&key_chars, &closer);

        let payload_lf: Vec<u16> = b"PASTED-BLOCK-line1\nline2"
            .iter()
            .map(|&b| u16::from(b))
            .collect();
        let payload_cr: Vec<u16> = b"PASTED-BLOCK-line1\rline2"
            .iter()
            .map(|&b| u16::from(b))
            .collect();
        let payload_crlf: Vec<u16> = b"PASTED-BLOCK-line1\r\nline2"
            .iter()
            .map(|&b| u16::from(b))
            .collect();

        let (payload_pos, line_break) = if let Some(p) = find_subslice(&key_chars, &payload_lf) {
            (Some(p), Some("LF".into()))
        } else if let Some(p) = find_subslice(&key_chars, &payload_cr) {
            (Some(p), Some("CR".into()))
        } else if let Some(p) = find_subslice(&key_chars, &payload_crlf) {
            (Some(p), Some("CRLF".into()))
        } else {
            (None, None)
        };

        // A navigation input is identifiable either as a translated VK
        // key-down record or as its raw CSI final in the u_char stream;
        // which form arrives is itself evidence, so neither is prescribed.
        let mut navigation_identified = 0usize;
        for (vk, final_byte) in NAV_VK_CODES.iter().zip(NAV_CSI_FINALS.iter()) {
            let by_vk = records.iter().any(|r| {
                r.variant == "KeyEvent"
                    && r.key_down == Some(true)
                    && r.virtual_key_code == Some(*vk)
            });
            let by_csi = find_subslice(&key_chars, &[0x001B, 0x005B, *final_byte]).is_some();
            if by_vk || by_csi {
                navigation_identified += 1;
            }
        }

        KeyEvidence {
            opener_pos,
            payload_pos,
            closer_pos,
            line_break,
            navigation_identified,
        }
    }

    /// Parses the delimited record stream. Every prefixed record must be
    /// well-formed: a truncated record, invalid UTF-8, or invalid JSON is
    /// a named failure, never a silent skip, because a skipped record is
    /// indistinguishable from dropped evidence. Events parsed before a
    /// failure are retained for the report alongside the failure.
    fn parse_events(raw: &[u8]) -> (Vec<Event>, Option<String>) {
        let mut events = Vec::new();
        let mut idx = 0;
        while let Some(rel) = find_subslice(&raw[idx..], RECORD_PREFIX) {
            let start = idx + rel + RECORD_PREFIX.len();
            let Some(end_rel) = raw
                .get(start..)
                .and_then(|tail| tail.iter().position(|&b| b == 0x07))
            else {
                return (
                    events,
                    Some(format!(
                        "truncated record at byte {start}: prefix without BEL terminator"
                    )),
                );
            };
            let end = start + end_rel;
            let s = match std::str::from_utf8(&raw[start..end]) {
                Ok(s) => s,
                Err(e) => {
                    return (
                        events,
                        Some(format!("record at byte {start} is not valid UTF-8: {e}")),
                    );
                }
            };
            match serde_json::from_str::<Event>(s) {
                Ok(ev) => events.push(ev),
                Err(e) => {
                    return (
                        events,
                        Some(format!("record at byte {start} is not valid JSON: {e}")),
                    );
                }
            }
            idx = end + 1;
        }
        (events, None)
    }

    fn run_arm(pty_system: &NativePtySystem, arm: &str, has_stimulus: bool) -> ArmReport {
        let mut report = ArmReport {
            arm: arm.into(),
            completed: false,
            original: None,
            baseline: None,
            requested: None,
            active: None,
            restored: None,
            record_count: 0,
            records: Vec::new(),
            transcript_bytes: 0,
            transcript_limit_exceeded: false,
            record_limit_exceeded: false,
            child_deadline_exceeded: false,
            child_terminated_normally: false,
            termination_cause: None,
            termination_message: None,
            opener_found: false,
            closer_found: false,
            payload_found: false,
            opener_position: None,
            payload_position: None,
            closer_position: None,
            delimiter_order_verified: false,
            navigation_inputs_identified: 0,
            resize_requests_sent: 0,
            resize_records_observed: 0,
            resize_coordinate_fidelity: RESIZE_FIDELITY_NOTE,
            bracketed_paste_2004_observed: false,
            mode_9001_observed: false,
            line_break_codepoint: None,
            stop_cause: None,
            feasibility: "inconclusive".into(),
            exact_key_evidence: "not_reached".into(),
            full_lossless_feasibility: "not_reached".into(),
        };

        let pair = match pty_system.openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => {
                report.stop_cause = Some(format!("openpty failed: {e}"));
                return report;
            }
        };

        let exe = std::env::current_exe().expect("current_exe");
        let mut cmd = CommandBuilder::new(&exe);
        cmd.arg("--exact");
        cmd.arg("windows_raw_input_record_child");
        cmd.arg("--ignored");
        cmd.arg("--nocapture");
        cmd.arg("--test-threads=1");
        cmd.env("PI_TUI_RAW_RECORD_ARM", arm);
        cmd.env(
            "PI_TUI_RAW_RECORD_DEADLINE_MS",
            if has_stimulus { "15000" } else { "3000" },
        );
        cmd.env("NO_COLOR", "1");

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                report.stop_cause = Some(format!("spawn_command failed: {e}"));
                return report;
            }
        };
        drop(pair.slave);

        let mut writer = pair.master.take_writer().expect("take writer");
        let mut reader = pair.master.try_clone_reader().expect("clone reader");

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader_thread = thread::spawn(move || {
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

        let started = Instant::now();
        let mut raw: Vec<u8> = Vec::new();
        let mut ready = false;

        let ready_pat = format!("\x1b]999;PI_TUI_RAW_RECORD_READY=1;arm={arm}").into_bytes();

        while started.elapsed() < HARD_TIMEOUT && !ready {
            drain_pending(&rx, &mut raw);
            if raw.len() > TRANSCRIPT_LIMIT {
                report.transcript_limit_exceeded = true;
                break;
            }
            if find_subslice(&raw, &ready_pat).is_some() {
                ready = true;
                break;
            }
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        if ready && has_stimulus {
            write_stimulus(
                &mut writer,
                child.as_mut(),
                b"\x1b[200~PASTED-BLOCK-line1\nline2\x1b[201~",
                "paste",
            );
            write_stimulus(
                &mut writer,
                child.as_mut(),
                b"\x1b[D\x1b[C\x1b[A\x1b[B\x1b[H\x1b[F",
                "cursor",
            );

            for (cols, rows) in RESIZE_PLAN {
                if started.elapsed() > HARD_TIMEOUT {
                    break;
                }
                if let Err(e) = pair.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    report.stop_cause = Some(format!("resize failed: {e}"));
                    break;
                }
                report.resize_requests_sent += 1;
                let slice_deadline = Instant::now() + Duration::from_millis(50);
                while Instant::now() < slice_deadline {
                    drain_pending(&rx, &mut raw);
                    if raw.len() > TRANSCRIPT_LIMIT {
                        report.transcript_limit_exceeded = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if report.transcript_limit_exceeded {
                    break;
                }
            }

            if !report.transcript_limit_exceeded {
                write_stimulus(&mut writer, child.as_mut(), b"\x04", "ctrl+d");
            }
        }

        let mut child_exited = false;
        while started.elapsed() < HARD_TIMEOUT {
            drain_pending(&rx, &mut raw);
            if raw.len() > TRANSCRIPT_LIMIT {
                report.transcript_limit_exceeded = true;
                break;
            }
            if child.try_wait().ok().flatten().is_some() {
                child_exited = true;
                let drain_until = Instant::now() + READ_IDLE;
                while Instant::now() < drain_until {
                    drain_pending(&rx, &mut raw);
                    thread::sleep(Duration::from_millis(10));
                }
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }

        if !child_exited {
            let _ = child.kill();
            let _ = child.wait();
            report.stop_cause = Some("child did not exit before HARD_TIMEOUT".into());
        }
        drop(writer);
        let _ = reader_thread.join();
        drain_pending(&rx, &mut raw);

        report.transcript_bytes = raw.len();
        if raw.len() > TRANSCRIPT_LIMIT {
            report.transcript_limit_exceeded = true;
        }

        let (events, parse_failure) = parse_events(&raw);
        let mut lifecycles: Vec<LifecycleEntry> = Vec::new();
        let mut records: Vec<RecordEntry> = Vec::new();
        let mut termination: Option<TerminationEntry> = None;

        for ev in events {
            match ev {
                Event::Lifecycle(l) => lifecycles.push(l),
                Event::Record(r) => records.push(r),
                Event::Termination(t) => termination = Some(t),
            }
        }

        let setup = lifecycles.iter().find(|l| l.stage == "setup");
        let teardown = lifecycles.iter().find(|l| l.stage == "teardown");

        if let Some(s) = setup {
            report.original = Some(s.original);
            report.baseline = Some(s.baseline);
            report.requested = Some(s.requested);
            report.active = s.active;
        }

        if let Some(t) = teardown {
            report.restored = t.restored;
        }

        if let Some(t) = &termination {
            report.record_count = t.record_count;
            report.child_terminated_normally = t.cause == "normal";
            report.termination_cause = Some(t.cause.clone());
            report.termination_message.clone_from(&t.message);
            report.record_limit_exceeded = t.cause == "record_limit";
            report.child_deadline_exceeded = t.cause == "deadline";
        }

        // Record indexes are a contiguous 1-based sequence assigned by the
        // child, and the termination entry carries the child's own record
        // count. A gap or a disagreement means records were dropped between
        // child and parent, in which case no evidence-based verdict may
        // stand.
        let record_evidence_error = if records.iter().enumerate().any(|(pos, r)| r.idx != pos + 1) {
            Some("record index sequence is not contiguous from 1; records were dropped".to_string())
        } else {
            match termination.as_ref().map(|t| t.record_count) {
                Some(n) if n == records.len() => None,
                Some(n) => Some(format!(
                    "termination record_count {n} != {} parsed records; records were dropped",
                    records.len()
                )),
                None => Some(
                    "termination record missing; child record count cannot be cross-checked".into(),
                ),
            }
        };
        let evidence_failures: Vec<String> = [parse_failure, record_evidence_error]
            .into_iter()
            .flatten()
            .collect();

        let evidence = analyze_key_records(&records);
        report.opener_found = evidence.opener_pos.is_some();
        report.closer_found = evidence.closer_pos.is_some();
        report.payload_found = evidence.payload_pos.is_some();
        report.opener_position = evidence.opener_pos;
        report.payload_position = evidence.payload_pos;
        report.closer_position = evidence.closer_pos;
        report.delimiter_order_verified = matches!(
            (evidence.opener_pos, evidence.payload_pos, evidence.closer_pos),
            (Some(o), Some(p), Some(c)) if o < p && p < c
        );
        report.navigation_inputs_identified = evidence.navigation_identified;
        report.line_break_codepoint = evidence.line_break;
        // The observed resize count is recorded as-is: the OS may coalesce
        // the 24 resize requests, so no one-record-per-request correspondence
        // is assumed.
        report.resize_records_observed = records
            .iter()
            .filter(|r| r.variant == "WindowBufferSizeEvent")
            .count();
        report.bracketed_paste_2004_observed = find_subslice(&raw, b"\x1b[?2004h").is_some()
            || find_subslice(&raw, b"\x1b[?2004l").is_some();
        report.mode_9001_observed = find_subslice(&raw, b"\x1b[?9001h").is_some()
            || find_subslice(&raw, b"\x1b[?9001l").is_some();
        let last_record_is_terminator = records.last().is_some_and(is_terminator);
        let lifecycle_errors: Vec<String> =
            lifecycles.iter().filter_map(|l| l.error.clone()).collect();
        report.records = records;

        report.completed = child_exited && !report.transcript_limit_exceeded;

        if !report.completed {
            report.feasibility = "failed".into();
            if report.stop_cause.is_none() {
                report.stop_cause = Some("child did not complete".into());
            }
        } else if !evidence_failures.is_empty() {
            // A rejected record or a broken record-index sequence means
            // evidence was dropped in transit; dropped evidence can never
            // produce a verdict.
            report.feasibility = "failed".into();
            let failure = evidence_failures.join("; ");
            report.stop_cause = Some(match report.stop_cause.take() {
                Some(prev) => format!("{prev}; {failure}"),
                None => failure,
            });
        } else if report.record_limit_exceeded {
            report.feasibility = "failed".into();
            report.stop_cause = Some("record count exceeded 4096".into());
        } else if report.active != report.requested {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            if report.stop_cause.is_none() {
                report.stop_cause = Some("active mode did not match requested".into());
            }
        } else if report.stop_cause.is_some() {
            // A retained mid-plan cause (for example a resize failure) is an
            // environmental/delivery defect; never clobber it with a verdict.
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
        } else if arm == "IDLE" {
            report.feasibility = "nonblocking_idle".into();
            report.exact_key_evidence = "not_applicable".into();
        } else if report.original.is_some_and(|o| o & VT_INPUT != 0) {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            report.stop_cause = Some(
                "baseline already has ENABLE_VIRTUAL_TERMINAL_INPUT (0x0200); no A/B contrast"
                    .into(),
            );
        } else if !report.child_terminated_normally {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            report.stop_cause = Some(format!(
                "child did not stop on a recorded Ctrl+D terminator (termination cause: {})",
                report
                    .termination_cause
                    .as_deref()
                    .unwrap_or("none recorded")
            ));
        } else if !last_record_is_terminator {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            report.stop_cause =
                Some("ordered Ctrl+D terminator record not retained as final record".into());
        } else if !matches!((report.original, report.restored), (Some(o), Some(r)) if o == r) {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            report.stop_cause = Some("exact original input mode restore not observed".into());
        } else if !lifecycle_errors.is_empty() {
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "inconclusive".into();
            report.stop_cause = Some(format!(
                "lifecycle errors retained: {}",
                lifecycle_errors.join("; ")
            ));
        } else if evidence.opener_pos.is_none() || evidence.closer_pos.is_none() {
            // Setup, delivery, and termination are verified, so a missing
            // delimiter is verified information loss, not an environmental
            // failure.
            report.feasibility = "incomplete".into();
            report.exact_key_evidence = "incomplete".into();
            let missing = match (evidence.opener_pos.is_none(), evidence.closer_pos.is_none()) {
                (true, true) => "ESC[200~ opener and ESC[201~ closer",
                (true, false) => "ESC[200~ opener",
                _ => "ESC[201~ closer",
            };
            report.stop_cause = Some(format!("missing genuine {missing} delimiter"));
        } else if evidence.payload_pos.is_none() {
            report.feasibility = "incomplete".into();
            report.exact_key_evidence = "incomplete".into();
            report.stop_cause = Some("exact paste payload not found in raw key records".into());
        } else if !report.delimiter_order_verified {
            report.feasibility = "incomplete".into();
            report.exact_key_evidence = "incomplete".into();
            report.stop_cause = Some(format!(
                "delimiter/payload ordering not retained: opener@{:?} payload@{:?} closer@{:?}",
                evidence.opener_pos, evidence.payload_pos, evidence.closer_pos
            ));
        } else if evidence.navigation_identified < 6 {
            report.feasibility = "incomplete".into();
            report.exact_key_evidence = "incomplete".into();
            report.stop_cause = Some(format!(
                "only {} of six navigation inputs identifiable in raw records",
                evidence.navigation_identified
            ));
        } else if report.resize_records_observed == 0 {
            report.feasibility = "incomplete".into();
            report.exact_key_evidence = "incomplete".into();
            report.stop_cause = Some("no WindowBufferSizeEvent records retained".into());
        } else {
            // Every key-boundary check passed: exact key evidence is
            // demonstrated. Full lossless-record feasibility remains
            // inconclusive because raw resize-coordinate fidelity is
            // unavailable through the approved safe wrapper; the narrower
            // result is never substituted for it.
            report.feasibility = "inconclusive".into();
            report.exact_key_evidence = "demonstrated".into();
            report.stop_cause = Some(FULL_LOSSLESS_NOTE.into());
        }

        report.full_lossless_feasibility = match report.feasibility.as_str() {
            // Verified information loss refutes full lossless-record
            // feasibility outright.
            "incomplete" => "refuted".into(),
            // The idle arm does not exercise the record path.
            "nonblocking_idle" => "not_applicable".into(),
            // A fully assessed arm still cannot prove lossless capture:
            // raw resize-coordinate fidelity is unavailable through the
            // approved safe wrapper.
            "inconclusive" if report.exact_key_evidence == "demonstrated" => "inconclusive".into(),
            // Environmental/setup/termination failures never reached the
            // full-lossless assessment.
            _ => "not_reached".into(),
        };

        report
    }

    pub fn run_parent() {
        let pty_system = NativePtySystem::default();
        let arms = [("A", true), ("B", true), ("IDLE", false)];
        let mut arm_reports = Vec::new();

        for (arm, has_stimulus) in arms {
            let report = run_arm(&pty_system, arm, has_stimulus);
            arm_reports.push(report);
        }

        // The A/B contrast is only valid when both fresh ConPTY children
        // inherited the same baseline mode word.
        let cross_arm_baseline_consistent = match (arm_reports[0].baseline, arm_reports[1].baseline)
        {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
        if cross_arm_baseline_consistent == Some(false) {
            for r in arm_reports.iter_mut().take(2) {
                if r.exact_key_evidence == "demonstrated" {
                    r.exact_key_evidence = "inconclusive".into();
                    r.full_lossless_feasibility = "not_reached".into();
                    r.stop_cause = Some(
                        "cross-arm baseline mismatch: arms A and B observed different baseline mode words; no valid A/B contrast"
                            .into(),
                    );
                }
            }
        }

        let final_report = FinalReport {
            arms: arm_reports,
            cross_arm_baseline_consistent,
            bracketed_paste_2004_emitted: false,
            mode_9001_emitted: false,
            negotiation_note: NEGOTIATION_NOTE,
            limitations: vec![
                STIMULUS_INJECTION_NOTE,
                RESIZE_FIDELITY_NOTE,
                FULL_LOSSLESS_NOTE,
            ],
        };
        let json = serde_json::to_string_pretty(&final_report).expect("serialize final report");

        let mut out = stdout().lock();
        out.write_all(json.as_bytes()).expect("write final report");
        out.write_all(b"\n").expect("write newline");
        out.flush().expect("flush final report");

        for r in &final_report.arms {
            assert!(r.completed, "arm {} did not complete", r.arm);
            assert!(
                !r.transcript_limit_exceeded,
                "arm {} exceeded 1 MiB transcript limit",
                r.arm
            );
            assert!(
                !r.record_limit_exceeded,
                "arm {} exceeded 4096 record limit",
                r.arm
            );
            match (r.original, r.restored) {
                (Some(o), Some(rst)) => {
                    assert_eq!(o, rst, "arm {} did not restore exact original mode", r.arm);
                }
                _ => panic!("arm {} missing mode restoration words", r.arm),
            }
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_raw_input_records_vt_mode_ab() {
    windows_raw_record::run_parent();
}

#[cfg(windows)]
#[test]
#[ignore = "helper entry point: the parent test `windows_raw_input_records_vt_mode_ab` spawns this \
 by name under a private ConPTY environment and reserved env contract"]
fn windows_raw_input_record_child() {
    windows_raw_record::run_child();
}
