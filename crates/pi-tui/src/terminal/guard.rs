//! Terminal mode guard with ordered activate/restore and emergency paths.

use std::fmt;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};

use crate::terminal::ScreenMode;

/// Desired Kitty keyboard flags: disambiguate | event types | alternate keys (= 7).
pub const KITTY_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Complete signal-safe terminal recovery sequence for a process that entered fullscreen.
///
/// Mouse modes and wrapping are always restored. The emergency writer omits
/// alternate-screen exit until this process has entered it, preserving a
/// parent application's alternate screen.
pub const EMERGENCY_RESTORE_BYTES: &[u8] = b"\x1b[?2026l\
\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\
\x1b[?7h\x1b[?1049l\x1b[<u\x1b[?2004l\
\x1b[?1004l\x1b[?2031l\x1b[?25h\x1b[0m";

/// Complete signal-safe terminal recovery sequence for a process that has not entered fullscreen.
pub const EMERGENCY_REGULAR_RESTORE_BYTES: &[u8] = b"\x1b[?2026l\
\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\
\x1b[?7h\x1b[<u\x1b[?2004l\
\x1b[?1004l\x1b[?2031l\x1b[?25h\x1b[0m";

// Historical rather than guard-local: rollback/drop must not erase emergency
// recovery coverage after a successful entry write.
static ENTERED_ALTERNATE_SCREEN: AtomicBool = AtomicBool::new(false);

/// Ordered restore stack entry for regular terminal modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreStep {
    RawMode,
    BracketedPaste,
    FocusChange,
    KittyPush,
    CursorHidden,
    ColorSchemeNotify,
}

/// Ordered fullscreen mode activation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullscreenStep {
    AlternateScreen,
    AutowrapDisabled,
    MouseNormal,
    MouseButton,
    MouseAll,
    MouseSgr,
}

/// Compound fullscreen-activation failure.
///
/// Carries the primary activation error plus, when the best-effort unwind
/// also failed, its error. The outer [`io::Error`] preserves the primary
/// [`io::ErrorKind`]; the original primary error stays reachable through
/// [`std::error::Error::source`].
#[derive(Debug)]
struct FullscreenActivationError {
    primary: io::Error,
    rollback: Option<io::Error>,
}

impl fmt::Display for FullscreenActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "fullscreen activation failed: {}", self.primary)?;
        if let Some(rollback) = &self.rollback {
            write!(formatter, "; restore also failed: {rollback}")?;
        }
        Ok(())
    }
}

impl std::error::Error for FullscreenActivationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

/// Owns raw mode and terminal modes; restores on drop.
pub struct TerminalGuard<W: Write> {
    writer: W,
    applied: Vec<RestoreStep>,
    fullscreen_applied: Vec<FullscreenStep>,
    screen_mode: ScreenMode,
    restored: bool,
    emergency: Arc<AtomicBool>,
    viewport_bottom_row: u16,
}

impl<W: Write> TerminalGuard<W> {
    /// Create a guard that has not yet activated any modes.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            applied: Vec::new(),
            fullscreen_applied: Vec::new(),
            screen_mode: ScreenMode::Regular,
            restored: false,
            emergency: Arc::new(AtomicBool::new(false)),
            viewport_bottom_row: 0,
        }
    }

    /// Current guard-owned screen mode.
    #[must_use]
    pub fn screen_mode(&self) -> ScreenMode {
        self.screen_mode
    }

    /// Shared emergency flag for panic/signal hooks.
    #[must_use]
    pub fn emergency_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.emergency)
    }

    /// Borrow the writer.
    pub fn writer(&self) -> &W {
        &self.writer
    }

    /// Borrow the writer mutably.
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Update the last known viewport bottom row used when parking the cursor.
    pub fn set_viewport_bottom_row(&mut self, row: u16) {
        self.viewport_bottom_row = row;
    }

    /// Activate modes in the mandated order.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when raw mode or a terminal write cannot be enabled.
    pub fn activate(&mut self, enable_kitty: bool) -> io::Result<()> {
        if self.restored {
            return Err(io::Error::other("terminal guard already restored"));
        }
        enable_raw_mode()?;
        self.applied.push(RestoreStep::RawMode);

        queue!(self.writer, EnableBracketedPaste)?;
        self.applied.push(RestoreStep::BracketedPaste);

        queue!(self.writer, EnableFocusChange)?;
        self.applied.push(RestoreStep::FocusChange);

        if enable_kitty {
            queue!(
                self.writer,
                PushKeyboardEnhancementFlags(KITTY_KEYBOARD_FLAGS)
            )?;
            self.applied.push(RestoreStep::KittyPush);
        }

        queue!(self.writer, Hide)?;
        self.applied.push(RestoreStep::CursorHidden);

        // Best-effort color scheme notify (OSC ? 2031 h). Unsupported terminals ignore it.
        self.writer.write_all(b"\x1b[?2031h")?;
        self.applied.push(RestoreStep::ColorSchemeNotify);

        self.writer.flush()?;
        Ok(())
    }

    /// Enter the fullscreen terminal modes while retaining the same writer.
    ///
    /// Every successful mode write is recorded before the next step is
    /// attempted. Activation runs as one fallible path — the ordered steps
    /// then the flush — and on failure the recorded prefix is unwound exactly
    /// once, in the opposite order. The returned error preserves the primary
    /// [`io::ErrorKind`] and original error and additionally reports a
    /// rollback (unwind) failure instead of discarding it.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the guard is already restored, fullscreen
    /// mode activation fails, or its rollback cannot complete.
    pub fn enter_fullscreen(&mut self) -> io::Result<()> {
        if self.restored {
            return Err(io::Error::other("terminal guard already restored"));
        }
        if self.screen_mode == ScreenMode::Fullscreen {
            return Ok(());
        }
        if self.applied.is_empty() {
            self.activate(false)?;
        }

        let mut steps = vec![
            FullscreenStep::AlternateScreen,
            FullscreenStep::AutowrapDisabled,
            FullscreenStep::MouseNormal,
            FullscreenStep::MouseButton,
        ];
        if !multiplexer_detected() {
            steps.push(FullscreenStep::MouseAll);
        }
        steps.push(FullscreenStep::MouseSgr);

        if let Err(primary) = self.activate_fullscreen_steps(&steps) {
            let rollback = self.restore_fullscreen_modes().err();
            let kind = primary.kind();
            return Err(io::Error::new(
                kind,
                FullscreenActivationError { primary, rollback },
            ));
        }
        self.screen_mode = ScreenMode::Fullscreen;
        Ok(())
    }

    /// One fallible activation path: apply every missing step, flushing the
    /// alternate-screen entry before later steps, then flush again at the end.
    fn activate_fullscreen_steps(&mut self, steps: &[FullscreenStep]) -> io::Result<()> {
        for step in steps {
            // A failed teardown keeps its mode recorded as unresolved; retain
            // that entry while adding only the missing fullscreen modes.
            if !self.fullscreen_applied.contains(step) {
                self.apply_fullscreen_step(*step)?;
            }
        }
        self.writer.flush()?;
        Ok(())
    }

    /// Leave fullscreen terminal modes without parking the cursor in
    /// scrollback. The regular inline cursor restoration remains deferred to
    /// the normal guard restore path.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when fullscreen mode teardown cannot complete.
    pub fn leave_fullscreen(&mut self) -> io::Result<()> {
        if self.restored {
            self.screen_mode = ScreenMode::Regular;
            self.fullscreen_applied.clear();
            return Ok(());
        }
        if self.screen_mode == ScreenMode::Regular && self.fullscreen_applied.is_empty() {
            return Ok(());
        }
        let result = self.restore_fullscreen_modes();
        // Report Regular even after a failed teardown so a rollback can
        // re-enter and rebuild the complete fullscreen set. Failed steps
        // remain recorded for a direct retry or final restore.
        self.screen_mode = ScreenMode::Regular;
        result
    }

    /// Switch only the guard-owned terminal modes.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when switching the guard-owned terminal modes
    /// cannot complete.
    pub fn set_screen_mode(&mut self, mode: ScreenMode) -> io::Result<()> {
        match mode {
            ScreenMode::Regular => self.leave_fullscreen(),
            ScreenMode::Fullscreen => self.enter_fullscreen(),
        }
    }

    fn apply_fullscreen_step(&mut self, step: FullscreenStep) -> io::Result<()> {
        match step {
            FullscreenStep::AlternateScreen => {
                queue!(self.writer, EnterAlternateScreen)?;
                // `queue!` may only buffer the entry. Arm emergency cleanup
                // after the flush that proves the terminal saw it.
                self.writer.flush()?;
                ENTERED_ALTERNATE_SCREEN.store(true, Ordering::Release);
            }
            FullscreenStep::AutowrapDisabled => {
                queue!(self.writer, DisableLineWrap)?;
            }
            FullscreenStep::MouseNormal => self.writer.write_all(b"\x1b[?1000h")?,
            FullscreenStep::MouseButton => self.writer.write_all(b"\x1b[?1002h")?,
            FullscreenStep::MouseAll => self.writer.write_all(b"\x1b[?1003h")?,
            FullscreenStep::MouseSgr => self.writer.write_all(b"\x1b[?1006h")?,
        }
        self.fullscreen_applied.push(step);
        Ok(())
    }

    fn restore_fullscreen_modes(&mut self) -> io::Result<()> {
        if self.fullscreen_applied.is_empty() && self.screen_mode == ScreenMode::Regular {
            return Ok(());
        }
        let mut first_error = None;
        // Walk the activation stack in reverse (last activated first), removing
        // only the steps that successfully restore. Failed steps stay on the
        // applied list so the caller can retry.
        let mut i = self.fullscreen_applied.len();
        while i > 0 {
            i -= 1;
            let step = self.fullscreen_applied[i];
            let result = match step {
                FullscreenStep::AlternateScreen => queue!(self.writer, LeaveAlternateScreen),
                FullscreenStep::AutowrapDisabled => queue!(self.writer, EnableLineWrap),
                FullscreenStep::MouseNormal => self.writer.write_all(b"\x1b[?1000l"),
                FullscreenStep::MouseButton => self.writer.write_all(b"\x1b[?1002l"),
                FullscreenStep::MouseAll => self.writer.write_all(b"\x1b[?1003l"),
                FullscreenStep::MouseSgr => self.writer.write_all(b"\x1b[?1006l"),
            };
            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            self.fullscreen_applied.remove(i);
        }
        // Show after leaving 1049 so a hot switch never emits inline cursor
        // parking or a scroll while the alternate screen is still active.
        if let Err(error) = queue!(self.writer, Show)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.writer.flush()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Suspend modes without dropping (ctrl+Z path).
    ///
    /// # Errors
    ///
    /// Returns an I/O error if suspending the process fails.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.restore_modes(false);
        #[cfg(unix)]
        {
            nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP)
                .map_err(|err| io::Error::other(format!("failed to raise SIGTSTP: {err}")))?;
        }
        Ok(())
    }

    /// Re-apply modes after resume.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when terminal modes cannot be re-enabled.
    pub fn resume(&mut self, enable_kitty: bool) -> io::Result<()> {
        let desired_mode = self.screen_mode;
        self.restored = false;
        self.applied.clear();
        self.fullscreen_applied.clear();
        self.screen_mode = ScreenMode::Regular;
        self.activate(enable_kitty)?;
        if desired_mode == ScreenMode::Fullscreen {
            self.enter_fullscreen()?;
        }
        Ok(())
    }

    /// Explicit restore (normal unwind).
    ///
    /// Restore is best-effort; individual terminal errors are intentionally ignored.
    pub fn restore(&mut self) {
        self.restore_modes(true);
    }

    /// Idempotent emergency restore for panic/signal handlers.
    pub fn emergency_restore(&mut self) {
        if self
            .emergency
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        // Mode restore only. Stage-3 transactions close DEC synchronized output
        // themselves; an unpaired CSI ? 2026 l here breaks check-6 balance on
        // clean emergency paths. Interrupted-frame close remains available via
        // [`write_emergency_restore_bytes`] for true signal-safe handlers.
        self.restore_modes(true);
    }

    /// Drain residual input after keyboard protocol pop (default 1000/50 ms).
    pub fn drain_input(&self, max: Duration, idle: Duration) {
        let start = Instant::now();
        let mut last = Instant::now();
        while start.elapsed() < max {
            match crossterm::event::poll(Duration::from_millis(5)) {
                Ok(true) => {
                    let _ = crossterm::event::read();
                    last = Instant::now();
                }
                Ok(false) => {
                    if last.elapsed() >= idle {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn restore_modes(&mut self, drain: bool) {
        if self.restored {
            return;
        }
        // Fullscreen teardown must precede the regular cursor parking below:
        // the inline MoveTo + CRLF sequence is forbidden while 1049 is active.
        let mode = self.screen_mode;
        let _ = self.restore_fullscreen_modes();
        self.screen_mode = mode;
        self.restored = true;

        // Stage-3 transactions always close DEC synchronized output themselves.
        // Do not emit an unpaired CSI ? 2026 l here: check 6 and ByteAuditReport
        // require balanced markers across clean exits. Emergency restore still
        // closes sync via [`write_emergency_restore_bytes`].
        // Reverse order of applied modes.
        while let Some(step) = self.applied.pop() {
            match step {
                RestoreStep::ColorSchemeNotify => {
                    let _ = self.writer.write_all(b"\x1b[?2031l");
                }
                RestoreStep::CursorHidden => {
                    let row = self.viewport_bottom_row.saturating_add(1);
                    let _ = queue!(self.writer, MoveTo(0, row));
                    let _ = self.writer.write_all(b"\r\n");
                    let _ = queue!(self.writer, Show);
                }
                RestoreStep::KittyPush => {
                    let _ = queue!(self.writer, PopKeyboardEnhancementFlags);
                    let _ = self.writer.write_all(b"\x1b[<u");
                }
                RestoreStep::FocusChange => {
                    let _ = queue!(self.writer, DisableFocusChange);
                }
                RestoreStep::BracketedPaste => {
                    let _ = queue!(self.writer, DisableBracketedPaste);
                }
                RestoreStep::RawMode => {
                    let _ = queue!(self.writer, ResetColor);
                    let _ = disable_raw_mode();
                }
            }
        }
        let _ = self.writer.flush();

        if drain {
            self.drain_input(Duration::from_secs(1), Duration::from_millis(50));
        }
    }
}

fn multiplexer_detected() -> bool {
    ["TMUX", "ZELLIJ", "STY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        || std::env::var_os("TERM")
            .and_then(|value| value.into_string().ok())
            .is_some_and(|term| term.starts_with("tmux") || term.starts_with("screen"))
}

impl<W: Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        self.restore_modes(true);
    }
}

/// Record of modes that would be applied (for unit tests without a real tty).
#[derive(Debug, Default, Clone)]
pub struct GuardScript {
    /// Applied steps in activation order.
    pub applied: Vec<&'static str>,
    /// Restore steps in reverse order.
    pub restored: Vec<&'static str>,
    emergency: bool,
}

impl GuardScript {
    /// Simulate activate ordering.
    pub fn activate(&mut self, enable_kitty: bool) {
        self.applied.push("raw");
        self.applied.push("bracketed_paste");
        self.applied.push("focus");
        if enable_kitty {
            self.applied.push("kitty");
        }
        self.applied.push("cursor_hidden");
        self.applied.push("color_scheme_notify");
    }

    /// Simulate the fullscreen activation order.
    pub fn enter_fullscreen(&mut self, multiplexer: bool) {
        self.applied.push("alternate_screen");
        self.applied.push("autowrap_disabled");
        self.applied.push("mouse_normal");
        self.applied.push("mouse_button");
        if !multiplexer {
            self.applied.push("mouse_all");
        }
        self.applied.push("mouse_sgr");
    }

    /// Simulate restore reverse ordering (no unpaired sync close).
    pub fn restore(&mut self) {
        if self.emergency {
            return;
        }
        for step in self.applied.iter().rev() {
            self.restored.push(*step);
        }
        self.restored.push("drain_input");
        self.emergency = true;
    }

    /// Idempotent emergency path records a defensive sync close then modes.
    pub fn emergency_restore(&mut self) {
        if self.emergency {
            return;
        }
        // Matches [`write_emergency_restore_bytes`] intent for interrupted frames
        // without requiring the production `TerminalGuard` path to unbalance
        // stage-3 markers on clean exits.
        self.restored.push("sync_close");
        for step in self.applied.iter().rev() {
            self.restored.push(*step);
        }
        self.restored.push("drain_input");
        self.emergency = true;
    }
}

/// Install a panic hook that invokes `restore` once, chaining the previous hook.
pub fn install_panic_emergency_hook(
    emergency: Arc<AtomicBool>,
    restore: Arc<dyn Fn() + Send + Sync>,
) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if emergency
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            restore();
        }
        previous(info);
    }));
}

/// Write the emergency restore sequence directly to a writer (signal-safe best effort).
///
/// # Errors
///
/// Returns an I/O error if writing or flushing the restore sequence fails.
pub fn write_emergency_restore_bytes<W: Write>(writer: &mut W) -> io::Result<()> {
    let bytes = if ENTERED_ALTERNATE_SCREEN.load(Ordering::Acquire) {
        EMERGENCY_RESTORE_BYTES
    } else {
        EMERGENCY_REGULAR_RESTORE_BYTES
    };
    writer.write_all(bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    static PANIC_HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn guard_script_restores_in_reverse() {
        let mut script = GuardScript::default();
        script.activate(true);
        script.restore();
        assert_eq!(
            script.applied,
            [
                "raw",
                "bracketed_paste",
                "focus",
                "kitty",
                "cursor_hidden",
                "color_scheme_notify"
            ]
        );
        // Normal restore must not emit an unpaired CSI ? 2026 l.
        assert_eq!(
            script.restored.first().copied(),
            Some("color_scheme_notify")
        );
        assert_eq!(script.restored.last().copied(), Some("drain_input"));
        assert!(!script.restored.contains(&"sync_close"));
        // Idempotent after normal restore.
        let len = script.restored.len();
        script.emergency_restore();
        assert_eq!(script.restored.len(), len);

        let mut emergency = GuardScript::default();
        emergency.activate(false);
        emergency.emergency_restore();
        assert_eq!(emergency.restored.first().copied(), Some("sync_close"));
    }

    #[test]
    fn emergency_bytes_close_sync_and_show_cursor() -> io::Result<()> {
        let mut cursor = Cursor::new(Vec::new());
        write_emergency_restore_bytes(&mut cursor)?;
        let bytes = cursor.into_inner();
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?2026l"));
        assert!(bytes.windows(4).any(|w| w == b"\x1b[<u"));
        assert!(bytes.windows(6).any(|w| w == b"\x1b[?25h"));
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?2004l"));
        Ok(())
    }

    #[test]
    fn updated_viewport_row_controls_normal_restore_cursor_park() {
        let mut guard = TerminalGuard::new(Cursor::new(Vec::new()));
        guard.applied.push(RestoreStep::CursorHidden);
        guard.set_viewport_bottom_row(6);

        guard.restore_modes(false);

        assert_eq!(guard.writer().get_ref(), b"\x1b[8;1H\r\n\x1b[?25h");
    }

    #[test]
    fn activate_queues_modes_on_writer() -> io::Result<()> {
        let mut guard = TerminalGuard::new(Cursor::new(Vec::new()));
        if guard.activate(true).is_ok() {
            let bytes = guard.writer().get_ref();
            assert!(bytes.windows(8).any(|w| w == b"\x1b[?2004h") || !bytes.is_empty());
            guard.restore();
        } else {
            let mut out = Cursor::new(Vec::new());
            write_emergency_restore_bytes(&mut out)?;
        }
        Ok(())
    }

    #[test]
    fn installed_panic_hook_restores_once_shares_cas_and_chains_previous() {
        let _lock = PANIC_HOOK_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::panic::take_hook();
        let previous_calls = Arc::new(AtomicUsize::new(0));
        let previous_calls_for_hook = Arc::clone(&previous_calls);
        std::panic::set_hook(Box::new(move |_| {
            previous_calls_for_hook.fetch_add(1, Ordering::SeqCst);
        }));

        let mut guard = TerminalGuard::new(Cursor::new(Vec::new()));
        guard.applied.push(RestoreStep::ColorSchemeNotify);
        let emergency = guard.emergency_flag();
        let restore_calls = Arc::new(AtomicUsize::new(0));
        let restore_calls_for_hook = Arc::clone(&restore_calls);
        install_panic_emergency_hook(
            Arc::clone(&emergency),
            Arc::new(move || {
                restore_calls_for_hook.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let panic_trigger = AtomicBool::new(false);

        let assertions = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(
                std::panic::catch_unwind(|| {
                    assert!(
                        panic_trigger.load(Ordering::Relaxed),
                        "intentional first panic"
                    );
                })
                .is_err()
            );
            assert!(emergency.load(Ordering::SeqCst));
            assert_eq!(restore_calls.load(Ordering::SeqCst), 1);
            assert_eq!(previous_calls.load(Ordering::SeqCst), 1);
            guard.emergency_restore();
            assert!(
                guard.writer().get_ref().is_empty(),
                "the hook and guard must share the same once flag"
            );

            assert!(
                std::panic::catch_unwind(|| {
                    assert!(
                        panic_trigger.load(Ordering::Relaxed),
                        "intentional second panic"
                    );
                })
                .is_err()
            );
            assert_eq!(restore_calls.load(Ordering::SeqCst), 1);
            assert_eq!(previous_calls.load(Ordering::SeqCst), 2);
        }));

        drop(std::panic::take_hook());
        std::panic::set_hook(original);
        if let Err(payload) = assertions {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn terminal_guard_emergency_restore_is_mode_only_and_idempotent() {
        let mut guard = TerminalGuard::new(Cursor::new(Vec::new()));
        guard.applied.push(RestoreStep::ColorSchemeNotify);
        let emergency = guard.emergency_flag();

        guard.emergency_restore();
        let first = guard.writer().get_ref().clone();
        guard.emergency_restore();

        assert!(emergency.load(Ordering::SeqCst));
        assert_eq!(guard.writer().get_ref(), &first);
        assert_eq!(first, b"\x1b[?2031l");
        assert!(!first.windows(8).any(|window| window == b"\x1b[?2026l"));
    }

    /// Writer that accepts writes up to a byte budget, then latches: every
    /// later write fails while every attempt is still recorded. The attempt
    /// log lets a test prove the rollback attempted every recorded step even
    /// after its first write failure.
    struct LatchingFailureWriter {
        bytes: Vec<u8>,
        attempted: Vec<Vec<u8>>,
        budget: usize,
    }

    impl Write for LatchingFailureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_all(buf)?;
            Ok(buf.len())
        }

        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            self.attempted.push(buf.to_vec());
            if self.bytes.len() + buf.len() > self.budget {
                return Err(io::Error::other("latched write failure"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fullscreen_activation_failure_reports_primary_and_rollback() -> io::Result<()> {
        // Budget 13 accepts exactly CSI ?1049h (8) + CSI ?7l (5); the third
        // activation write (CSI ?1000h) fails, and every later write fails.
        let mut guard = TerminalGuard::new(LatchingFailureWriter {
            bytes: Vec::new(),
            attempted: Vec::new(),
            budget: 13,
        });
        // Seed one applied step so enter_fullscreen skips real raw-mode
        // activation (enable_raw_mode needs a tty).
        guard.applied.push(RestoreStep::RawMode);

        let Err(error) = guard.enter_fullscreen() else {
            return Err(io::Error::other("activation must fail at the third step"));
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(
            error.to_string().contains("restore also failed"),
            "compound error must expose the rollback failure: {error}"
        );
        let compound = error
            .get_ref()
            .and_then(|payload| payload.downcast_ref::<FullscreenActivationError>())
            .ok_or_else(|| io::Error::other("compound activation payload"))?;
        assert_eq!(compound.primary.to_string(), "latched write failure");
        let rollback = compound
            .rollback
            .as_ref()
            .ok_or_else(|| io::Error::other("rollback failure must be reported, not swallowed"))?;
        assert_eq!(rollback.to_string(), "latched write failure");

        // Only the two successful activation steps reached the wire.
        assert_eq!(guard.writer().bytes, b"\x1b[?1049h\x1b[?7l");
        // Every activation step and every restore step must have attempted a
        // write — including the second restore step (CSI ?1049l) even though
        // the first restore write (CSI ?7h) already failed.
        let attempted: Vec<&[u8]> = guard.writer().attempted.iter().map(Vec::as_slice).collect();
        for expected in [
            b"\x1b[?1049h".as_slice(), // activation step 1 (recorded)
            b"\x1b[?7l".as_slice(),    // activation step 2 (recorded)
            b"\x1b[?1000h".as_slice(), // activation step 3 (primary failure)
            b"\x1b[?7h".as_slice(),    // restore step 2 (first rollback write fails)
            b"\x1b[?1049l".as_slice(), // restore step 1 (attempted anyway)
            b"\x1b[?25h".as_slice(),   // cursor Show unwind
        ] {
            assert!(
                attempted.contains(&expected),
                "missing write attempt {expected:?}"
            );
        }
        // A failed switch must not latch the fullscreen bookkeeping.
        assert_eq!(guard.screen_mode(), ScreenMode::Regular);
        Ok(())
    }

    #[test]
    fn leave_fullscreen_retains_failed_steps_and_reports_regular() {
        // Activation order: alternate screen, then autowrap, then SGR mouse.
        // Budget 15 lets the first two restore writes (SGR mouse 8,
        // autowrap 5) succeed, then fails the alternate-screen restore (8)
        // and Show. The failed step stays recorded for retry while the
        // logical mode reports Regular so a rollback can re-enter fullscreen.
        let mut guard = TerminalGuard::new(LatchingFailureWriter {
            bytes: Vec::new(),
            attempted: Vec::new(),
            budget: 15,
        });
        guard.applied.push(RestoreStep::RawMode);
        guard.fullscreen_applied = vec![
            FullscreenStep::AlternateScreen,
            FullscreenStep::AutowrapDisabled,
            FullscreenStep::MouseSgr,
        ];
        guard.screen_mode = ScreenMode::Fullscreen;

        let result = guard.leave_fullscreen();
        assert!(
            result.is_err(),
            "leave_fullscreen must report the failed restore"
        );
        assert_eq!(
            guard.screen_mode(),
            ScreenMode::Regular,
            "failed teardown must permit fullscreen rollback"
        );
        assert_eq!(
            guard.fullscreen_applied,
            [FullscreenStep::AlternateScreen],
            "only successfully restored steps should be removed"
        );

        // Increase the budget and retry; the remaining step, Show, and flush
        // now all succeed, so the mode remains Regular with no steps pending.
        guard.writer_mut().budget = 100;
        let result = guard.leave_fullscreen();
        assert!(result.is_ok(), "retry should succeed with a healthy writer");
        assert_eq!(
            guard.screen_mode(),
            ScreenMode::Regular,
            "mode remains Regular after full success"
        );
        assert!(guard.fullscreen_applied.is_empty(), "all steps restored");
    }

    #[test]
    fn failed_leave_reentry_reapplies_missing_fullscreen_steps() -> io::Result<()> {
        let mut guard = TerminalGuard::new(LatchingFailureWriter {
            bytes: Vec::new(),
            attempted: Vec::new(),
            budget: 15,
        });
        guard.applied.push(RestoreStep::RawMode);
        guard.fullscreen_applied = vec![
            FullscreenStep::AlternateScreen,
            FullscreenStep::AutowrapDisabled,
            FullscreenStep::MouseSgr,
        ];
        guard.screen_mode = ScreenMode::Fullscreen;

        let result = guard.leave_fullscreen();
        assert!(result.is_err(), "the seeded teardown must fail first");
        guard.writer_mut().budget = 100;

        // Re-entry keeps the unresolved mode and adds only missing modes.
        guard.enter_fullscreen()?;
        let mut expected = vec![
            FullscreenStep::AlternateScreen,
            FullscreenStep::AutowrapDisabled,
            FullscreenStep::MouseNormal,
            FullscreenStep::MouseButton,
        ];
        if !multiplexer_detected() {
            expected.push(FullscreenStep::MouseAll);
        }
        expected.push(FullscreenStep::MouseSgr);
        assert_eq!(guard.screen_mode(), ScreenMode::Fullscreen);
        assert_eq!(
            guard.fullscreen_applied, expected,
            "re-entry must not duplicate or lose fullscreen steps"
        );
        Ok(())
    }

    #[test]
    fn restored_fullscreen_guard_rejects_reentry() {
        let mut guard = TerminalGuard::new(Cursor::new(Vec::new()));
        guard.screen_mode = ScreenMode::Fullscreen;

        guard.restore_modes(false);

        let result = guard.enter_fullscreen();
        assert!(result.is_err(), "restored guards must reject re-entry");
        assert!(
            !guard
                .writer()
                .get_ref()
                .windows(8)
                .any(|window| window == b"\x1b[?1049h"),
            "re-entry must not report success without activation"
        );
    }
    // MUTATION RECIPE — reverting `enter_fullscreen` to the pre-fix shape
    // must fail the test above:
    //     if let Err(error) = self.activate_fullscreen_steps(&steps) {
    //         self.restore_fullscreen_modes(); // unwind result discarded
    //         return Err(error);
    //     }
    // The discarded unwind result drops the rollback error, so the
    // `rollback ... expect` assertion fails.
}
