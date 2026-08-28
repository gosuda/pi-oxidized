#![allow(
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    dead_code
)]
//! TUI-P4 static-frame spinner prototype evidence (issue #84).
//!
//! Spawns the `pi_tui_static_frame_fixture` binary under a real PTY, reads
//! the evidence markers from the output stream, and asserts the three
//! invariants that the static-frame path must preserve under load:
//!
//! 1. **Static-sufficiency** — the kind label + elapsed counter is rendered
//!    by the real `Loader::render` into a ratatui `Buffer` at sampled ticks;
//!    the drawn text is emitted as evidence and asserted here.
//! 2. **Anti-chatter** — `Loader::advance` returns `false` for single-frame
//!    indicator (zero frame-animation repaints).
//! 3. **Tick repaint-suppression** — under load, sub-second ticks do not
//!    trigger repaints; only elapsed-second boundary changes do. This is
//!    validated against simulated per-configuration logic (frame modulus =
//!    indicator's frame count) that TUI-T11 (#78) would implement; the
//!    current `tick_status_indicator` always cycles mod 10.
//!
//! This is evidence for TUI-G1 (#49) and TUI-T11 (#78); no settings change
//! lands here.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const HARD_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn static_frame_preserves_three_invariants_under_load() {
    let evidence = drive_static_frame_fixture();

    // 1. Static-sufficiency: kind label and elapsed counter are present in
    //    the rendered output at sampled ticks.
    let kind = evidence.get("kind_label").expect("missing kind_label");
    assert!(
        !kind.is_empty(),
        "static-sufficiency: kind label must be non-empty, got {kind:?}"
    );
    let total_elapsed: u64 = evidence
        .get("total_elapsed_secs")
        .expect("missing total_elapsed_secs")
        .parse()
        .expect("total_elapsed_secs not a number");
    assert!(
        total_elapsed > 0,
        "static-sufficiency: elapsed counter must advance past 0, got {total_elapsed}"
    );

    // Verify rendered text samples contain the kind label and (for non-zero
    // elapsed) the elapsed counter. The real renderer suppresses the counter
    // at 0s, so tick 0 should show only the kind label.
    let static_samples: Vec<(&str, &str)> = evidence
        .iter()
        .filter(|(k, _)| {
            k.starts_with("static_render_sample_")
                && k["static_render_sample_".len()..]
                    .chars()
                    .all(|c| c.is_ascii_digit())
        })
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert!(
        !static_samples.is_empty(),
        "static-sufficiency: expected at least one rendered text sample"
    );
    for (key, text) in &static_samples {
        assert!(
            text.contains(kind),
            "static-sufficiency: rendered text at {key} must contain kind label {kind:?}, got {text:?}"
        );
        // Extract the tick number from the key.
        let tick_str = key.strip_prefix("static_render_sample_").unwrap_or("");
        let tick: usize = tick_str.parse().unwrap_or(0);
        let elapsed_at_tick = (tick + 1) as u64 * 80 / 1000;
        if elapsed_at_tick > 0 {
            assert!(
                text.contains(&format!("{elapsed_at_tick}s")),
                "static-sufficiency: rendered text at {key} must contain elapsed counter {elapsed_at_tick}s, got {text:?}"
            );
        }
    }

    // 2. Anti-chatter: single-frame Loader::advance never returns true.
    let static_advance_true: usize = evidence
        .get("static_advance_true")
        .expect("missing static_advance_true")
        .parse()
        .expect("static_advance_true not a number");
    assert_eq!(
        static_advance_true, 0,
        "anti-chatter: single-frame Loader::advance must never return true, got {static_advance_true}"
    );

    // The animated loader should have advance_true > 0 (sanity check that
    // the test is actually exercising the animation path).
    let animated_advance_true: usize = evidence
        .get("animated_advance_true")
        .expect("missing animated_advance_true")
        .parse()
        .expect("animated_advance_true not a number");
    assert!(
        animated_advance_true > 0,
        "sanity: animated loader should have frame changes, got {animated_advance_true}"
    );

    // 3. Tick repaint-suppression: status-level repaints for the static path
    //    equal the number of elapsed-second boundary crossings, not the
    //    total tick count. This is simulated per-configuration logic (frame
    //    modulus = indicator's frame count) that TUI-T11 would implement.
    let status_static_repaints: usize = evidence
        .get("status_static_repaints")
        .expect("missing status_static_repaints")
        .parse()
        .expect("status_static_repaints not a number");
    let status_animated_repaints: usize = evidence
        .get("status_animated_repaints")
        .expect("missing status_animated_repaints")
        .parse()
        .expect("status_animated_repaints not a number");
    let static_ticks: usize = evidence
        .get("static_ticks")
        .expect("missing static_ticks")
        .parse()
        .expect("static_ticks not a number");

    // The static path should have far fewer repaints than ticks (only
    // elapsed-second boundaries trigger repaints).
    assert!(
        status_static_repaints < static_ticks,
        "tick repaint-suppression: static repaints ({status_static_repaints}) must be < ticks ({static_ticks})"
    );
    // The static repaints should equal the total elapsed seconds (each
    // second boundary triggers one repaint).
    assert_eq!(
        status_static_repaints, total_elapsed as usize,
        "tick repaint-suppression: static repaints ({status_static_repaints}) must equal elapsed-second boundaries ({total_elapsed})"
    );
    // The animated path should have repaints on every tick (frame changes
    // every tick).
    assert_eq!(
        status_animated_repaints, static_ticks,
        "tick repaint-suppression: animated repaints ({status_animated_repaints}) must equal ticks ({static_ticks}) — every tick changes the frame"
    );

    // Verify the COMPLETE marker was emitted.
    assert!(
        evidence.contains_key("COMPLETE"),
        "evidence stream must end with EVIDENCE:COMPLETE"
    );
}

/// Drive the static-frame fixture through a real PTY and parse evidence.
fn drive_static_frame_fixture() -> HashMap<String, String> {
    let binary = static_frame_fixture_binary();
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
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .unwrap_or_else(|err| panic!("spawn fixture failed: {err}"));
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .unwrap_or_else(|err| panic!("pty reader: {err}"));

    // No probe reply needed — the simplified fixture does not use the Tui
    // pipeline and does not read stdin.

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

    // Wait for the child to finish, with a hard timeout to avoid hanging
    // the test forever on a wedged fixture.
    let wait_deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > wait_deadline {
                    let _ = child.kill();
                    panic!("fixture did not exit within {HARD_TIMEOUT:?}");
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("child wait failed: {err}"),
        }
    }

    // Drain any remaining data from the channel.
    let mut raw = Vec::new();
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(500)) {
        raw.extend_from_slice(&chunk);
    }
    let _ = reader_thread.join();

    // Parse EVIDENCE markers from the raw output. Use `find` instead of
    // `strip_prefix` because the PTY may echo bytes that concatenate with
    // the first EVIDENCE line.
    let text = String::from_utf8_lossy(&raw);
    let mut evidence = HashMap::new();
    for line in text.lines() {
        if let Some(pos) = line.find("EVIDENCE:") {
            let rest = &line[pos + "EVIDENCE:".len()..];
            if let Some((key, value)) = rest.split_once('=') {
                evidence.insert(key.to_owned(), value.trim().to_owned());
            } else {
                // Handle markers without '=' (like COMPLETE).
                evidence.insert(rest.trim().to_owned(), String::new());
            }
        }
    }

    evidence
}

// Re-export Instant for the wait deadline.
use std::time::Instant;

fn static_frame_fixture_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_static_frame_fixture") {
        return PathBuf::from(path);
    }
    let mut candidates = Vec::new();
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        candidates.push(PathBuf::from(target));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    candidates.push(PathBuf::from("target"));
    let bin_name = static_frame_bin_name();
    for root in &candidates {
        for profile in ["debug", "release"] {
            let path = root.join(profile).join(bin_name);
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
            "pi_tui_static_frame_fixture",
            "--quiet",
        ])
        .status()
        .unwrap_or_else(|err| panic!("failed to build fixture: {err}"));
    assert!(status.success(), "fixture build failed");
    // Re-scan candidate roots after build (handles CARGO_TARGET_DIR).
    for root in &candidates {
        for profile in ["debug", "release"] {
            let path = root.join(profile).join(bin_name);
            if path.exists() {
                return path;
            }
        }
    }
    panic!(
        "fixture binary missing after build; searched: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn static_frame_bin_name() -> &'static str {
    if cfg!(windows) {
        "pi_tui_static_frame_fixture.exe"
    } else {
        "pi_tui_static_frame_fixture"
    }
}
