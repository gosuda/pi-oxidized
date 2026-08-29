//! TUI-P4 static-frame spinner prototype fixture (issue #84).
//!
//! Exercises the real `Loader` component in two configurations — static
//! (single-frame indicator: kind label + elapsed counter, no animation) and
//! animated (default 10-frame braille) — through the same tick cadence under
//! load, and emits evidence markers that the PTY test harness parses to
//! verify the three invariants:
//!
//! 1. **Static-sufficiency** — the kind label + elapsed counter is rendered
//!    by the real `Loader::render` into a ratatui `Buffer` at sampled ticks;
//!    the drawn text is emitted as evidence and asserted by the test.
//! 2. **Anti-chatter** — `Loader::advance` returns `false` for the
//!    single-frame indicator, so no frame-animation repaint is requested.
//! 3. **Tick repaint-suppression** — under load, sub-second ticks do not
//!    trigger repaints when neither the frame nor the elapsed counter
//!    changed. This is validated against simulated per-configuration logic
//!    (frame modulus = indicator's frame count, not the shipped hardcoded
//!    mod-10) that TUI-T11 (#78) would implement; the current
//!    `tick_status_indicator` always cycles mod 10, so this simulation
//!    represents the target behavior, not the current code.
//!
//! The fixture runs under a real PTY (spawned by the test harness) but does
//! not use the `Tui` rendering pipeline — the `Loader::advance` and
//! `Loader::render` logic is pure and does not require a terminal.
//! Evidence markers are written to stdout after the run, prefixed
//! `EVIDENCE:` so the test harness can parse them.
//!
//! Run:
//!   cargo run -p pi-tui --bin pi_tui_static_frame_fixture

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use pi_tui::component::Component;
use pi_tui::components::{Loader, LoaderIndicatorOptions};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Spinner tick cadence; matches `DEFAULT_INTERVAL_MS` and the runtime's
/// `SPINNER_TICK`.
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Number of ticks to simulate under load.
const TICK_COUNT: usize = 200;

/// Kind label for the static-frame status indicator.
const KIND_LABEL: &str = "Working";

/// Render width for Buffer-based visibility evidence.
const RENDER_WIDTH: u16 = 80;

/// Ticks at which to sample the rendered output for static-sufficiency
/// evidence: tick 0 (elapsed 0s, counter suppressed), tick 12 (~1s),
/// tick 100 (~8s), and the final tick (16s).
const SAMPLE_TICKS: &[usize] = &[0, 12, 100, TICK_COUNT - 1];

fn id(s: &str) -> String {
    s.to_owned()
}

/// Render a component into a Buffer and return the visible text of the
/// first non-blank row.
fn render_to_text(component: &mut dyn Component, width: u16) -> String {
    let height = component.measure(width).max(1);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    component.render(area, &mut buf);
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            let cell = buf[(x, y)].symbol();
            if !cell.is_empty() {
                line.push_str(cell);
            }
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    String::new()
}

/// Build the status message the way the real renderer does: suppress the
/// elapsed counter at 0 seconds (status.rs:42-46).
fn status_message(kind: &str, elapsed_secs: u64) -> String {
    if elapsed_secs == 0 {
        kind.to_owned()
    } else {
        format!("{kind} {elapsed_secs}s")
    }
}

fn main() -> ExitCode {
    let frames_len = pi_tui::components::DEFAULT_LOADER_FRAMES.len();

    // Build the static-frame loader (single-frame indicator).
    let mut static_loader = Loader::new(
        id,
        id,
        status_message(KIND_LABEL, 0),
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["●".into()]),
            interval_ms: Some(80),
        }),
    );
    static_loader.start();

    // Build the animated loader (default 10-frame braille).
    let mut animated_loader = Loader::new(id, id, status_message(KIND_LABEL, 0), None);
    animated_loader.start();

    // Synthetic clock for deterministic ticks.
    let t0 = Instant::now();
    let mut tick_instant = t0;

    // Counters for evidence.
    let mut static_advance_true: usize = 0;
    let mut animated_advance_true: usize = 0;
    let mut status_static_repaints: usize = 0;
    let mut status_animated_repaints: usize = 0;
    let mut status_static_frame: usize = 0;
    let mut status_animated_frame: usize = 0;
    let mut status_elapsed: u64 = 0;

    // Collected rendered text samples for static-sufficiency evidence.
    let mut static_samples: Vec<(usize, String)> = Vec::new();

    for tick in 0..TICK_COUNT {
        tick_instant += SPINNER_TICK;

        // Drive the Loader components.
        if static_loader.advance(tick_instant) {
            static_advance_true += 1;
        }
        if animated_loader.advance(tick_instant) {
            animated_advance_true += 1;
        }

        // Compute the new elapsed seconds.
        let new_elapsed = (tick + 1) as u64 * SPINNER_TICK.as_millis() as u64 / 1000;

        // Capture the previous elapsed before any mutation, mirroring the
        // real tick_status_indicator's per-status elapsed_secs.
        let prev_elapsed = status_elapsed;

        // Simulate tick_status_indicator for the static path (1-frame spinner).
        // The static path cycles through 1 frame, so the frame never changes.
        // This uses the indicator's actual frame count (1), not the shipped
        // hardcoded mod-10 — see module docs for the qualification.
        let new_static_frame = 0; // single-frame spinner: frame never changes
        let static_status_changed =
            new_static_frame != status_static_frame || new_elapsed != prev_elapsed;
        if static_status_changed {
            status_static_repaints += 1;
        }
        status_static_frame = new_static_frame;
        status_elapsed = new_elapsed;

        // Simulate tick_status_indicator for the animated path (10-frame spinner).
        // Uses prev_elapsed (captured before mutation) so the elapsed-boundary
        // disjunct is not dead code.
        let new_animated_frame = (status_animated_frame + 1) % frames_len;
        let animated_status_changed =
            new_animated_frame != status_animated_frame || new_elapsed != prev_elapsed;
        if animated_status_changed {
            status_animated_repaints += 1;
        }
        status_animated_frame = new_animated_frame;

        // Update the elapsed counter in both loaders' messages, matching the
        // real renderer's 0s-suppression.
        let msg = status_message(KIND_LABEL, new_elapsed);
        static_loader.set_message(msg.clone());
        animated_loader.set_message(msg);

        // Sample rendered output for static-sufficiency evidence.
        if SAMPLE_TICKS.contains(&tick) {
            let rendered = render_to_text(&mut static_loader, RENDER_WIDTH);
            static_samples.push((tick, rendered));
        }
    }

    static_loader.stop();
    animated_loader.stop();

    let total_elapsed_secs = TICK_COUNT as u64 * SPINNER_TICK.as_millis() as u64 / 1000;

    // Emit evidence markers to stdout.
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let emit = |lock: &mut std::io::StdoutLock<'_>,
                key: &str,
                value: &str|
     -> std::io::Result<()> { writeln!(lock, "EVIDENCE:{key}={value}") };

    let _ = emit(&mut lock, "static_ticks", &TICK_COUNT.to_string());
    let _ = emit(
        &mut lock,
        "static_advance_true",
        &static_advance_true.to_string(),
    );
    let _ = emit(
        &mut lock,
        "static_frame_index",
        &static_loader.frame_index().to_string(),
    );
    let _ = emit(&mut lock, "animated_ticks", &TICK_COUNT.to_string());
    let _ = emit(
        &mut lock,
        "animated_advance_true",
        &animated_advance_true.to_string(),
    );
    let _ = emit(
        &mut lock,
        "animated_frame_index",
        &animated_loader.frame_index().to_string(),
    );
    let _ = emit(
        &mut lock,
        "status_static_repaints",
        &status_static_repaints.to_string(),
    );
    let _ = emit(
        &mut lock,
        "status_animated_repaints",
        &status_animated_repaints.to_string(),
    );
    let _ = emit(&mut lock, "kind_label", KIND_LABEL);
    let _ = emit(
        &mut lock,
        "total_elapsed_secs",
        &total_elapsed_secs.to_string(),
    );
    let _ = emit(&mut lock, "static_frame_count", "1");
    let _ = emit(&mut lock, "animated_frame_count", &frames_len.to_string());

    // Emit rendered text samples for static-sufficiency evidence. Each
    // sample key includes the tick number so they don't collide.
    for (tick, text) in &static_samples {
        let _ = writeln!(lock, "EVIDENCE:static_render_sample_{tick}={text}");
    }

    let _ = emit(&mut lock, "COMPLETE", "");
    let _ = lock.flush();

    ExitCode::SUCCESS
}
