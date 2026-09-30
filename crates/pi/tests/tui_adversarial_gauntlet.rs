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

fn require_prerequisites() -> Result<(), AdvError> {
    let _ = pi_binary()?;
    let host = workspace_root()?
        .join("packages")
        .join("extension-host")
        .join("dist")
        .join(if cfg!(windows) {
            "pi-extension-host.exe"
        } else {
            "pi-extension-host"
        });
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
        workspace_root()?
            .join("packages/extension-host/dist/pi-extension-host")
            .to_string_lossy()
            .into_owned(),
    );
    env.insert("PI_VERIFICATION_MODE".to_owned(), "text".to_owned());
    env.insert("PI_VERIFICATION_CHUNK_COUNT".to_owned(), "3".to_owned());
    env.insert("PI_VERIFICATION_CHUNK_DELAY_MS".to_owned(), "0".to_owned());
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
        Ok(pi_tui::testkit::conpty::ConPtyDriver::open_clean(spec)?)
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

    /// Empty the composer by backspacing generously, then settle ready.
    fn clear_editor(&mut self) -> Result<(), AdvError> {
        for _ in 0..64 {
            self.write_input(KEY_BACKSPACE)?;
        }
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
        b"\x1b]8;;http://x", // unterminated OSC ...
        b"\x1b\\",           // ... closed by ST (a bare \x07 here is ctrl+g → external editor)
    ];
    for (index, burst) in bursts.iter().enumerate() {
        run.write_input(burst)?;
        // Liveness probe between hostile bursts — proves no wedge.
        run.prove_editor_focus(scenario, &format!("ii{index}"))?;
    }

    // An open bracketed paste must capture subsequent keys as paste content:
    // `ii9` is typed while the paste is unclosed and must NOT appear on the
    // composer; after `\x1b[201~` closes the paste, liveness returns.
    run.write_input(b"\x1b[200~unclosed paste body\nnext\n")?;
    run.write_input(b" ii9")?;
    let mid_paste = run.settle_screen(|_| true)?;
    if mid_paste.lines.iter().any(|line| line.contains("ii9")) {
        return Err(AdvError::Assert(format!(
            "{scenario}: keys typed inside an open bracketed paste leaked to the composer; screen:\n{}",
            mid_paste.lines.join("\n")
        )));
    }
    run.write_input(b"\x1b[201~")?;
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
    ];
    let mut verdicts = Vec::new();
    let mut first_failure: Option<String> = None;
    for (name, scenario) in scenarios {
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
