//! Single ownership handoff for terminal input.
//!
//! [`TerminalSession`] owns the terminal guard and the startup probe
//! lifecycle, coordinating the handoff of stdin from the probe collector
//! to the `EventStream` reader. The input handle itself is owned by the
//! product runtime (it needs the receiver in its `select!` loop); the
//! session takes `&TerminalInput` for the coordinated start/pause/resume
//! steps so the ordering convention lives here, not in the product.
//!
//! ## Why `finish_probe` + `start_input` (split, not `complete`)
//!
//! Probe-window keystroke events go to the product's own reinject queue
//! (`pending_ui_reinject`), not the input channel. The run loop drains
//! that queue before pulling from the input channel, so the reader must
//! start only AFTER the product has queued the events. A single `complete`
//! that starts the reader would either (a) enqueue into the input channel
//! — changing the queue and the relative priority vs resize-coalesced
//! events — or (b) return events for the product to queue but start the
//! reader before the product queues them, breaking the ordering. The split
//! keeps both steps on one owner with the ordering explicit: `finish_probe`
//! → product queues → `start_input`.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::task::JoinHandle;

use ratatui::layout::Size;

use crate::component::UiEvent;
use crate::terminal::ScreenMode;
use crate::terminal::caps::TerminalCapabilities;
use crate::terminal::guard::TerminalGuard;
use crate::terminal::input::TerminalInput;
use crate::terminal::probe::{probe_collect_replies_with_yield, probe_write_batch};

/// Owns the terminal guard and startup probe lifecycle, coordinating the
/// single ownership handoff of stdin from the probe collector to the
/// `EventStream` reader.
///
/// The caller creates and activates the [`TerminalGuard`] (installing a
/// product-specific panic hook first), then hands it to [`Self::begin`]. The
/// session takes ownership and drives the probe → input handoff, editor
/// suspend/resume, and shutdown restore.
/// The probe collector's join result: discovered capabilities plus any
/// keystrokes that arrived during the probe window.
type ProbeJoin = io::Result<(TerminalCapabilities, Vec<UiEvent>)>;

/// Sole owner of the terminal input lifecycle: startup probing, the
/// probe-to-input handoff, editor suspend/resume, and shutdown restore.
pub struct TerminalSession<W: Write> {
    guard: TerminalGuard<W>,
    probe_task: Option<JoinHandle<ProbeJoin>>,
    probe_yield: Arc<AtomicBool>,
    enable_kitty: bool,
}

impl<W: Write> TerminalSession<W> {
    /// Take ownership of an activated guard, write the startup probe batch,
    /// spawn the blocking reply collector, and create the deferred input
    /// handle.
    ///
    /// Returns `(session, input)` — the input handle is owned by the caller
    /// (the product runtime needs its receiver in the event loop); the
    /// session retains the guard and probe join handle and coordinates the
    /// stdin ownership handoff.
    ///
    /// The caller is responsible for guard creation, panic-hook
    /// installation, and activation — the panic hook is product-specific
    /// and must be installed before activation. The probe batch write
    /// happens inside `begin` so the ordering invariant (probe bytes
    /// precede all sync output) is owned by the session, not the caller.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when writing or flushing the probe batch fails.
    pub fn begin(
        mut guard: TerminalGuard<W>,
        enable_kitty: bool,
        probe_caps: TerminalCapabilities,
    ) -> io::Result<(Self, TerminalInput)> {
        let issued = probe_write_batch(guard.writer_mut())?;
        let probe_yield = Arc::new(AtomicBool::new(false));
        let probe_task = issued.map(|issued| {
            let mut caps = probe_caps;
            let yield_now = Arc::clone(&probe_yield);
            tokio::task::spawn_blocking(move || {
                probe_collect_replies_with_yield(&mut caps, &yield_now, &issued)
                    .map(|pending| (caps, pending))
            })
        });
        let input = TerminalInput::deferred();
        Ok((
            Self {
                guard,
                probe_task,
                probe_yield,
                enable_kitty,
            },
            input,
        ))
    }

    /// Arm the probe yield and join the collector, returning the refined
    /// capabilities and probe-window keystroke events for product adoption.
    ///
    /// Does NOT start the reader — the caller must queue the pending events
    /// into its own reinject queue first, then call [`Self::start_input`].
    /// `fallback_caps` is returned when no probe task was spawned (stdin
    /// is not a terminal).
    ///
    /// # Errors
    ///
    /// Returns a string when the probe task fails or the collector returns
    /// an I/O error.
    pub async fn finish_probe(
        &mut self,
        fallback_caps: TerminalCapabilities,
    ) -> Result<(TerminalCapabilities, Vec<UiEvent>), String> {
        self.probe_yield.store(true, Ordering::Relaxed);
        match self.probe_task.take() {
            Some(handle) => match handle.await {
                Ok(Ok(joined)) => Ok(joined),
                Ok(Err(error)) => {
                    // Defined recovery on a latched reply protocol error:
                    // clear the latch so a session retry starts healthy; the
                    // failure still surfaces to the caller.
                    if crossterm::event::reply::is_protocol_error(&error) {
                        crossterm::event::reply::recover_protocol_error();
                    }
                    Err(format!("terminal probe failed: {error}"))
                }
                Err(error) => Err(format!("terminal probe task failed: {error}")),
            },
            None => Ok((fallback_caps, Vec::new())),
        }
    }

    /// Start the `EventStream` reader. Call only after [`Self::finish_probe`] and
    /// after the product has queued probe-window events into its reinject
    /// queue — the reader becomes the sole stdin owner from this point.
    pub fn start_input(&mut self, input: &mut TerminalInput) {
        input.start();
    }
    /// Pause the input reader and restore terminal modes for an external
    /// editor. The product runs the editor between this and
    /// [`Self::resume_from_editor`].
    ///
    /// # Errors
    ///
    /// Returns a string when the input pause fails.
    pub async fn suspend_for_editor(&mut self, input: &TerminalInput) -> Result<(), String> {
        input
            .pause()
            .await
            .map_err(|e| format!("pause terminal input for editor: {e}"))?;
        self.guard.restore();
        Ok(())
    }

    /// Pause the sole reader, switch guard-owned screen modes, query the
    /// terminal's current dimensions, and resume the same reader.
    ///
    /// The pause acknowledgment is awaited before any mode bytes are written;
    /// no second event stream or probe reader is created.
    ///
    /// # Errors
    ///
    /// Returns a string when pausing or resuming input, changing terminal
    /// modes, querying terminal dimensions, or restoring the previous mode
    /// fails.
    pub async fn switch_screen_mode(
        &mut self,
        input: &TerminalInput,
        mode: ScreenMode,
    ) -> Result<Size, String> {
        input
            .pause()
            .await
            .map_err(|e| format!("pause terminal input for screen-mode switch: {e}"))?;

        let previous_mode = self.guard.screen_mode();
        let mut lifecycle = self
            .guard
            .set_screen_mode(mode)
            .map_err(|e| format!("switch terminal to {mode}: {e}"))
            .and_then(|()| {
                fresh_terminal_size()
                    .map_err(|e| format!("query terminal size after switching to {mode}: {e}"))
            });

        if lifecycle.is_err()
            && previous_mode != mode
            && let Err(error) = self.guard.set_screen_mode(previous_mode)
        {
            lifecycle = Err(format!(
                "{}; failed to restore {previous_mode} after the failed switch: {error}",
                lifecycle
                    .err()
                    .unwrap_or_else(|| "screen-mode switch failed".to_owned())
            ));
        }

        let resumed = input
            .resume(Vec::new())
            .await
            .map_err(|e| format!("resume terminal input after screen-mode switch: {e}"));

        let size = lifecycle?;
        resumed?;
        self.guard
            .set_viewport_bottom_row(size.height.saturating_sub(1));
        Ok(size)
    }

    /// Re-activate terminal modes and resume the input reader after an
    /// external editor returns.
    ///
    /// # Errors
    ///
    /// Returns a string when guard re-activation, size querying, or input
    /// resume fails. The dimensions are queried only after mode restoration.
    pub async fn resume_from_editor(&mut self, input: &TerminalInput) -> Result<Size, String> {
        let lifecycle = self
            .guard
            .resume(self.enable_kitty)
            .map_err(|e| format!("terminal resume after editor failed: {e}"))
            .and_then(|()| {
                fresh_terminal_size()
                    .map_err(|e| format!("query terminal size after editor: {e}"))
            });
        let resumed = input
            .resume(Vec::new())
            .await
            .map_err(|e| format!("resume terminal input after editor: {e}"));

        let size = lifecycle?;
        resumed?;
        self.guard
            .set_viewport_bottom_row(size.height.saturating_sub(1));
        Ok(size)
    }

    /// Suspend terminal modes and raise SIGTSTP (ctrl+Z path). Does NOT
    /// pause the input reader — the process is suspended, not the reader.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if suspending the process fails.
    pub fn suspend(&mut self) -> io::Result<()> {
        self.guard.suspend()
    }

    /// Re-activate modes after SIGCONT and return freshly queried dimensions.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when terminal modes cannot be re-enabled or the
    /// terminal size cannot be queried.
    pub fn resume(&mut self) -> io::Result<Size> {
        self.guard.resume(self.enable_kitty)?;
        let size = fresh_terminal_size()?;
        self.guard
            .set_viewport_bottom_row(size.height.saturating_sub(1));
        Ok(size)
    }

    /// Restore terminal modes. The input reader is stopped by dropping the
    /// `TerminalInput` handle (owned by the product runtime) before calling
    /// this — dropping the handle ends the input task, then this call
    /// restores modes in the documented order.
    pub fn shutdown(mut self) {
        self.guard.restore();
    }

    /// Current guard-owned screen mode.
    #[must_use]
    pub fn screen_mode(&self) -> ScreenMode {
        self.guard.screen_mode()
    }

    /// Borrow the guard for viewport updates (operations that do not
    /// involve the input reader).
    pub fn guard_mut(&mut self) -> &mut TerminalGuard<W> {
        &mut self.guard
    }
}


fn fresh_terminal_size() -> io::Result<Size> {
    crossterm::terminal::size().map(|(width, height)| Size::new(width, height))
}