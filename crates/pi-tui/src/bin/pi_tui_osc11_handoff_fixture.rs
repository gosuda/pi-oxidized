//! Native PTY fixture for OSC 11 reply handoff across the startup cutoff.
//!
//! Runs the real `TerminalSession` / `TerminalInput` pipeline. The harness
//! splits an OSC 11 reply across the probe / `EventStream` boundary (or
//! drives the other acceptance scenarios: lone ESC expiry, early keys,
//! reply flood, mid-session requery). Everything is collected through the
//! ONE shared crossterm reader: probe replies via `poll_reply` inside the
//! session, post-handoff replies via the bounded sink, and ordinary keys
//! via the regular event stream.
//!
//! Reported protocol (one tag per line):
//! `DARK=` classified background (true/false/unknown)
//! `RECOVERED=` whether a latched reply protocol error was recovered
//! `REQUERY=` mid-session OSC 11 requery classification (true/false/unknown)
//! `KEYS=` ordinary keystrokes observed after `start_input`
//! `ESC=` whether a lone Esc keystroke was observed

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::reply::{
    TerminalReply, drain_replies, is_protocol_error, recover_protocol_error,
};
use pi_tui::component::UiEvent;
use pi_tui::terminal::{TerminalCapabilities, TerminalGuard, TerminalSession, classify_background};
use tokio::time::{sleep, timeout};

const HANDSHAKE: &[u8] = b"READY\n";
const KEY_WINDOW: Duration = Duration::from_millis(150);

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut guard = TerminalGuard::new(io::stdout());
    if let Err(error) = guard.activate(true) {
        eprintln!("terminal activation failed: {error}");
        return ExitCode::from(2);
    }

    let (mut session, mut input) =
        match TerminalSession::begin(guard, true, TerminalCapabilities::default()) {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!("terminal session begin failed: {error}");
                return ExitCode::from(2);
            }
        };

    // Let the harness observe the probe batch and write the first fragment
    // (or early keys) before we arm the probe yield and start the stream.
    sleep(Duration::from_millis(50)).await;

    if let Err(error) = session.finish_probe(TerminalCapabilities::default()).await {
        eprintln!("finish_probe failed: {error}");
        return ExitCode::from(2);
    }

    session.start_input(&mut input);

    let writer = session.guard_mut().writer_mut();
    if writer.write_all(HANDSHAKE).is_err() || writer.flush().is_err() {
        eprintln!("failed to write handshake");
        return ExitCode::from(2);
    }

    // Let the harness complete the split reply (or flood / requery setup)
    // while the EventStream owns the reader.
    sleep(Duration::from_millis(120)).await;

    // Post-handoff replies land in the bounded typed sink via the shared
    // parser; probe-window replies were merged into caps during finish_probe.
    let drained = drain_replies();
    let split_dark = drained.iter().find_map(|reply| match reply {
        TerminalReply::Osc11(payload) => classify_background(payload),
        _ => None,
    });
    // Mid-session requery over the SAME stream (no raw takeover, no fresh
    // parser): pause so the shared reader is quiescent, recover any latched
    // reply protocol error while the reader is free, classify a fresh OSC 11
    // reply, then resume. This is the documented quiescence precondition.
    let mut recovered = false;
    let mut requery = None;
    if input.pause().await.is_ok() {
        recovered = recover_protocol_error();
        match pi_tui::terminal::probe::probe_background(writer) {
            Ok(dark) => requery = dark,
            Err(error) => {
                eprintln!("requery failed (continuing): {error}");
                if is_protocol_error(&error) {
                    recovered = recover_protocol_error();
                }
            }
        }
        let _ = input.resume(Vec::new()).await;
    }

    // Ordinary keystrokes observed after start_input (early keys queued
    // during the probe window are delivered by the event stream in order).
    let mut key_names: Vec<&'static str> = Vec::new();
    loop {
        match timeout(KEY_WINDOW, input.recv()).await {
            Ok(Some(event)) => {
                if let Some(name) = key_name(&event) {
                    key_names.push(name);
                }
            }
            Ok(None) => break,
            Err(_elapsed) => break,
        }
    }

    // Report results: one tag per line, flushed before mode restore.
    let dark_word = |dark: Option<bool>| match dark {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    let esc_seen = key_names.contains(&"<ESC>");
    {
        let writer = session.guard_mut().writer_mut();
        let mut report = |line: String| {
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.write_all(b"\n");
        };
        report(format!("DARK={}", dark_word(split_dark)));
        report(format!("RECOVERED={recovered}"));
        report(format!("REQUERY={}", dark_word(requery)));
        report(format!("KEYS={}", key_names.concat()));
        report(format!("ESC={esc_seen}"));
        let _ = writer.flush();
    }

    let _ = input.pause().await;
    session.shutdown();
    ExitCode::SUCCESS
}

fn key_name(event: &UiEvent) -> Option<&'static str> {
    match event {
        UiEvent::Key(key) => match key.code {
            crossterm::event::KeyCode::Esc => Some("<ESC>"),
            crossterm::event::KeyCode::Enter => Some("<ENTER>"),
            crossterm::event::KeyCode::Left => Some("<LEFT>"),
            crossterm::event::KeyCode::Right => Some("<RIGHT>"),
            crossterm::event::KeyCode::Up => Some("<UP>"),
            crossterm::event::KeyCode::Down => Some("<DOWN>"),
            crossterm::event::KeyCode::Tab => Some("<TAB>"),
            crossterm::event::KeyCode::Backspace => Some("<BS>"),
            crossterm::event::KeyCode::Char(c) if c.is_ascii_graphic() => match c {
                'a' => Some("a"),
                'b' => Some("b"),
                'c' => Some("c"),
                'd' => Some("d"),
                'e' => Some("e"),
                'f' => Some("f"),
                'g' => Some("g"),
                'h' => Some("h"),
                'i' => Some("i"),
                'j' => Some("j"),
                'k' => Some("k"),
                'l' => Some("l"),
                'm' => Some("m"),
                'n' => Some("n"),
                'o' => Some("o"),
                'p' => Some("p"),
                'q' => Some("q"),
                'r' => Some("r"),
                's' => Some("s"),
                't' => Some("t"),
                'u' => Some("u"),
                'v' => Some("v"),
                'w' => Some("w"),
                'x' => Some("x"),
                'y' => Some("y"),
                'z' => Some("z"),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}
