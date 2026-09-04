//! Regression: input written after the first frame must paint through the
//! production `EventStream` parser (issue: post-first-frame paint stall).
//!
//! The startup capability probe once owned stdin past the first frame: a
//! bracketed paste written as soon as the first frame was observed landed in
//! the byte-level probe collector and was re-injected through the lossy
//! startup mapper — `ESC[200~` became `Esc` + literal `[200~` cells and the
//! marker's `Esc` cleared the editor, so the paste text never survived on
//! screen. The collector now yields stdin before the frame paints, so the
//! paste is parsed by crossterm and painted as one synchronized transaction.
//!
//! Drives the real binary through a PTY (T33 contract: /quit is the success
//! path; the final close is cleanup only).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pi_tui::testkit::CapabilityProfile;
use pi_tui::testkit::driver::{
    DriverError, DriverSession, Geometry, LaunchSpec, RenderSession, SettlePolicy, TerminalDriver,
};
use pi_tui::testkit::posix::PosixPtyDriver;
use tempfile::TempDir;

const PASTE_LABEL: &str = "check 9 paste-echo";
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

fn pi_binary() -> Result<PathBuf, DriverError> {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(DriverError::pty(&format!(
            "product prerequisite missing: CARGO_BIN_EXE_pi points at missing binary {}; rebuild with cargo test -p pi --test startup_paste_echo",
            path.display()
        )))
    }
}

#[expect(
    clippy::expect_used,
    reason = "test setup and assertions: binary, sandbox, PTY session, frame reads, and settle policies must all succeed"
)]
#[expect(
    clippy::panic,
    reason = "test assertion: paste echo read failure is a test failure signal"
)]
#[test]
fn paste_after_first_frame_paints_label_as_synchronized_text() {
    let binary = pi_binary().expect("pi binary");
    let sandbox = TempDir::new().expect("sandbox");
    let home = sandbox.path().join("home");
    let agent = sandbox.path().join("agent");
    let sessions = sandbox.path().join("sessions");
    for directory in [&home, &agent, &sessions] {
        fs::create_dir_all(directory).expect("sandbox dirs");
    }

    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), home.to_string_lossy().into_owned());
    env.insert(
        "PI_CODING_AGENT_DIR".to_owned(),
        agent.to_string_lossy().into_owned(),
    );
    env.insert(
        "PI_CODING_AGENT_SESSION_DIR".to_owned(),
        sessions.to_string_lossy().into_owned(),
    );
    env.insert("PI_OFFLINE".to_owned(), "1".to_owned());
    env.insert("PI_SKIP_VERSION_CHECK".to_owned(), "1".to_owned());

    let spec = LaunchSpec {
        argv: vec![
            binary.to_string_lossy().into_owned(),
            "--provider".to_owned(),
            "anthropic".to_owned(),
            "--model".to_owned(),
            "claude-sonnet-4-5".to_owned(),
            "--api-key".to_owned(),
            "verification-no-network".to_owned(),
            "--no-extensions".to_owned(),
            "--no-session".to_owned(),
            "--offline".to_owned(),
            "--no-context-files".to_owned(),
            "--no-skills".to_owned(),
            "--no-prompt-templates".to_owned(),
            "--no-themes".to_owned(),
            "--approve".to_owned(),
        ],
        cwd: sandbox.path().to_path_buf(),
        env,
        geometry: Geometry::new(100, 32).expect("geometry"),
        // Dumb: no canned probe replies, so the DSR responder's CPR-only
        // answer keeps the probe collector's fragment window open past the
        // first frame — exactly the harness condition that corrupted input.
        profile: CapabilityProfile::Dumb,
    };
    let mut session = PosixPtyDriver.open(&spec).expect("pty session");

    // 1. First frame: first balanced DEC 2026 transaction.
    let frame_policy = SettlePolicy::new(Duration::from_millis(80), Duration::from_secs(20))
        .expect("settle policy");
    session
        .read_output(&frame_policy, |bytes| {
            let begin = bytes
                .windows(SYNC_BEGIN.len())
                .any(|window| window == SYNC_BEGIN);
            let end = bytes
                .windows(SYNC_END.len())
                .any(|window| window == SYNC_END);
            begin && end
        })
        .expect("first synchronized frame");

    // 2. Paste immediately after the first frame — exactly the harness timing
    //    that corrupted input when the probe collector still owned stdin.
    session
        .write(format!("\x1b[200~{PASTE_LABEL}\x1b[201~").as_bytes())
        .expect("paste write");

    // 3. The paste must paint the label into the editor and the settled
    //    screen must keep it. Damage-diff painting splits the label across
    //    escape sequences, so the trigger predicate only requires ANY output;
    //    the label assertion runs on the AVT-rendered screen, and the
    //    paste-marker bytes must never leak into it as literal cells.
    let paint_policy = SettlePolicy::new(Duration::from_millis(300), Duration::from_secs(2))
        .expect("settle policy");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut label_painted = false;
    while !label_painted && Instant::now() < deadline {
        let frame = match session.read_settled_frame(&paint_policy, |_| true) {
            Ok(frame) => frame,
            // No bytes within the ceiling: keep polling until the deadline.
            Err(DriverError::SettleCeiling(_)) => continue,
            Err(error) => panic!("paste echo read failed: {error}"),
        };
        let screen = frame.snapshot.lines.join("\n");
        label_painted = screen.contains(PASTE_LABEL);
        if label_painted {
            assert!(
                !screen.contains("[200~"),
                "paste marker leaked as literal cells; screen: {screen:?}"
            );
        }
    }
    assert!(
        label_painted,
        "paste written after the first frame never painted its label through the EventStream parser"
    );

    // 4. Polite exit: clear the pasted editor (Ctrl+U) so "/quit" reaches the
    //    command bar, then quit; close() waits on the child once it exits.
    let _ = session.write(b"\x15");
    let clear_policy =
        SettlePolicy::new(Duration::from_millis(100), Duration::from_secs(5)).expect("quit policy");
    let _ = session.read_output(&clear_policy, |_| true);
    let _ = session.write(b"/quit\r");
    let quit_policy =
        SettlePolicy::new(Duration::from_millis(100), Duration::from_secs(5)).expect("quit policy");
    let _ = session.read_output(&quit_policy, |_| true);
    let _ = session.close();
}
