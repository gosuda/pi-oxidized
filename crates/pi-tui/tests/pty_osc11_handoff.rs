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

//! Native Linux PTY regression: split OSC 11 reply across startup handoff.
//!
//! The `pi_tui_osc11_handoff_fixture` binary runs the real
//! `TerminalSession`/`TerminalInput` pipeline. The harness deliberately
//! fragments an OSC 11 background reply so the first half arrives while the
//! startup probe owns stdin and the second half arrives after the
//! `EventStream` has taken over. The fixture must classify the reply and must
//! not leak the payload as ordinary input.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, NativePtySystem, PtySize, PtySystem};

const HARD_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_KILL_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_ROWS: u16 = 24;
const PROBE_COLS: u16 = 80;
const PROBE_QUERY: &[u8] = b"\x1b]11;?\x07";
const HANDSHAKE: &[u8] = b"READY\n";
const RESULT_PREFIX: &[u8] = b"DARK=";

#[test]
#[cfg(unix)]
fn osc11_reply_split_across_startup_handoff() {
    let raw = drive(
        // First fragment: the OSC 11 introducer and payload prefix.
        b"\x1b]11;rgb:0",
        // Second fragment: the remainder and BEL terminator.
        b"000/000/000\x07",
    );

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("DARK=true"),
        "split OSC 11 must be classified as a dark background; got: {text:?}"
    );
    assert!(
        !text.contains("rgb:"),
        "OSC 11 payload must not leak into printable output; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn osc11_split_at_payload_middle_preserves_trust() {
    // Fragment after the introducer, at the payload body, to prove the split
    // boundary does not matter as long as both halves make a valid reply.
    let raw = drive(b"\x1b]11;rgb:0000/", b"0000/0000\x07");

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("DARK=true"),
        "mid-payload split must still classify; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn osc11_overlong_payload_is_ignored() {
    // A deliberate oversize second half should be rejected by the bounded
    // payload parser and not be mistaken for a dark or light background.
    let garbage = "x".repeat(200);
    let second = format!("{garbage}\x07").into_bytes();
    let raw = drive(b"\x1b]11;rgb:0", &second);

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("DARK=unknown"),
        "overlong OSC 11 must not be classified; got: {text:?}"
    );
    assert!(
        text.contains("RECOVERED=true"),
        "overlong recognized framing must latch a protocol error and recover; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn lone_esc_at_startup_expires_into_esc_after_deadline() {
    // A lone ESC in its own read must not wedge the parser: at the absolute
    // 50 ms framing deadline it decodes into the Esc key, exactly once, and
    // everything after it keeps decoding normally.
    let raw = drive_scenario(b"\x1b", b"", false);

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("ESC=true"),
        "lone ESC must surface as the Esc key; got: {text:?}"
    );
    assert!(
        text.contains("DARK=unknown"),
        "a bare ESC must not classify a background; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn early_keystrokes_survive_the_startup_probe_in_order() {
    // Keys typed while the probe collector drives the shared reader must be
    // queued (not re-injected, not dropped) and delivered in order after
    // start_input.
    let raw = drive_scenario(b"ab\x1b[B", b"", false);

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("KEYS=ab<DOWN>"),
        "early keys must be delivered in order after the stream starts; got: {text:?}"
    );
    assert!(
        !text.contains("rgb:"),
        "no probe payload may leak into key events; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn reply_flood_then_requery_quiesces_and_stays_live() {
    // A sustained reply flood between startup and shutdown must not wedge
    // the worker (bounded per-iteration reads; wake/shutdown honored), and a
    // mid-session requery over the SAME shared parser must still classify.
    let flood = b"\x1b]11;rgb:00/00/00\x07".repeat(150);
    let raw = drive_scenario(b"", &flood, true);

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("REQUERY=true"),
        "requery over the shared parser must classify the answered reply; got: {text:?}"
    );
    assert!(
        text.contains("DARK=true"),
        "flooded replies must reach the sink without wedging the session; got: {text:?}"
    );
}

#[test]
#[cfg(unix)]
fn pause_quiesces_while_reply_writer_is_still_active() {
    // The fixture's mid-session pause drops and JOINS the EventStream
    // worker. This scenario keeps the reply producer writing through that
    // pause — and through the final shutdown pause — and stops it only
    // after the acknowledged result is observed. A pause that wedges under
    // a sustained flood would hang the fixture, the DARK= result would
    // never arrive, and the harness watchdogs would fail the test.
    let raw = drive_with_active_writer();

    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("DARK=true"),
        "flooded replies must be classified while the writer is still active; got: {text:?}"
    );
    assert!(
        text.contains("REQUERY=true"),
        "the mid-session requery must classify while the writer is still active; got: {text:?}"
    );
}

/// Runs the flood scenario with a continuously ACTIVE producer: after the
/// READY handshake a dedicated thread floods complete OSC 11 replies until
/// the fixture's classified result is observed, answers the mid-session
/// requery on request, and only then stops. Every pause the fixture
/// performs therefore happens while the writer is still running.
fn drive_with_active_writer() -> Vec<u8> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let binary = fixture_binary();
    let pty = NativePtySystem::default();
    let pair = pty
        .openpty(PtySize {
            rows: PROBE_ROWS,
            cols: PROBE_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap_or_else(|err| panic!("openpty failed: {err}"));

    let mut cmd = CommandBuilder::new(&binary);
    cmd.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .unwrap_or_else(|err| panic!("spawn fixture failed: {err}"));
    drop(pair.slave);

    let mut writer = pair
        .master
        .take_writer()
        .unwrap_or_else(|err| panic!("pty writer: {err}"));
    let reader = pair
        .master
        .try_clone_reader()
        .unwrap_or_else(|err| panic!("pty reader: {err}"));

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 256];
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

    let mut raw = Vec::new();
    wait_for_substring(&mut raw, &rx, &mut *child, PROBE_QUERY, "probe query");
    wait_for_substring(&mut raw, &rx, &mut *child, HANDSHAKE, "READY handshake");

    // The continuously active producer: floods complete replies, writes the
    // requery answer exactly once on request, and stops only after the
    // acknowledged result was observed — never before the pauses it must
    // survive.
    let stop = Arc::new(AtomicBool::new(false));
    let (answer_tx, answer_rx) = mpsc::channel::<()>();
    let writer_thread = {
        let stop = stop.clone();
        thread::spawn(move || {
            let ten_replies = b"\x1b]11;rgb:00/00/00\x07".repeat(10);
            let mut answered = false;
            while !stop.load(Ordering::SeqCst) {
                if !answered && answer_rx.try_recv().is_ok() {
                    writer
                        .write_all(b"\x1b]11;rgb:0000/0000/0000\x07")
                        .unwrap_or_else(|err| panic!("requery answer: {err}"));
                    writer
                        .flush()
                        .unwrap_or_else(|err| panic!("requery answer flush: {err}"));
                    answered = true;
                }
                // Tight loop: the master write blocks while the slave's
                // input queue is full, which keeps bytes pending before
                // every reader read — the sustained-occupancy schedule the
                // drain fairness contract is about. No sleeping: the
                // producer must never let the queue run dry.
                for chunk in ten_replies.chunks(64) {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    writer
                        .write_all(chunk)
                        .unwrap_or_else(|err| panic!("flood write: {err}"));
                    writer
                        .flush()
                        .unwrap_or_else(|err| panic!("flood flush: {err}"));
                }
            }
        })
    };

    // The fixture sleeps 120 ms after READY (draining the flood), then
    // pauses the stream and requeries — while the producer above is still
    // writing. Answer the requery through the producer thread.
    wait_for_occurrences(&mut raw, &rx, &mut *child, PROBE_QUERY, 2, "requery query");
    let _ = answer_tx.send(());

    // Wait for the classified result; only then stop the producer.
    wait_for_substring(&mut raw, &rx, &mut *child, RESULT_PREFIX, "DARK= result");
    stop.store(true, Ordering::SeqCst);
    let _ = writer_thread.join();

    let kill_deadline = Instant::now() + CHILD_KILL_TIMEOUT;
    while child.try_wait().ok().flatten().is_none() && Instant::now() < kill_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();

    let _ = reader_thread.join();

    while let Ok(chunk) = rx.try_recv() {
        raw.extend_from_slice(&chunk);
    }
    raw
}

fn drive(first: &[u8], second: &[u8]) -> Vec<u8> {
    drive_scenario(first, second, false)
}

/// Runs one fixture scenario: `phase0` is written while the startup probe
/// still owns the reader (after the probe batch is observed), `phase1` after
/// the READY handshake (once the `EventStream` owns the reader). When
/// `answer_requery` is set, the harness answers the fixture's mid-session
/// OSC 11 requery with a dark reply.
fn drive_scenario(phase0: &[u8], phase1: &[u8], answer_requery: bool) -> Vec<u8> {
    let binary = fixture_binary();
    let pty = NativePtySystem::default();
    let pair = pty
        .openpty(PtySize {
            rows: PROBE_ROWS,
            cols: PROBE_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap_or_else(|err| panic!("openpty failed: {err}"));

    let mut cmd = CommandBuilder::new(&binary);
    cmd.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .unwrap_or_else(|err| panic!("spawn fixture failed: {err}"));
    drop(pair.slave);

    let mut writer = pair
        .master
        .take_writer()
        .unwrap_or_else(|err| panic!("pty writer: {err}"));
    let reader = pair
        .master
        .try_clone_reader()
        .unwrap_or_else(|err| panic!("pty reader: {err}"));

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 256];
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

    let mut raw = Vec::new();

    // Wait for the startup probe query (which proves raw mode is active and
    // the probe task has started reading stdin), then immediately write the
    // first reply fragment while the probe still owns stdin.
    wait_for_substring(&mut raw, &rx, &mut *child, PROBE_QUERY, "probe query");
    write_paced(&mut writer, phase0);

    // Wait for the fixture to finish the probe, hand stdin to the EventStream,
    // and signal that the second fragment may be written.
    wait_for_substring(&mut raw, &rx, &mut *child, HANDSHAKE, "READY handshake");
    // Paced writes: a single >~2 KB master write outruns the slave's input
    // queue and is truncated by the kernel (real terminals pace their output
    // the same way), so bulk phases are written in small chunks.
    write_paced(&mut writer, phase1);

    if answer_requery {
        // The startup batch contains PROBE_QUERY once; the requery writes it
        // again. Answer the second occurrence with a dark background reply.
        wait_for_occurrences(&mut raw, &rx, &mut *child, PROBE_QUERY, 2, "requery query");
        write_paced(&mut writer, b"\x1b]11;rgb:0000/0000/0000\x07");
    }

    // Wait for the classified result.
    wait_for_substring(&mut raw, &rx, &mut *child, RESULT_PREFIX, "DARK= result");

    drop(writer);

    // Poll the child with a short deadline and force-kill if it did not exit
    // cleanly (the fixture should shut down after writing DARK=...).
    let kill_deadline = Instant::now() + CHILD_KILL_TIMEOUT;
    while child.try_wait().ok().flatten().is_none() && Instant::now() < kill_deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();

    let _ = reader_thread.join();

    while let Ok(chunk) = rx.try_recv() {
        raw.extend_from_slice(&chunk);
    }
    raw
}

/// Write in small chunks with tiny gaps so the pty's input queue is never
/// asked to absorb a burst larger than the kernel delivers to a reader that
/// is not yet draining.
fn write_paced(writer: &mut dyn Write, payload: &[u8]) {
    for chunk in payload.chunks(64) {
        writer
            .write_all(chunk)
            .unwrap_or_else(|err| panic!("paced write: {err}"));
        writer
            .flush()
            .unwrap_or_else(|err| panic!("paced flush: {err}"));
        // 1 ms: enough gap for the pty line discipline, fast enough that a
        // multi-hundred-byte phase lands well inside the fixture's windows.
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_substring(
    raw: &mut Vec<u8>,
    rx: &mpsc::Receiver<Vec<u8>>,
    child: &mut dyn Child,
    needle: &[u8],
    what: &str,
) {
    let started = Instant::now();
    while started.elapsed() < HARD_TIMEOUT {
        loop {
            match rx.try_recv() {
                Ok(chunk) => raw.extend_from_slice(&chunk),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        if find_subslice(raw, needle).is_some() {
            return;
        }
        if child.try_wait().ok().flatten().is_some() {
            // No needle and child exited: any further output is already in `raw`.
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "timeout waiting for {what}; raw_len={} head={:?}",
        raw.len(),
        String::from_utf8_lossy(&raw[..raw.len().min(200)])
    );
}

fn wait_for_occurrences(
    raw: &mut Vec<u8>,
    rx: &mpsc::Receiver<Vec<u8>>,
    child: &mut dyn Child,
    needle: &[u8],
    count: usize,
    what: &str,
) {
    let deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        let hits = raw.windows(needle.len()).filter(|w| *w == needle).count();
        if hits >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (occurrence {count})"
        );
        let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) else {
            assert!(
                child.try_wait().ok().flatten().is_none(),
                "fixture exited early while waiting for {what}"
            );
            continue;
        };
        raw.extend_from_slice(&chunk);
    }
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
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_osc11_handoff_fixture") {
        return PathBuf::from(path);
    }
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
    PathBuf::from(target)
        .join("debug")
        .join("pi_tui_osc11_handoff_fixture")
}
