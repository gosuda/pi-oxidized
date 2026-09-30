//! Adversarial E2E gauntlet for the shipped `pi` TUI over a real PTY.
//!
//! Where `tui_keyboard_gauntlet.rs` proves canonical flows through
//! `RecordingSession` digests, this lane hammers the shipped binary with
//! hostile input: bracketed-paste storms split mid-sequence, resize storms
//! mid-stream and mid-dialog, sub-floor geometries, invalid UTF-8 and
//! unterminated CSI/OSC bytes, rapid key floods, stacked-dialog floods,
//! overlay abuse, unicode grapheme clusters, back-to-back submits, and
//! ctrl+d/quit races. The autocomplete flood also exercises the off-loop
//! provider path end to end.
//!
//! A second wave drives multi-vector collisions: queued steer chains, an
//! open paste under a resize storm, Esc aborts mid-stream, a ~40 KiB paste,
//! UTF-8 split across write boundaries, nested paste markers, selector
//! reflow under resizes, boundary-width composer text, and
//! whitespace-only submits.
//!
//! A third wave pushes harder on combined vectors and protocol spoofing:
//! kill-ring/undo/word-motion internals under flood, terminal replies
//! injected as keystrokes (cursor reports, DA/kitty/XTVersion answers, OSC
//! replies), SGR mouse and focus-event floods, key aliasing edges
//! (ctrl+j newline vs ctrl+m submit), a 4 KiB single-line composer,
//! binary control bytes inside bracketed paste, a queued prompt submitted
//! from an open paste mid-stream, a selector opened mid-stream, and a
//! sustained interleaved-everything pressure loop.
//!
//! Assertions are liveness/correctness only (no canonical digests): each
//! scenario must keep the composer responsive, converge the screen to a
//! sane state, and exit 0. Every settle is predicate-then-quiescence —
//! never a bare timer.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pi_tui::testkit::driver::{
    DriverSession, Geometry, LaunchSpec, RenderSession, SettlePolicy, TerminalSnapshot,
};
use pi_tui::testkit::transcript::CapabilityProfile;
use tempfile::TempDir;

#[cfg(windows)]
use pi_tui::testkit::conpty::ConPtyDriver;
#[cfg(unix)]
use pi_tui::testkit::posix::PosixPtyDriver;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const FINAL_MARKER: &str = "PI_VERIFICATION_FINAL_TUI";
const READY_MARKERS: &[&str] = &["type a message", "type a message to begin", "No messages"];
const PROMPT_GLYPH: char = '❯';

const KEY_ENTER: &[u8] = b"\r";
const KEY_ESCAPE: &[u8] = b"\x1b";
const KEY_UP: &[u8] = b"\x1b[A";
const KEY_DOWN: &[u8] = b"\x1b[B";
const KEY_LEFT: &[u8] = b"\x1b[D";
const KEY_RIGHT: &[u8] = b"\x1b[C";
const KEY_HOME: &[u8] = b"\x1b[H";
const KEY_END: &[u8] = b"\x1b[F";
const KEY_SHIFT_TAB: &[u8] = b"\x1b[Z";
const KEY_PAGE_UP: &[u8] = b"\x1b[5~";
const KEY_PAGE_DOWN: &[u8] = b"\x1b[6~";
const KEY_BACKSPACE: &[u8] = b"\x7f";
const KEY_CTRL_D: &[u8] = b"\x04";

const STACKED_OVERLAY_LINE: &str = "Verification overlay-stack state=pending";
const STACKED_DIALOG_TITLE: &str = "Verification stacked select";

#[derive(Debug)]
enum AdvError {
    Prerequisite(String),
    Driver(String),
    Io(String),
    Assert(String),
}

impl std::fmt::Display for AdvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prerequisite(m) | Self::Driver(m) | Self::Io(m) | Self::Assert(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl From<pi_tui::testkit::driver::DriverError> for AdvError {
    fn from(error: pi_tui::testkit::driver::DriverError) -> Self {
        Self::Driver(error.to_string())
    }
}

impl From<std::io::Error> for AdvError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

struct Sandbox {
    _root: TempDir,
    home_dir: PathBuf,
    agent_dir: PathBuf,
    session_dir: PathBuf,
    work_dir: PathBuf,
}

fn workspace_root() -> Result<PathBuf, AdvError> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            AdvError::Prerequisite(format!(
                "workspace root not found above {}",
                manifest.display()
            ))
        })
}

fn pi_binary() -> Result<PathBuf, AdvError> {
    let compiled = PathBuf::from(env!("CARGO_BIN_EXE_pi"));
    if compiled.is_file() {
        return Ok(compiled);
    }
    let fallback = target_root().join("debug").join("pi");
    if fallback.is_file() {
        return Ok(fallback);
    }
    Err(AdvError::Prerequisite(format!(
        "pi binary missing: {} / {}",
        compiled.display(),
        fallback.display()
    )))
}

fn target_root() -> PathBuf {
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

/// Built extension-host binary — `PI_EXTENSION_HOST` must name an existing
/// file, so this shares the platform-aware name with the prerequisite check.
fn extension_host_path() -> Result<PathBuf, AdvError> {
    Ok(workspace_root()?
        .join("packages")
        .join("extension-host")
        .join("dist")
        .join(if cfg!(windows) {
            "pi-extension-host.exe"
        } else {
            "pi-extension-host"
        }))
}

fn require_prerequisites() -> Result<(), AdvError> {
    let _ = pi_binary()?;
    let host = extension_host_path()?;
    if !host.is_file() {
        return Err(AdvError::Prerequisite(format!(
            "extension host missing at {} (build: bun run --cwd packages/extension-host build)",
            host.display()
        )));
    }
    let extension = workspace_root()?.join("scripts/verification/extension.ts");
    if !extension.is_file() {
        return Err(AdvError::Prerequisite(format!(
            "verification extension missing at {}",
            extension.display()
        )));
    }
    Ok(())
}

fn create_sandbox() -> Result<Sandbox, AdvError> {
    let root = TempDir::new()?;
    let home_dir = root.path().join("home");
    let agent_dir = root.path().join("agent");
    let session_dir = root.path().join("sessions");
    let work_dir = root.path().join("work");
    for directory in [&home_dir, &agent_dir, &session_dir, &work_dir] {
        fs::create_dir_all(directory)?;
    }
    Ok(Sandbox {
        _root: root,
        home_dir,
        agent_dir,
        session_dir,
        work_dir,
    })
}

/// Verification launch argv/env: real binary + real extension host +
/// deterministic `verification` provider, no network, no user config.
fn launch_spec(sandbox: &Sandbox) -> Result<LaunchSpec, AdvError> {
    require_prerequisites()?;
    let argv = vec![
        pi_binary()?.to_string_lossy().into_owned(),
        "--provider".to_owned(),
        "verification".to_owned(),
        "--model".to_owned(),
        "model".to_owned(),
        "--api-key".to_owned(),
        "verification-key".to_owned(),
        "--extension".to_owned(),
        workspace_root()?
            .join("scripts/verification/extension.ts")
            .to_string_lossy()
            .into_owned(),
        "--verification-profile".to_owned(),
        "tui-transcript-profile".to_owned(),
        "--offline".to_owned(),
        "--no-context-files".to_owned(),
        "--no-skills".to_owned(),
        "--no-prompt-templates".to_owned(),
        "--no-themes".to_owned(),
        "--approve".to_owned(),
    ];
    let mut env = BTreeMap::new();
    env.insert(
        "HOME".to_owned(),
        sandbox.home_dir.to_string_lossy().into_owned(),
    );
    env.insert("PI_OFFLINE".to_owned(), "1".to_owned());
    env.insert(
        "PI_EXTENSION_HOST".to_owned(),
        extension_host_path()?.to_string_lossy().into_owned(),
    );
    env.insert("PI_VERIFICATION_MODE".to_owned(), "text".to_owned());
    env.insert("PI_VERIFICATION_CHUNK_COUNT".to_owned(), "3".to_owned());
    // Nonzero chunk delay keeps a turn mid-stream for ~450 ms so resize and
    // back-to-back submit scenarios exercise genuine streaming concurrency.
    env.insert(
        "PI_VERIFICATION_CHUNK_DELAY_MS".to_owned(),
        "150".to_owned(),
    );
    env.insert(
        "PI_VERIFICATION_FINAL_MARKER".to_owned(),
        FINAL_MARKER.to_owned(),
    );
    env.insert(
        "PI_CODING_AGENT_DIR".to_owned(),
        sandbox.agent_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "PI_CODING_AGENT_SESSION_DIR".to_owned(),
        sandbox.session_dir.to_string_lossy().into_owned(),
    );
    Ok(LaunchSpec {
        argv,
        cwd: sandbox.work_dir.clone(),
        env,
        geometry: Geometry::new(INITIAL_COLS, INITIAL_ROWS)
            .map_err(|e| AdvError::Prerequisite(format!("geometry: {e}")))?,
        profile: CapabilityProfile::Xterm256ColorTruecolor,
    })
}

#[cfg(unix)]
type HostSession = pi_tui::testkit::posix::PosixPtySession;
#[cfg(windows)]
type HostSession = pi_tui::testkit::conpty::ConPtySession;

fn open_session(spec: &LaunchSpec) -> Result<HostSession, AdvError> {
    #[cfg(unix)]
    {
        Ok(PosixPtyDriver::open_clean(spec)?)
    }
    #[cfg(windows)]
    {
        Ok(ConPtyDriver::open_clean(spec)?)
    }
}

struct ProductRun {
    session: Option<HostSession>,
    policy: SettlePolicy,
}

impl ProductRun {
    fn open(spec: &LaunchSpec) -> Result<Self, AdvError> {
        Ok(Self {
            session: Some(open_session(spec)?),
            policy: SettlePolicy::new(Duration::from_millis(150), Duration::from_secs(45))
                .map_err(|e| AdvError::Prerequisite(format!("settle policy: {e}")))?,
        })
    }

    fn session_mut(&mut self) -> Result<&mut HostSession, AdvError> {
        self.session
            .as_mut()
            .ok_or_else(|| AdvError::Driver("session already closed".to_owned()))
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<(), AdvError> {
        self.session_mut()?.write(bytes)?;
        Ok(())
    }

    /// Widen the quiescence deadline for scenarios that legitimately emit
    //  output for longer than the default policy (e.g. a typed-flood drain).
    fn settle_budget(&mut self, max: Duration) {
        if let Ok(policy) = SettlePolicy::new(self.policy.quiet, max) {
            self.policy = policy;
        }
    }

    fn send_line(&mut self, line: &str) -> Result<(), AdvError> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(KEY_ENTER);
        self.write_input(&bytes)
    }

    /// Settle until `pred` holds on the rendered screen model; the error
    /// carries the last screen for triage.
    fn settle_screen<F>(&mut self, mut pred: F) -> Result<TerminalSnapshot, AdvError>
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        let mut last = String::new();
        let policy = self.policy;
        self.session_mut()?
            .read_settled_frame_where(&policy, |snapshot| {
                last = snapshot.lines.join("\n");
                pred(snapshot)
            })
            .map(|frame| frame.snapshot)
            .map_err(|error| AdvError::Assert(format!("{error}; last screen:\n{last}")))
    }

    fn settle_ready(&mut self) -> Result<(), AdvError> {
        let _ = self.settle_screen(ready_screen)?;
        Ok(())
    }

    /// Type `text` one byte per write and settle until it shows on screen.
    fn type_slowly(&mut self, text: &str) -> Result<(), AdvError> {
        for byte in text.bytes() {
            self.write_input(&[byte])?;
        }
        let needle = text.to_owned();
        let _ = self.settle_screen(move |s| screen_has(s, &needle))?;
        Ok(())
    }

    /// Empty the composer by killing in both directions, then settle ready.
    fn clear_editor(&mut self) -> Result<(), AdvError> {
        // ctrl+k (deleteToLineEnd) covers text right of a mid-line cursor,
        // ctrl+u (deleteToLineStart) the left side however long it is, and
        // the backspace joins the line above so the next pair drains it too.
        // Fixed 64 backspaces could neither clear giant single lines nor
        // reach text past a moved cursor.
        self.write_input(&[0x0b, 0x15, 0x7f].repeat(40))?;
        self.settle_ready()
    }

    /// Prove composer focus: sentinel must land on the `❯` prompt line.
    /// A throwaway space leads the sentinel: a truncated UTF-8 tail byte from
    /// a prior hostile burst legitimately consumes the next input byte, and
    /// a pending sequence state may swallow one printable char.
    fn prove_editor_focus(&mut self, scenario: &str, sentinel: &str) -> Result<(), AdvError> {
        self.write_input(b" ")?;
        for byte in sentinel.bytes() {
            self.write_input(&[byte])?;
        }
        let needle = sentinel.to_owned();
        let snapshot = self.settle_screen({
            let needle = needle.clone();
            move |s| screen_has(s, &needle)
        })?;
        let on_prompt = snapshot.lines.iter().any(|line| {
            let trimmed = line.trim();
            trimmed.contains(&needle) && trimmed.contains(PROMPT_GLYPH)
        });
        if !on_prompt {
            return Err(AdvError::Assert(format!(
                "{scenario}: sentinel {sentinel:?} did not land on the composer; screen:\n{}",
                snapshot.lines.join("\n")
            )));
        }
        for _ in 0..=sentinel.len() {
            self.write_input(KEY_BACKSPACE)?;
        }
        self.settle_ready()
    }

    /// `/quit` — the session closes over the child's exit.
    fn quit_clean(&mut self) -> Result<(), AdvError> {
        self.send_line("/quit")?;
        Ok(())
    }

    fn close_assert(&mut self, scenario: &str) -> Result<(), AdvError> {
        let Some(session) = self.session.take() else {
            return Err(AdvError::Driver("session already closed".to_owned()));
        };
        let status = session.close()?;
        if !status.success() {
            return Err(AdvError::Assert(format!(
                "{scenario}: expected clean exit 0, got code {} signal {:?}",
                status.code, status.signal
            )));
        }
        Ok(())
    }
}

fn screen_has(snapshot: &TerminalSnapshot, needle: &str) -> bool {
    snapshot.lines.iter().any(|line| line.contains(needle))
}

fn ready_screen(snapshot: &TerminalSnapshot) -> bool {
    if screen_has(snapshot, "esc to cancel") {
        return false;
    }
    READY_MARKERS.iter().any(|m| screen_has(snapshot, m)) || screen_has(snapshot, FINAL_MARKER)
}

fn count_lines_containing(snapshot: &TerminalSnapshot, needle: &str) -> usize {
    snapshot
        .lines
        .iter()
        .filter(|line| line.contains(needle))
        .count()
}

/// One scenario: fresh sandbox + fresh PTY child.
fn boot() -> Result<(Sandbox, ProductRun), AdvError> {
    let sandbox = create_sandbox()?;
    let spec = launch_spec(&sandbox)?;
    let mut run = ProductRun::open(&spec)?;
    run.settle_ready()?;
    Ok((sandbox, run))
}

// ---------------------------------------------------------------------------
// Adversarial scenarios
// ---------------------------------------------------------------------------

/// Bracketed paste of >10 unicode lines (large-paste marker path), delivered
/// in two writes that split the paste mid-sequence, plus a small paste with
/// tabs/quotes that must stay inline. Submit and prove the turn streams.
fn scenario_paste_storm() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-paste-storm";

    let mut payload = String::new();
    for line in 0..25 {
        let _ = writeln!(
            payload,
            "paste line {line:02} 🦀 👨\u{200d}👩\u{200d}👧\u{200d}👦 e\u{301} tail"
        );
    }
    let split = payload.len() / 2;
    // Split mid-paste: opener + half the payload, then the rest + closer.
    let mut first = b"\x1b[200~".to_vec();
    first.extend_from_slice(&payload.as_bytes()[..split]);
    run.write_input(&first)?;
    let mut second = payload.as_bytes()[split..].to_vec();
    second.extend_from_slice(b"\x1b[201~");
    run.write_input(&second)?;

    let snapshot = run.settle_screen(|s| screen_has(s, "[paste #1"))?;
    let marker_line = snapshot
        .lines
        .iter()
        .find(|line| line.contains("[paste #1"))
        .cloned()
        .unwrap_or_default();
    let looks_like_lines = marker_line.contains('+') && marker_line.contains("lines");
    let looks_like_chars = marker_line.contains("chars");
    if !(looks_like_lines || looks_like_chars) {
        return Err(AdvError::Assert(format!(
            "{scenario}: expected large-paste marker, got {marker_line:?}"
        )));
    }

    // Small paste with tabs/quotes stays inline (below both thresholds).
    let mut small = "\x1b[200~a\tb'c\"d\ne\u{301}f".as_bytes().to_vec();
    small.extend_from_slice(b"\x1b[201~");
    run.write_input(&small)?;
    let _ = run.settle_screen(|s| screen_has(s, "b'c\"d") && screen_has(s, "e\u{301}f"))?;

    // Submit the pasted buffer; the verification turn must stream to FINAL.
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.prove_editor_focus(scenario, "pastefocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Resize storm mid-stream, then another while a selector holds focus.
fn scenario_resize_storm() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-resize-storm";

    run.send_line("verification storm turn")?;
    // Fire the storm with the stream still in flight.
    run.session_mut()?
        .resize_storm(&[(120, 40), (24, 8), (17, 5), (100, 30), (80, 24)])?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.settle_ready()?;

    // Storm while a selector is open; it must survive and remain coherent.
    run.send_line("/model")?;
    let _ = run.settle_screen(|s| screen_has(s, "Nova 2 Lite"))?;
    run.session_mut()?
        .resize_storm(&[(200, 60), (30, 10), (80, 24)])?;
    let _ = run.settle_screen(|s| screen_has(s, "Nova 2 Lite"))?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "stormfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Sub-floor and degenerate geometries blank the render; the viewport must
/// recover a coherent frame on restore.
fn scenario_tiny_geometry() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-tiny-geometry";

    for (cols, rows) in [(1, 1), (19, 4), (80, 1), (2, 24), (80, 24)] {
        run.session_mut()?.resize(cols, rows)?;
        // Settle per geometry: SIGWINCH notifications coalesce, so without an
        // intervening read the product may only ever observe the last size.
        let _ = run.settle_screen(|_| true)?;
    }
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "tinyfocus")?;

    // Sub-floor while a selector is open.
    run.send_line("/tree")?;
    let _ = run.settle_screen(|s| screen_has(s, "No entries found"))?;
    run.session_mut()?
        .resize_storm(&[(19, 4), (1, 1), (80, 24)])?;
    let _ = run.settle_screen(|s| screen_has(s, "No entries found"))?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "tinyfocus2")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A burst of navigation keys must neither wedge nor crash the app. CSI
/// sequences can legitimately split at a read boundary under burst cadence —
/// a trailing `\x1b` is then a real Esc (which clears the composer by design),
/// so buffer contents are not asserted; responsiveness is. Afterwards the
/// double-Esc tree selector must dismiss without leaving stale glyphs in the
/// prompt strip (a dismissed overlay's cells once lingered inside the
/// editor's claimed span).
fn scenario_key_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-key-flood";

    run.type_slowly("kf-sentinel")?;
    for _ in 0..3 {
        run.write_input(KEY_UP)?;
        run.write_input(KEY_DOWN)?;
        run.write_input(KEY_LEFT)?;
        run.write_input(KEY_RIGHT)?;
        run.write_input(KEY_HOME)?;
        run.write_input(KEY_END)?;
        run.write_input(KEY_PAGE_UP)?;
        run.write_input(KEY_PAGE_DOWN)?;
        run.write_input(KEY_SHIFT_TAB)?;
    }
    // The flood may open or dismiss UI (shift+tab thinking toggle, history
    // nav); converge to a quiet screen and prove the composer still responds.
    run.settle_ready()?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    run.clear_editor()?;

    // `/tree` opens the selector; Esc dismisses it. The selector's hint cells
    // once survived inside the editor's claim (`❯ e` ghost) — the composer
    // row must come back clean.
    run.send_line("/tree")?;
    let _ = run.settle_screen(|s| screen_has(s, "No entries found"))?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    let snapshot = run.settle_screen(|s| s.lines.iter().any(|line| line.contains(PROMPT_GLYPH)))?;
    let ghost = snapshot.lines.iter().any(|line| line.contains("❯ e"));
    if ghost {
        return Err(AdvError::Assert(format!(
            "{scenario}: stale glyph left on the composer row after selector dismiss; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "floodfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Invalid UTF-8, lone/truncated escapes, unterminated OSC, unclosed
/// bracketed-paste, and C0 controls must not wedge the input loop.
fn scenario_invalid_input() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-invalid-input";

    let bursts: &[&[u8]] = &[
        b"\x80",             // lone UTF-8 continuation
        b"\xff\xfe",         // never-valid lead bytes
        b"\xe2\x98",         // truncated 3-byte sequence
        b"\xed\xa0\x80",     // encoded surrogate
        b"\x01\x02\x05\x0b", // C0 controls (ctrl+a/b/e/k)
        b"\x1b",             // lone ESC
        b"\x1b[999~",        // malformed CSI
        // `ESC ] <digits> ;` opens reply-framing payload mode (the real
        // terminal contract), so the ST close must ride in the same burst —
        // a probe between open and close has its own bytes eaten as payload.
        b"\x1b]8;;http://x\x1b\\",
    ];
    for (index, burst) in bursts.iter().enumerate() {
        run.write_input(burst)?;
        // Liveness probe between hostile bursts — proves no wedge.
        run.prove_editor_focus(scenario, &format!("ii{index}"))?;
    }

    // An open bracketed paste buffers subsequent keys as paste content.
    // Closing it renders the buffer (including `ii9`) into the composer —
    // the settle waits on that text, so `ii9` visible afterwards proves the
    // keys were captured into the paste rather than dropped.
    run.write_input(b"\x1b[200~unclosed paste body\nnext\n")?;
    run.write_input(b" ii9")?;
    run.write_input(b"\x1b[201~")?;
    let _ = run.settle_screen(|s| screen_has(s, "unclosed") && screen_has(s, "ii9"))?;
    // Submit and prove `ii9` survived inside the delivered paste text.
    run.write_input(KEY_ENTER)?;
    let snapshot = run.settle_screen(|s| screen_has(s, "ii9") && screen_has(s, FINAL_MARKER))?;
    if !(screen_has(&snapshot, "ii9") && screen_has(&snapshot, FINAL_MARKER)) {
        return Err(AdvError::Assert(format!(
            "{scenario}: paste-captured keys lost — `ii9` missing after submit; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "ii10")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Flood every verification dialog stage with navigation, printable keys,
/// Enter, and Esc; the chain must run to completion and restore focus.
fn scenario_dialog_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-dialog-flood";

    // Each dialog resolves with exactly one key: Enter accepts select/confirm/
    // input/editor, Esc cancels the current dialog and the chain advances to
    // the next await. Noise keys flood the dialog before the resolution key.
    let resolutions: &[(&str, &[u8])] = &[
        ("Verification select prompt", KEY_ENTER),
        ("Verification confirm prompt", KEY_ESCAPE),
        ("Verification input prompt", KEY_ENTER),
        ("Verification editor prompt", KEY_ESCAPE),
    ];
    run.send_line("/verification-dialogs")?;
    for (title, resolve) in resolutions {
        let _ = run.settle_screen(|s| screen_has(s, title))?;
        // Burst noise: navigate + printable keys before resolving.
        run.write_input(KEY_DOWN)?;
        run.write_input(b"xz")?;
        run.write_input(resolve)?;
        let _ = run.settle_screen(|s| !screen_has(s, title))?;
    }
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "flooddialogs")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Stacked focusable overlay + pending select under a key flood; both must
/// resolve (Esc/timeout/'x') and the composer must come back.
fn scenario_overlay_abuse() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-overlay-abuse";

    run.send_line("/verification-overlay-stack")?;
    let _ = run.settle_screen(|s| {
        screen_has(s, STACKED_OVERLAY_LINE) && screen_has(s, STACKED_DIALOG_TITLE)
    })?;

    // Flood while the stack is live: Esc, arrows, stray 'x' presses.
    run.write_input(KEY_ESCAPE)?;
    run.write_input(b"xx")?;
    for _ in 0..4 {
        run.write_input(KEY_UP)?;
        run.write_input(KEY_DOWN)?;
    }

    // The select auto-cancels after ~6 s or on Esc; the unfocused overlay
    // completes only when a lone 'x' reaches its handleInput. Send one clean
    // 'x' (a batched "xx" may not match data === 'x'), then Esc to drop any
    // lingering dialog/overlay focus so the composer is reachable again.
    let _ = run.settle_screen(|s| !screen_has(s, STACKED_DIALOG_TITLE))?;
    run.write_input(b"x")?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "overlayabuse")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Grapheme-hostile composer text: ZWJ families, flags, VS16, CJK, RTL,
/// combining marks, zero-width joiners — plus partial grapheme backspaces.
fn scenario_unicode_composer() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-unicode-composer";

    // The rendered snapshot merges combining marks, ZWJ chains, and ZWSP into
    // grapheme cells — the raw codepoints never appear in `lines`, so the
    // settle asserts the render-stable fragments instead.
    let text = "🦀🚀 👨\u{200d}👩\u{200d}👧\u{200d}👦 🇺🇸🇩🇪 ❤️✌️ テスト שלום عربى e\u{301}a\u{308} x\u{200b}y";
    for byte in text.bytes() {
        run.write_input(&[byte])?;
    }
    let _ = run.settle_screen(|s| {
        screen_has(s, "🦀") && screen_has(s, "テスト") && screen_has(s, "عربى")
    })?;
    // Backspace across grapheme boundaries must not corrupt the buffer.
    for _ in 0..5 {
        run.write_input(KEY_BACKSPACE)?;
    }
    let _ = run.settle_screen(|s| screen_has(s, "🦀"))?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    // The echoed user turn keeps the surviving prefix on screen.
    let _ = run.settle_screen(|s| screen_has(s, "🦀"))?;
    run.prove_editor_focus(scenario, "unicfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Back-to-back submits: the second message must queue while the first
/// streams — never dropped, never wedged.
fn scenario_rapid_submit() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-rapid-submit";

    // Back-to-back sends: the ~450 ms stream keeps turn one in flight while
    // turn two submits, so the second message must queue (steer) rather than
    // race a fresh prompt — never dropped, never wedged.
    run.send_line("rapid turn one")?;
    run.send_line("rapid turn two")?;
    let snapshot = run.settle_screen(|s| count_lines_containing(s, FINAL_MARKER) >= 2)?;
    if count_lines_containing(&snapshot, FINAL_MARKER) < 2 {
        return Err(AdvError::Assert(format!(
            "{scenario}: queued submit lost — only {} FINAL markers",
            count_lines_containing(&snapshot, FINAL_MARKER)
        )));
    }
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "rapidfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// `@` completion under a key flood: trigger chars, path prefixes, a
/// leading-hyphen query (`fd --` boundary), Tab force-completion, and Esc
/// — while completions resolve off the event loop. The menu must deliver
/// real entries and the composer must stay responsive.
fn scenario_autocomplete_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let scenario = "adversarial-autocomplete-flood";

    // Seed the launch cwd so the fuzzy walk has entries to return.
    fs::create_dir_all(sandbox.work_dir.join("subdir/nested"))?;
    fs::write(sandbox.work_dir.join("subdir/file_alpha.rs"), "fn a() {}\n")?;
    fs::write(sandbox.work_dir.join("subdir/file_beta.rs"), "fn b() {}\n")?;
    fs::write(sandbox.work_dir.join("top_level.md"), "# top\n")?;

    // `@s` must surface `subdir/` via the async provider path.
    run.type_slowly("@s")?;
    let _ = run.settle_screen(|s| screen_has(s, "subdir"))?;

    // Flood around completion: more chars, path separators, hyphenated
    // query, Tab force-completes, Esc closes.
    for token in ["u", "b", "/", "-foo", "\u{1b}", "\t", "@top"] {
        run.write_input(token.as_bytes())?;
    }
    run.write_input(KEY_ESCAPE)?;
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "acfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// ctrl+d everywhere it is safe: selector context (never exits), non-empty
/// editor (forward-delete), then empty-editor mash = clean exit.
fn scenario_ctrl_mash() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-ctrl-mash";

    run.send_line("/model")?;
    let _ = run.settle_screen(|s| screen_has(s, "Nova 2 Lite"))?;
    for _ in 0..5 {
        run.write_input(KEY_CTRL_D)?;
    }
    // Selector context swallows ctrl+d; the app must still be alive.
    let _ = run.settle_screen(|s| screen_has(s, "Nova 2 Lite") || ready_screen(s))?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;

    // Non-empty composer: ctrl+d is forward-delete, not exit.
    run.type_slowly("mash")?;
    run.write_input(KEY_LEFT)?;
    run.write_input(KEY_CTRL_D)?;
    for _ in 0..8 {
        run.write_input(KEY_BACKSPACE)?;
    }
    run.settle_ready()?;

    // Empty composer: ctrl+d mash exits cleanly.
    for _ in 0..3 {
        run.write_input(KEY_CTRL_D)?;
    }
    run.close_assert(scenario)
}

/// `/quit` raced with trailing input bytes and a resize — exit must stay
/// clean even when input arrives during teardown.
fn scenario_quit_race() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-quit-race";

    run.send_line("/quit")?;
    // Bytes and a resize arriving during shutdown must not matter.
    run.write_input(b"trailing-garbage").ok();
    let _ = run.session_mut().map(|s| s.resize(120, 40));
    run.close_assert(scenario)
}

/// Steer chain: three submits back-to-back — turn one in flight (~450 ms),
/// turns two and three must queue rather than wedge or drop.
fn scenario_steer_chain() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-steer-chain";

    run.send_line("steer chain one")?;
    run.send_line("steer chain two")?;
    run.send_line("steer chain three")?;
    let snapshot = run.settle_screen(|s| count_lines_containing(s, FINAL_MARKER) >= 3)?;
    if count_lines_containing(&snapshot, FINAL_MARKER) < 3 {
        return Err(AdvError::Assert(format!(
            "{scenario}: only {} of 3 chained turns completed; screen:\n{}",
            count_lines_containing(&snapshot, FINAL_MARKER),
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "steerfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Bracketed paste colliding with a resize storm while the paste is still
/// open — mid-paste reflows must not corrupt the capture or wedge the
/// composer.
fn scenario_paste_resize_collision() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-paste-resize";

    run.write_input(b"\x1b[200~collision alpha\n")?;
    run.session_mut()?
        .resize_storm(&[(31, 9), (120, 40), (17, 5), (80, 24)])?;
    run.write_input(b"collision beta\n")?;
    run.write_input(b"\x1b[201~")?;
    let _ =
        run.settle_screen(|s| screen_has(s, "collision alpha") && screen_has(s, "collision beta"))?;
    run.write_input(KEY_ENTER)?;
    let snapshot = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    if !screen_has(&snapshot, FINAL_MARKER) {
        return Err(AdvError::Assert(format!(
            "{scenario}: pasted turn never streamed; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "collidefocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Esc mid-stream: the in-flight turn aborts, the UI settles back to idle,
/// and a fresh turn streams normally afterwards.
fn scenario_interrupt_stream() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-interrupt-stream";

    run.send_line("interrupt me mid stream")?;
    // Lands inside the ~450 ms stream window: Interrupt aborts the turn.
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;

    run.send_line("post interrupt turn")?;
    let snapshot = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    if !screen_has(&snapshot, FINAL_MARKER) {
        return Err(AdvError::Assert(format!(
            "{scenario}: post-interrupt turn never streamed; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "intrfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A ~40 KiB paste delivered in chunks — large-paste capture must not
/// truncate the buffer or wedge the composer.
fn scenario_huge_paste() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-huge-paste";

    let mut payload = String::new();
    for line in 0..300 {
        let _ = writeln!(payload, "huge {line:03} {}", "🦀🌊🚀".repeat(8));
    }
    let bytes = payload.as_bytes();
    let chunk = bytes.len() / 4 + 1;
    run.write_input(b"\x1b[200~")?;
    for part in bytes.chunks(chunk) {
        run.write_input(part)?;
    }
    run.write_input(b"\x1b[201~")?;
    // A multi-line paste renders as a `[paste #N` chip in the composer.
    let _ = run.settle_screen(|s| screen_has(s, "[paste #"))?;
    run.write_input(KEY_ENTER)?;
    let snapshot = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    if !screen_has(&snapshot, FINAL_MARKER) {
        return Err(AdvError::Assert(format!(
            "{scenario}: huge-paste turn never streamed; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "hugefocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Multi-byte UTF-8 split across individual writes — the input parser must
/// buffer partial sequences instead of dropping or mojibake-ing them.
fn scenario_split_utf8() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-split-utf8";

    // 🦀 (F0 9F A6 80) and 🔥 (F0 9F 94 A5), one byte per write with ASCII
    // interleaved — each write is a separate PTY read boundary.
    for byte in "🦀".as_bytes() {
        run.write_input(&[*byte])?;
    }
    run.write_input(b" ")?;
    for byte in "🔥".as_bytes() {
        run.write_input(&[*byte])?;
    }
    let _ = run.settle_screen(|s| screen_has(s, "🦀") && screen_has(s, "🔥"))?;
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "utf8focus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A paste body containing a literal `\x1b[200~` — the inner marker is paste
/// data, not a re-arm of the paste state machine.
fn scenario_nested_paste_marker() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-nested-paste-marker";

    run.write_input(b"\x1b[200~outerA\x1b[200~outerB\x1b[201~")?;
    let snapshot = run.settle_screen(|s| screen_has(s, "outerA"))?;
    if !screen_has(&snapshot, "outerA") {
        return Err(AdvError::Assert(format!(
            "{scenario}: paste body missing after nested marker; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    // Whatever the parser did with the inner marker, typing must still work.
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "nestedfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Selector dialog under a resize storm — the dialog must reflow, Esc must
/// dismiss it, and the composer row must come back clean.
fn scenario_selector_resize() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-selector-resize";

    run.send_line("/tree")?;
    let _ = run.settle_screen(|s| screen_has(s, "No entries found"))?;
    run.session_mut()?
        .resize_storm(&[(19, 4), (120, 40), (1, 1), (80, 24)])?;
    run.write_input(KEY_ESCAPE)?;
    run.settle_ready()?;
    let snapshot = run.settle_screen(|s| s.lines.iter().any(|line| line.contains(PROMPT_GLYPH)))?;
    let ghost = snapshot
        .lines
        .iter()
        .any(|line| line.trim().contains("No entries found") && line.contains(PROMPT_GLYPH));
    if ghost {
        return Err(AdvError::Assert(format!(
            "{scenario}: selector residue survived into the composer row; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "selresize")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Composer text at the column boundary: wide CJK + emoji + combining marks
/// exactly filling the row — wrap must not clip or smear cells.
fn scenario_width_edge() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-width-edge";

    // 39 wide chars (78 cells) + 2 ASCII = exactly 80 columns; then a long
    // combining-mark run pushes past the edge. The composer scrolls the
    // overflow out of view, so the settled screen can only show the tail.
    let edge = format!("{}ab{}", "\u{754c}".repeat(39), "e\u{301}".repeat(10));
    for byte in edge.bytes() {
        run.write_input(&[byte])?;
    }
    // The emulator keeps U+0301 inside the cell text ("ééé"), so the
    // contiguous marker for a landed combining tail is "abe", not "eeee".
    let _ = run.settle_screen(|s| screen_has(s, "abe"))?;
    run.write_input(KEY_ENTER)?;
    let snapshot = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    if !screen_has(&snapshot, FINAL_MARKER) {
        return Err(AdvError::Assert(format!(
            "{scenario}: boundary-width turn never streamed; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.prove_editor_focus(scenario, "edgefocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A whitespace-only submit must not spawn an empty turn — the composer
/// clears (or stays) and no agent run starts.
fn scenario_whitespace_submit() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-whitespace-submit";

    run.write_input(b"    ")?;
    run.write_input(KEY_ENTER)?;
    let snapshot = run.settle_screen(ready_screen)?;
    if screen_has(&snapshot, FINAL_MARKER) || screen_has(&snapshot, "esc to cancel") {
        return Err(AdvError::Assert(format!(
            "{scenario}: whitespace submit spawned an agent turn; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "wsfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Kill-ring round trip under flood: ctrl+a + ctrl+k kills the line into
/// the ring, ctrl+y yanks it back, alt+y pops (single entry — may keep or
/// clear), ctrl+u kills whatever remains, ctrl+y re-yanks. Either pop
/// semantics restores "ring text": if alt+y kept the line, ctrl+u pushed
/// it and ctrl+y yanks it; if alt+y cleared the composer, ctrl+u pushed
/// nothing and ctrl+y yanks the original kill.
fn scenario_kill_yank() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-kill-yank";

    run.write_input(b"ring text")?;
    run.write_input(b"\x01")?; // ctrl+a — line start
    run.write_input(b"\x0b")?; // ctrl+k — kill to end
    run.write_input(b"\x19")?; // ctrl+y — yank
    let _ = run.settle_screen(|s| screen_has(s, "ring text"))?;
    run.write_input(b"\x1by")?; // alt+y — yank pop
    run.write_input(b"\x15")?; // ctrl+u — kill to start
    run.write_input(b"\x19")?; // ctrl+y — yank
    let _ = run.settle_screen(|s| screen_has(s, "ring text"))?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Editor motions under flood: alt+word jumps, home/end, word delete, and
/// a ctrl+j embedded newline which must insert — not submit — before the
/// real Enter does.
fn scenario_editor_motions() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-editor-motions";

    run.write_input(b"foo bar baz")?;
    run.write_input(b"\x1bb\x1bb")?; // alt+b twice — word left
    run.write_input(b"\x1bf")?; // alt+f — word right
    run.write_input(b"\x01")?; // ctrl+a — line start
    run.write_input(b"\x05")?; // ctrl+e — line end
    run.write_input(b"\x17")?; // ctrl+w — delete word backward ("baz")
    run.write_input(b"\x0a")?; // ctrl+j — newline insert, not submit
    run.write_input(b"line2")?;
    let _ = run.settle_screen(|s| screen_has(s, "line2"))?;
    let snapshot = run.settle_screen(|_| true)?;
    if screen_has(&snapshot, FINAL_MARKER) || screen_has(&snapshot, "esc to cancel") {
        return Err(AdvError::Assert(format!(
            "{scenario}: ctrl+j submitted instead of inserting a newline; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Terminal query replies injected as keystrokes: cursor-position reports,
/// primary DA, kitty keyboard flags, `XTVersion`, an OSC 11 colour reply, and
/// a window-op reply. They are answers the terminal would send — the input
/// layer must consume them without leaking bytes into the composer.
fn scenario_reply_spoof() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-reply-spoof";

    let replies: &[&[u8]] = &[
        b"\x1b[1;1R",                        // cursor position report
        b"\x1b[12;34R",                      // cursor position report, wide
        b"\x1b[?62;4;22c",                   // primary device attributes
        b"\x1b[?1u",                         // kitty keyboard flags report
        b"\x1b[>0;276;0c",                   // XTVersion reply
        b"\x1b]11;rgb:0101/0202/0303\x1b\\", // OSC 11 reply (ST terminator)
        b"\x1b[4;24;80t",                    // window-op text-area reply
    ];
    for reply in replies {
        run.write_input(reply)?;
    }
    let snapshot = run.settle_screen(|_| true)?;
    for needle in ["1;1R", "12;34", "62;4", "276", "0101/0202"] {
        if screen_has(&snapshot, needle) {
            return Err(AdvError::Assert(format!(
                "{scenario}: terminal reply bytes leaked to screen ({needle}); screen:\n{}",
                snapshot.lines.join("\n")
            )));
        }
    }
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "spooffocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// SGR mouse reports and focus in/out events flooded as input. Mouse events
/// are routed UI events; they must not corrupt the composer or wedge the
/// frame — and must not leave the app interpreting them as keystrokes.
fn scenario_mouse_focus_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-mouse-focus";

    let events: &[&[u8]] = &[
        b"\x1b[<0;10;5M",  // left press
        b"\x1b[<0;10;5m",  // left release
        b"\x1b[<32;15;6M", // drag
        b"\x1b[<32;25;9M",
        b"\x1b[<35;30;12M", // motion, no button
        b"\x1b[<64;20;8M",  // wheel up
        b"\x1b[<65;20;8M",  // wheel down
        b"\x1b[<3;79;23M",  // right press far corner
        b"\x1b[<3;79;23m",
        b"\x1b[I", // focus in
        b"\x1b[O", // focus out
        b"\x1b[I",
    ];
    for event in events {
        run.write_input(event)?;
    }
    let snapshot = run.settle_screen(|_| true)?;
    for needle in ["<0;10;5", "<64;20;8", "79;23"] {
        if screen_has(&snapshot, needle) {
            return Err(AdvError::Assert(format!(
                "{scenario}: mouse report leaked to screen ({needle}); screen:\n{}",
                snapshot.lines.join("\n")
            )));
        }
    }
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "mousefocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A 4 KiB single-line composer (no newlines): the buffer must scroll,
/// the cursor stay live, and the whole payload still submit.
fn scenario_giant_line() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-giant-line";

    // Input-driven paints bypass the coalescer, so a typed flood drains at
    // roughly 5ms per key (insert + full wrap-map rebuild + repaint): give
    // quiescence a bigger budget than the default policy.
    run.settle_budget(Duration::from_secs(120));
    let line = "g".repeat(4096);
    for chunk in line.as_bytes().chunks(2048) {
        run.write_input(chunk)?;
    }
    // Quiescence only — the composer scrolled the head out of view.
    let _ = run.settle_screen(|_| true)?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// NUL and C0 control bytes inside a bracketed paste: they must be
/// literalized or stripped — never acted on as commands (a stray \x04
/// mid-paste must not EOF, \x0d must not submit early).
fn scenario_binary_paste() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-binary-paste";

    run.write_input(b"\x1b[200~")?;
    run.write_input(b"lead\x00\x01\x02\x0b\x0c\x0e\x1ftail")?;
    run.write_input(b"\x1b[201~")?;
    let snapshot = run.settle_screen(|_| true)?;
    if screen_has(&snapshot, FINAL_MARKER) || screen_has(&snapshot, "esc to cancel") {
        return Err(AdvError::Assert(format!(
            "{scenario}: control byte inside paste triggered an early submit; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A prompt typed into a paste while a turn streams, then submitted: the
/// queue must run it after the in-flight turn — two markers total.
fn scenario_paste_queue() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-paste-queue";

    run.send_line("paste queue turn one")?;
    // Mid-stream: open a paste, type the follow-up, close, submit.
    run.write_input(b"\x1b[200~")?;
    run.write_input(b"paste queue turn two")?;
    run.write_input(b"\x1b[201~")?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| count_lines_containing(s, FINAL_MARKER) >= 2)?;
    run.settle_ready()?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A slash-command selector opened while a turn streams: `/tree` during a
/// run still dispatches (slash bypasses the queue gate), and the selector
/// must coexist with — then dismiss back into — the live stream.
fn scenario_selector_stream() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-selector-stream";

    run.send_line("selector stream turn")?;
    // Stream is ~450ms (3 x 150ms chunks) — fire /tree straight away so it
    // opens mid-stream. A tree selector with a submitted turn lists the
    // live session as its node, so its footer (not "No entries found") is
    // the open signal.
    run.write_input(b"/tree")?;
    run.write_input(KEY_ENTER)?;
    let snapshot =
        run.settle_screen(|s| screen_has(s, "esc to cancel") || screen_has(s, FINAL_MARKER))?;
    if !screen_has(&snapshot, "esc to cancel") {
        return Err(AdvError::Assert(format!(
            "{scenario}: /tree selector never opened mid-stream; screen:\n{}",
            snapshot.lines.join("\n")
        )));
    }
    run.write_input(KEY_ESCAPE)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "ssfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Kitchen-sink collision: while a turn streams, resize to a 1x1 floor,
/// emit focus events, fire SGR mouse reports, open a paste, drop invalid
/// bytes inside it, close, and restore. The turn must still complete.
fn scenario_kitchen_sink() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-kitchen-sink";

    run.send_line("kitchen sink turn")?;
    run.session_mut()?.resize(1, 1)?;
    run.write_input(b"\x1b[O\x1b[I")?; // focus out/in
    run.write_input(b"\x1b[<0;1;1M\x1b[<0;1;1m")?; // mouse at corner
    run.write_input(b"\x1b[200~")?; // open paste
    run.write_input(b"\xff\xfepaste-in-storm")?; // invalid bytes + text
    run.write_input(b"\x1b[201~")?; // close paste
    run.session_mut()?.resize(80, 24)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.settle_ready()?;
    run.clear_editor()?;
    run.prove_editor_focus(scenario, "ksfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Sustained interleaved pressure: a deterministic mixed stream of
/// printable keys, cursor keys, backspaces, ctrl chords, focus events and
/// resizes for 64 rounds. The composer may hold arbitrary text afterward —
/// liveness and a clean quit are the contract.
fn scenario_sustained_pressure() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-sustained-pressure";

    let chords: &[&[u8]] = &[
        b"x",
        b"y",
        KEY_LEFT,
        KEY_RIGHT,
        KEY_UP,
        KEY_DOWN,
        KEY_BACKSPACE,
        b"\x17",  // ctrl+w
        b"\x1bb", // alt+b
        b"\x1bf", // alt+f
        b"\x1a",  // ctrl+z byte
        b"\x1b[I",
        b"\x1b[O", // focus in/out
        b"\t",     // tab
        KEY_HOME,
        KEY_END,
    ];
    for round in 0..64 {
        run.write_input(chords[round % chords.len()])?;
        if round % 16 == 7 {
            let cols = 24 + u16::try_from(round % 3).unwrap_or(0) * 40;
            run.session_mut()?.resize(cols, 8)?;
        }
        if round % 16 == 15 {
            run.session_mut()?.resize(80, 24)?;
        }
    }
    run.write_input(KEY_ESCAPE)?;
    run.write_input(KEY_ESCAPE)?;
    let _ = run.settle_screen(|_| true)?;
    run.clear_editor()?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "spfocus")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Undo flood: type, mash undo (ctrl+-) well past the bottom of history,
/// then retype and submit — the edit stack must bottom out cleanly rather
/// than corrupting the buffer.
fn scenario_undo_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-undo-flood";

    run.write_input(b"und ground")?;
    for _ in 0..16 {
        run.write_input(b"\x1f")?; // ctrl+- — undo
    }
    run.write_input(b"redo tail")?;
    let _ = run.settle_screen(|_| true)?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Terminal mode commands arriving on the INPUT wire: alt-screen, bracketed
/// paste enable, synchronized output, insert mode, OSC 52 clipboard set,
/// erase/display and cursor-home sequences. These are output-direction
/// commands — as input they must parse as keys or be dropped, never execute
/// or corrupt the frame.
fn scenario_command_injection() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-command-injection";

    for seq in [
        b"\x1b[?1049h".as_slice(),        // DECSET alt screen
        b"\x1b[?2004h".as_slice(),        // DECSET bracketed paste
        b"\x1b[?2026h".as_slice(),        // synchronized output mode
        b"\x1b[4;2h".as_slice(),          // insert/replace mode set
        b"\x1b]52;c;aGk=\x07".as_slice(), // OSC 52 clipboard write
        b"\x1b[2J".as_slice(),            // erase display
        b"\x1b[H".as_slice(),             // cursor home (parses as Home key)
        b"\x1b[?25h".as_slice(),          // show cursor
        b"\x1b[!p".as_slice(),            // soft reset
        b"\x1bc".as_slice(),              // RIS full reset
    ] {
        run.write_input(seq)?;
    }
    run.clear_editor()?;
    let snapshot = run.settle_screen(ready_screen)?;
    if snapshot.lines.iter().any(|line| line.contains("1049")) {
        return Err(AdvError::Assert(format!(
            "alt-screen command leaked onto the screen: {snapshot:?}"
        )));
    }
    run.prove_editor_focus(scenario, "cmdinj")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Input racing the boot sequence: a full prompt written immediately after
/// the child spawns, before the TUI finishes probing and painting. Whatever
/// survives the race must not corrupt the session — a clean turn still runs.
fn scenario_preboot_input() -> Result<(), AdvError> {
    let sandbox = create_sandbox()?;
    let spec = launch_spec(&sandbox)?;
    let mut run = ProductRun::open(&spec)?;
    let scenario = "adversarial-preboot-input";

    run.write_input(b"preboot probe\r")?;
    run.settle_ready()?;
    run.clear_editor()?;
    run.write_input(b"preboot-ok")?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Bytes written after /quit races the exit path. Writes fail once the child
/// dies — the contract is a clean exit 0, not that the writes land.
fn scenario_post_quit_bytes() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-post-quit-bytes";

    run.write_input(b"/quit")?;
    run.write_input(KEY_ENTER)?;
    for _ in 0..8 {
        if run.write_input(b"post-exit bytes\r\n").is_err() {
            break;
        }
    }
    run.close_assert(scenario)
}

/// Geometry at `u16` boundaries: 0x0, 1x1, `u16::MAX` on each axis and both.
/// The kernel accepts absurd winsizes; the compositor must clamp rather than
/// allocate-or-crash on 65535x65535. Driver-level ioctl rejection is tolerated
/// — the product contract is that it never wedges or panics.
fn scenario_extreme_geometry() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-extreme-geometry";

    let _ = run.session_mut()?.resize(0, 0);
    let _ = run.settle_screen(|_| true)?;
    let _ = run.session_mut()?.resize(1, 1);
    let _ = run.settle_screen(|_| true)?;
    let _ = run.session_mut()?.resize(u16::MAX, 1);
    let _ = run.session_mut()?.resize(1, u16::MAX);
    let _ = run.settle_screen(|_| true)?;
    let _ = run.session_mut()?.resize(u16::MAX, u16::MAX);
    let _ = run.settle_screen(|_| true)?;
    run.session_mut()?.resize(80, 24)?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "geommax")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Pathological grapheme clusters: a 25-member ZWJ emoji chain, 100 combining
/// marks on one base, tag characters, a 50-variation-selector run, a regional
/// indicator run, and an unassigned codepoint. Cursor math and the submit
/// path must handle all of them.
fn scenario_grapheme_torture() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-grapheme-torture";
    run.settle_budget(Duration::from_secs(90));

    let mut torture = String::new();
    for _ in 0..25 {
        torture.push_str("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}\u{200d}");
    }
    torture.push('a');
    for _ in 0..100 {
        torture.push('\u{301}');
    }
    for cp in 0xe0001_u32..=0xe0050_u32 {
        if let Some(c) = char::from_u32(cp) {
            torture.push(c);
        }
    }
    torture.push('\u{25a0}');
    for _ in 0..50 {
        torture.push('\u{fe0f}');
    }
    for cp in 0x1f1e6_u32..=0x1f1fb_u32 {
        if let Some(c) = char::from_u32(cp) {
            torture.push(c);
        }
    }
    torture.push('\u{378}');
    for chunk in torture.as_bytes().chunks(512) {
        run.write_input(chunk)?;
    }
    let _ = run.settle_screen(|_| true)?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Double-width characters straddling the exact right edge at several
/// geometries: (cols-1) ASCII cells then a CJK wide char lands split across
/// the boundary. Wrap math must not loop, overlap or drop the char.
fn scenario_wide_edge_sweep() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-wide-edge-sweep";

    for cols in [80_u16, 100, 160] {
        run.session_mut()?.resize(cols, 24)?;
        run.clear_editor()?;
        let line = "a".repeat(usize::from(cols - 1)) + "界";
        run.write_input(line.as_bytes())?;
        let _ = run.settle_screen(|_| true)?;
    }
    run.session_mut()?.resize(80, 24)?;
    run.clear_editor()?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "widesweep")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Legacy X10 mouse reports (CSI M + 3 raw offset bytes) including high-bit
/// coordinates, plus DECRQM mode reports. Reply/report bytes must not leak
/// into the composer or transcript.
fn scenario_x10_mouse() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-x10-mouse";

    for report in [
        b"\x1b[M \x25\x2a".as_slice(),    // left press at (5,10)
        b"\x1b[M#\x30\x35".as_slice(),    // release at (16,21)
        b"\x1b[M\x20\x84\x85".as_slice(), // high-bit coords (100,101)
        b"\x1b[?9;1$y".as_slice(),        // DECRQM X10 mouse report
        b"\x1b[?1006;2$y".as_slice(),     // DECRQM SGR mouse report
    ] {
        run.write_input(report)?;
    }
    let snapshot = run.settle_screen(ready_screen)?;
    if snapshot
        .lines
        .iter()
        .any(|line| line.contains('M') && (line.contains("%*") || line.contains("#05")))
    {
        return Err(AdvError::Assert(format!(
            "x10 mouse report bytes leaked onto the screen: {snapshot:?}"
        )));
    }
    run.prove_editor_focus(scenario, "x10ok")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// History stack boundaries: submit ten distinct prompts, then Up/Down churn
/// past both ends of the stack and re-submit a recalled entry.
fn scenario_history_pressure() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-history-pressure";

    for i in 0..10 {
        run.write_input(format!("hist-{i}").as_bytes())?;
        run.write_input(KEY_ENTER)?;
        let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    }
    for _ in 0..15 {
        run.write_input(KEY_UP)?;
    }
    for _ in 0..15 {
        run.write_input(KEY_DOWN)?;
    }
    for _ in 0..12 {
        run.write_input(KEY_UP)?;
    }
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "histok")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Terminal replies spliced inside a typed word: cursor-position and DA1
/// replies between "hel" and "lo world". The reply layer must consume them —
/// the submitted message must read "hello world" intact.
fn scenario_reply_midword() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-reply-midword";

    run.write_input(b"hel")?;
    run.write_input(b"\x1b[12;1R")?;
    run.write_input(b"lo")?;
    run.write_input(b"\x1b[?62;4c")?;
    run.write_input(b" world")?;
    let _ = run.settle_screen(|s| screen_has(s, "hello world"))?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A bracketed paste left open across other keys: everything until the close
/// marker is paste content, including arrows and Enter. Closing late must
/// produce one coherent paste chip that submits normally.
fn scenario_unclosed_paste() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-unclosed-paste";

    // Only large pastes render as `[paste #N` chips (>10 lines or >1000
    // chars); a small single-line paste lands inline instead.
    run.write_input(b"\x1b[200~dangling paste payload")?;
    run.write_input(KEY_DOWN)?; // swallowed as literal paste bytes
    run.write_input(b"still inside the paste\n")?;
    run.write_input(b"more\n".repeat(10).as_slice())?;
    run.write_input(b"\x1b[201~")?;
    let _ = run.settle_screen(|s| screen_has(s, "[paste #1"))?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// One 96 KiB bracketed paste delivered in 8 KiB writes: the paste becomes a
/// single chip and the session stays responsive enough to submit it.
fn scenario_byte_bomb() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-byte-bomb";

    run.write_input(b"\x1b[200~")?;
    let payload = "0123456789abcdef".repeat(6 * 1024); // 96 KiB
    for chunk in payload.as_bytes().chunks(8192) {
        run.write_input(chunk)?;
    }
    run.write_input(b"\x1b[201~")?;
    let _ = run.settle_screen(|s| screen_has(s, "[paste #1"))?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Alt+letter for the full alphabet in one flood: word-motion bindings vs
/// unbound chords must all resolve without corrupting the frame.
fn scenario_alt_alpha_flood() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-alt-alpha-flood";

    run.write_input(b"alpha base text")?;
    for c in b'a'..=b'z' {
        run.write_input(&[0x1b, c])?;
    }
    let _ = run.settle_screen(|_| true)?;
    run.clear_editor()?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "altok")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// Composer pushed to its height cap: embedded newlines (or wrapped text if
/// ctrl+j is unbound) grow it until it clamps, then it must still submit.
fn scenario_composer_max_lines() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-composer-max-lines";

    for line in 0..18 {
        run.write_input(format!("line-{line:02}").as_bytes())?;
        run.write_input(b"\x0a")?; // ctrl+j — newline or no-op
    }
    let _ = run.settle_screen(|_| true)?;
    run.write_input(KEY_ENTER)?;
    let _ = run.settle_screen(|s| screen_has(s, FINAL_MARKER))?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

/// A deterministically shuffled mega-stream: paste chunks, terminal replies,
/// X10 mouse, C1 bytes, invalid UTF-8, cursor keys and text all interleaved
/// in a single burst, followed by resize churn.
fn scenario_interleaved_bits() -> Result<(), AdvError> {
    let (sandbox, mut run) = boot()?;
    let _ = &sandbox;
    let scenario = "adversarial-interleaved-bits";

    let mut chunks: Vec<Vec<u8>> = vec![
        // The paste must be complete inside one chunk: an open bracketed
        // paste swallows every later byte (including /quit) until its close,
        // so a shuffle separating open from close would never let the
        // session exit.
        b"\x1b[200~interleaved paste\x1b[201~".to_vec(),
        b"\x1b[12;34R".to_vec(),
        b"\x1b[?62;4c".to_vec(),
        b"abc".to_vec(),
        b"\x1b[M %$".to_vec(),
        vec![0xff, 0x00],
        b"\x1b[I".to_vec(),
        KEY_LEFT.to_vec(),
        b"def".to_vec(),
        vec![0x9b], // 8-bit CSI
        b"ghi".to_vec(),
        b"\x1b[?25h".to_vec(),
        KEY_UP.to_vec(),
        b"jkl".to_vec(),
        b"\x1b[4;24;80t".to_vec(),
    ];
    // Deterministic LCG shuffle (seed fixed for reproducibility).
    let mut state: u64 = 0x9e37_79b9;
    for i in (1..chunks.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = usize::try_from(state >> 33).unwrap_or(0) % (i + 1);
        chunks.swap(i, j);
    }
    for chunk in &chunks {
        run.write_input(chunk)?;
    }
    for (cols, rows) in [(120_u16, 30_u16), (60, 20), (80, 24)] {
        run.session_mut()?.resize(cols, rows)?;
    }
    run.clear_editor()?;
    run.settle_ready()?;
    run.prove_editor_focus(scenario, "bitsok")?;
    run.quit_clean()?;
    run.close_assert(scenario)
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

#[expect(
    clippy::type_complexity,
    reason = "scenario table type is inherently complex; a type alias would be used only in this test"
)]
#[test]
fn tui_adversarial_gauntlet_hostile_inputs_geometry_and_dialog_storms() {
    if std::env::var("PI_ADVERSARIAL_SKIP").is_ok() {
        eprintln!("tui_adversarial_gauntlet skipped (PI_ADVERSARIAL_SKIP)");
        return;
    }
    let only: Option<String> = std::env::var("PI_GAUNTLET_ONLY").ok();
    let scenarios: &[(&str, fn() -> Result<(), AdvError>)] = &[
        ("paste-storm", scenario_paste_storm),
        ("resize-storm", scenario_resize_storm),
        ("tiny-geometry", scenario_tiny_geometry),
        ("key-flood", scenario_key_flood),
        ("invalid-input", scenario_invalid_input),
        ("dialog-flood", scenario_dialog_flood),
        ("overlay-abuse", scenario_overlay_abuse),
        ("unicode-composer", scenario_unicode_composer),
        ("rapid-submit", scenario_rapid_submit),
        ("autocomplete-flood", scenario_autocomplete_flood),
        ("ctrl-mash", scenario_ctrl_mash),
        ("quit-race", scenario_quit_race),
        ("steer-chain", scenario_steer_chain),
        ("paste-resize", scenario_paste_resize_collision),
        ("interrupt-stream", scenario_interrupt_stream),
        ("huge-paste", scenario_huge_paste),
        ("split-utf8", scenario_split_utf8),
        ("nested-paste-marker", scenario_nested_paste_marker),
        ("selector-resize", scenario_selector_resize),
        ("width-edge", scenario_width_edge),
        ("whitespace-submit", scenario_whitespace_submit),
        ("kill-yank", scenario_kill_yank),
        ("editor-motions", scenario_editor_motions),
        ("reply-spoof", scenario_reply_spoof),
        ("mouse-focus", scenario_mouse_focus_flood),
        ("giant-line", scenario_giant_line),
        ("binary-paste", scenario_binary_paste),
        ("paste-queue", scenario_paste_queue),
        ("selector-stream", scenario_selector_stream),
        ("kitchen-sink", scenario_kitchen_sink),
        ("sustained-pressure", scenario_sustained_pressure),
        ("undo-flood", scenario_undo_flood),
        ("command-injection", scenario_command_injection),
        ("preboot-input", scenario_preboot_input),
        ("post-quit-bytes", scenario_post_quit_bytes),
        ("extreme-geometry", scenario_extreme_geometry),
        ("grapheme-torture", scenario_grapheme_torture),
        ("wide-edge-sweep", scenario_wide_edge_sweep),
        ("x10-mouse", scenario_x10_mouse),
        ("history-pressure", scenario_history_pressure),
        ("reply-midword", scenario_reply_midword),
        ("unclosed-paste", scenario_unclosed_paste),
        ("byte-bomb", scenario_byte_bomb),
        ("alt-alpha-flood", scenario_alt_alpha_flood),
        ("composer-max-lines", scenario_composer_max_lines),
        ("interleaved-bits", scenario_interleaved_bits),
    ];
    let mut verdicts = Vec::new();
    let mut first_failure: Option<String> = None;
    for (name, scenario) in scenarios {
        if only.as_deref().is_some_and(|filter| filter != *name) {
            continue;
        }
        let started = Instant::now();
        match scenario() {
            Ok(()) => {
                let ms = started.elapsed().as_millis();
                verdicts.push(format!("PASS {name} {ms}ms"));
            }
            Err(error) => {
                verdicts.push(format!("FAIL {name}: {error}"));
                first_failure = Some(format!("{name}: {error}"));
                break;
            }
        }
    }
    for verdict in &verdicts {
        eprintln!("{verdict}");
    }
    if let Some(failure) = first_failure {
        hard_fail(&failure);
    }
}

#[expect(
    clippy::panic,
    reason = "test hard-fail: gauntlet failure is irrecoverable"
)]
fn hard_fail(failure: &str) -> ! {
    panic!("adversarial gauntlet failed: {failure}");
}
