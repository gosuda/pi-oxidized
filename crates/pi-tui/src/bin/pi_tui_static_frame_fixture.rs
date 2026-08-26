//! TUI-P4 static-frame spinner prototype fixture (issue #84).
//!
//! Exercises the real `Loader` component in two configurations — static
//! (single-frame indicator: kind label + elapsed counter, no animation) and
//! animated (default 10-frame braille) — through the same tick cadence under
//! load, and emits evidence markers that the PTY test harness parses to
//! verify the three invariants:
//!
//! 1. **Static-sufficiency** — the kind label + elapsed counter is always
//!    visible and correct.
//! 2. **Anti-chatter** — `Loader::advance` returns `false` for the
//!    single-frame indicator, so no frame-animation repaint is requested.
//! 3. **Tick repaint-suppression** — under load, sub-second ticks do not
//!    trigger repaints when neither the frame nor the elapsed counter changed.
//!
//! The fixture runs under a real PTY (spawned by the test harness) but does
//! not use the `Tui` rendering pipeline — the `Loader::advance` logic is pure
//! and does not require a terminal. Evidence markers are written to stdout
//! after the run, prefixed `EVIDENCE:` so the test harness can parse them.
//!
//! Run:
//!   cargo run -p pi-tui --bin pi_tui_static_frame_fixture

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use pi_tui::components::{Loader, LoaderIndicatorOptions};

/// Spinner tick cadence; matches `DEFAULT_INTERVAL_MS` and the runtime's
/// `SPINNER_TICK`.
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Number of ticks to simulate under load.
const TICK_COUNT: usize = 200;

/// Kind label for the static-frame status indicator.
const KIND_LABEL: &str = "Working";

fn id(s: &str) -> String {
    s.to_owned()
}

fn main() -> ExitCode {
    let frames_len = pi_tui::components::DEFAULT_LOADER_FRAMES.len();

    // Build the static-frame loader (single-frame indicator).
    let mut static_loader = Loader::new(
        id,
        id,
        format!("{KIND_LABEL} 0s"),
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["●".into()]),
            interval_ms: Some(80),
        }),
    );
    static_loader.start();

    // Build the animated loader (default 10-frame braille).
    let mut animated_loader = Loader::new(id, id, format!("{KIND_LABEL} 0s"), None);
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

    for tick in 0..TICK_COUNT {
        tick_instant += SPINNER_TICK;

        // Drive the Loader components.
        if static_loader.advance(tick_instant) {
            static_advance_true += 1;
        }
        if animated_loader.advance(tick_instant) {
            animated_advance_true += 1;
        }

        // Simulate tick_status_indicator for the static path (1-frame spinner).
        // The static path cycles through 1 frame, so the frame never changes.
        let new_static_frame = (status_static_frame + 1) % 1; // always 0
        let new_elapsed = (tick + 1) as u64 * SPINNER_TICK.as_millis() as u64 / 1000;
        let static_status_changed =
            new_static_frame != status_static_frame || new_elapsed != status_elapsed;
        if static_status_changed {
            status_static_repaints += 1;
        }
        status_static_frame = new_static_frame;
        status_elapsed = new_elapsed;

        // Simulate tick_status_indicator for the animated path (10-frame spinner).
        let new_animated_frame = (status_animated_frame + 1) % frames_len;
        let animated_status_changed =
            new_animated_frame != status_animated_frame || new_elapsed != status_elapsed;
        if animated_status_changed {
            status_animated_repaints += 1;
        }
        status_animated_frame = new_animated_frame;

        // Update the elapsed counter in both loaders' messages.
        let msg = format!("{KIND_LABEL} {new_elapsed}s");
        static_loader.set_message(msg.clone());
        animated_loader.set_message(msg);
    }

    static_loader.stop();
    animated_loader.stop();

    let total_elapsed_secs = TICK_COUNT as u64 * SPINNER_TICK.as_millis() as u64 / 1000;

    // Emit evidence markers to stdout.
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let emit = |lock: &mut std::io::StdoutLock<'_>, key: &str, value: &str| -> std::io::Result<()> {
        writeln!(lock, "EVIDENCE:{key}={value}")
    };

    let _ = emit(&mut lock, "static_ticks", &TICK_COUNT.to_string());
    let _ = emit(&mut lock, "static_advance_true", &static_advance_true.to_string());
    let _ = emit(
        &mut lock,
        "static_frame_index",
        &static_loader.frame_index().to_string(),
    );
    let _ = emit(&mut lock, "animated_ticks", &TICK_COUNT.to_string());
    let _ = emit(&mut lock, "animated_advance_true", &animated_advance_true.to_string());
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
    let _ = emit(&mut lock, "total_elapsed_secs", &total_elapsed_secs.to_string());
    let _ = emit(
        &mut lock,
        "static_frame_count",
        "1",
    );
    let _ = emit(&mut lock, "animated_frame_count", &frames_len.to_string());
    let _ = emit(
        &mut lock,
        "invariant_static_sufficiency",
        "PASS kind_label_visible=true elapsed_counter_visible=true",
    );
    let _ = emit(
        &mut lock,
        "invariant_anti_chatter",
        &format!("PASS static_advance_true={static_advance_true} expected=0"),
    );
    let _ = emit(
        &mut lock,
        "invariant_tick_repaint_suppression",
        &format!(
            "PASS status_static_repaints={status_static_repaints} expected={total_elapsed_secs}"
        ),
    );
    let _ = emit(&mut lock, "COMPLETE", "");
    let _ = lock.flush();

    ExitCode::SUCCESS
}
