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

use crate::component::UiEvent;
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
        let probe_written = probe_write_batch(guard.writer_mut())?;
        let probe_yield = Arc::new(AtomicBool::new(false));
        let probe_task = probe_written.then(|| {
            let mut caps = probe_caps;
            let yield_now = Arc::clone(&probe_yield);
            tokio::task::spawn_blocking(move || {
                probe_collect_replies_with_yield(&mut caps, &yield_now)
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
                Ok(Err(error)) => Err(format!("terminal probe failed: {error}")),
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

    /// Re-activate terminal modes and resume the input reader after an
    /// external editor returns.
    ///
    /// # Errors
    ///
    /// Returns a string when guard re-activation or input resume fails.
    pub async fn resume_from_editor(&mut self, input: &TerminalInput) -> Result<(), String> {
        self.guard
            .resume(self.enable_kitty)
            .map_err(|e| format!("terminal resume after editor failed: {e}"))?;
        input
            .resume(Vec::new())
            .await
            .map_err(|e| format!("resume terminal input after editor: {e}"))?;
        Ok(())
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

    /// Re-activate terminal modes after SIGCONT.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when terminal modes cannot be re-enabled.
    pub fn resume(&mut self) -> io::Result<()> {
        self.guard.resume(self.enable_kitty)
    }

    /// Restore terminal modes. The input reader is stopped by dropping the
    /// `TerminalInput` handle (owned by the product runtime) before calling
    /// this — dropping the handle ends the input task, then this call
    /// restores modes in the documented order.
    pub fn shutdown(mut self) {
        self.guard.restore();
    }

    /// Borrow the guard for viewport updates (operations that do not
    /// involve the input reader).
    pub fn guard_mut(&mut self) -> &mut TerminalGuard<W> {
        &mut self.guard
    }
}
