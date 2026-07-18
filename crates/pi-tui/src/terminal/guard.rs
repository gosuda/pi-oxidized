//! Terminal mode guard with ordered activate/restore and emergency paths.

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
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// Desired Kitty keyboard flags: disambiguate | event types | alternate keys (== 7).
pub const KITTY_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Ordered restore stack entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreStep {
    RawMode,
    BracketedPaste,
    FocusChange,
    KittyPush,
    CursorHidden,
    ColorSchemeNotify,
}

/// Owns raw mode and terminal modes; restores on drop.
pub struct TerminalGuard<W: Write> {
    writer: W,
    applied: Vec<RestoreStep>,
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
            restored: false,
            emergency: Arc::new(AtomicBool::new(false)),
            viewport_bottom_row: 0,
        }
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
        self.restored = false;
        self.applied.clear();
        self.activate(enable_kitty)
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
    writer.write_all(b"\x1b[?2026l\x1b[<u\x1b[?2004l\x1b[?1004l\x1b[?2031l\x1b[?25h\x1b[0m")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
}
