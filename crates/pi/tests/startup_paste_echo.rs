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
use pi_tui::testkit::posix::{PosixPtyDriver, PosixPtySession};
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

/// Shared product harness: isolated sandbox, offline flags, and the real
/// binary behind a Posix PTY with the dumb capability profile (no canned
/// probe replies). The caller must keep the returned sandbox alive for as
/// long as the session.
fn launch_offline_pi_session() -> Result<(TempDir, PosixPtySession), DriverError> {
    let binary = pi_binary()?;
    let sandbox = TempDir::new().map_err(|error| DriverError::pty(&error))?;
    let home = sandbox.path().join("home");
    let agent = sandbox.path().join("agent");
    let sessions = sandbox.path().join("sessions");
    for directory in [&home, &agent, &sessions] {
        fs::create_dir_all(directory).map_err(|error| DriverError::pty(&error))?;
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
        geometry: Geometry::new(100, 32)?,
        // Dumb: no canned probe replies, so the DSR responder's CPR-only
        // answer keeps the probe collector's fragment window open past the
        // first frame — exactly the harness condition that corrupted input.
        // Canonical CSI-u press/repeat/release forms parse regardless of the
        // negotiated profile.
        profile: CapabilityProfile::Dumb,
    };
    let session = PosixPtyDriver.open(&spec)?;
    Ok((sandbox, session))
}

#[expect(
    clippy::expect_used,
    reason = "test setup and assertions: binary, sandbox, PTY session, frame reads, and settle policies must all succeed"
)]
#[test]
fn paste_after_first_frame_paints_label_as_synchronized_text() {
    // Shared harness (sandbox must outlive the session):
    let (_sandbox, mut session) = launch_offline_pi_session().expect("pty session");

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
    let (label_painted, _) = poll_settled_screen(&mut session, "paste echo", |screen| {
        let painted = screen.contains(PASTE_LABEL);
        if painted {
            assert!(
                !screen.contains("[200~"),
                "paste marker leaked as literal cells; screen: {screen:?}"
            );
        }
        painted
    });
    assert!(
        label_painted,
        "paste written after the first frame never painted its label through the EventStream parser"
    );

    // 4. Polite exit.
    quit_session(session);
}

/// Regression: default product consumers must reject key Release events
/// while Press and Repeat keep editing.
///
/// Phase 1 drives the real `EventStream` parser with Press p, Release p,
/// Release x (never pressed), then Press q. The pre-guard runtime lets the
/// default editor act on both releases, so the composer settles to "❯ ppxq";
/// the fixed runtime boundary must reject Release for default consumers and
/// settle to "❯ pq". Phase 2 clears the editor and sends Press p, Repeat p,
/// Repeat p, Press q — the settled line must be "❯ pppq", proving the guard
/// preserves repeats.
///
/// Each phase waits for a settled frame containing its q fence before
/// asserting: a correctly ignored Release may paint nothing, and a snapshot
/// taken before the fence could pass before the Release was processed. One
/// write per event keeps the canonical CSI-u stream order: Press
/// `ESC[112;1:1u`, Repeat `ESC[112;1:2u`, Release `ESC[112;1:3u`.
#[expect(
    clippy::expect_used,
    reason = "test setup and assertions: shared PTY session, frame reads, and settle policies must all succeed"
)]
#[test]
fn product_key_releases_do_not_edit_but_repeats_do() {
    let (_sandbox, mut session) = launch_offline_pi_session().expect("pty session");

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

    // 2. Phase 1: Press p, Release p, Release x (never pressed), Press q.
    //    Releases must not edit: the composer must settle to "❯ pq", not the
    //    pre-guard defect "❯ ppxq".
    session.write(b"\x1b[112;1:1u").expect("write press p");
    session.write(b"\x1b[112;1:3u").expect("write release p");
    session
        .write(b"\x1b[120;1:3u")
        .expect("write release x (never pressed)");
    session
        .write(b"\x1b[113;1:1u")
        .expect("write press q fence");
    wait_for_composer_line(&mut session, "pq");

    // 3. Phase 2: Ctrl+U clears, then Press p, Repeat p, Repeat p, Press q.
    //    Repeats must keep editing: the composer must settle to "❯ pppq".
    session.write(b"\x15").expect("write ctrl+u clear");
    session
        .write(b"\x1b[112;1:1u")
        .expect("write phase-2 press p");
    session
        .write(b"\x1b[112;1:2u")
        .expect("write phase-2 repeat p");
    session
        .write(b"\x1b[112;1:2u")
        .expect("write phase-2 repeat p (second)");
    session
        .write(b"\x1b[113;1:1u")
        .expect("write phase-2 press q fence");
    wait_for_composer_line(&mut session, "pppq");

    // 4. Polite exit.
    quit_session(session);
}

/// Poll settled frames until `predicate` accepts the joined screen text,
/// returning `(settled, last_screen)`. A quiet ceiling means no bytes
/// arrived, so the poll continues to the deadline instead of failing.
#[expect(
    clippy::expect_used,
    reason = "test helper: settle policy construction must succeed"
)]
#[expect(
    clippy::panic,
    reason = "test assertion: a real read failure is a test failure signal"
)]
fn poll_settled_screen(
    session: &mut PosixPtySession,
    what: &str,
    mut predicate: impl FnMut(&str) -> bool,
) -> (bool, String) {
    let paint_policy = SettlePolicy::new(Duration::from_millis(300), Duration::from_secs(2))
        .expect("settle policy");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_screen = String::new();
    let mut settled = false;
    while !settled && Instant::now() < deadline {
        let frame = match session.read_settled_frame(&paint_policy, |_| true) {
            Ok(frame) => frame,
            // No bytes within the ceiling: keep polling until the deadline.
            Err(DriverError::SettleCeiling(_)) => continue,
            Err(error) => panic!("{what} read failed: {error}"),
        };
        last_screen = frame.snapshot.lines.join("\n");
        settled = predicate(&last_screen);
    }
    (settled, last_screen)
}

/// Polite exit: clear the editor (Ctrl+U) so "/quit" reaches the command
/// bar, then quit; `close()` waits on the child once it exits.
#[expect(
    clippy::expect_used,
    reason = "test helper: settle policy construction must succeed"
)]
fn quit_session(mut session: PosixPtySession) {
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

/// Poll settled frames until one shows a composer line whose editor content
/// (everything after the `❯` prompt, with layout padding stripped) equals
/// `expected` exactly, panicking with the last screen at the deadline. A
/// correctly ignored Release paints nothing, so quiet ceilings keep polling
/// rather than failing; only the fence result satisfies the wait.
fn wait_for_composer_line(session: &mut PosixPtySession, expected: &str) {
    let (settled, last_screen) = poll_settled_screen(session, "composer", |screen| {
        screen.lines().any(|line| {
            line.trim()
                .strip_prefix('❯')
                .is_some_and(|editor| editor.trim() == expected)
        })
    });
    assert!(
        settled,
        "composer never settled to {expected:?}; last screen:\n{last_screen}"
    );
}
