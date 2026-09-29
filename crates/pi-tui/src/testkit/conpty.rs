//! Windows `ConPTY` adapter backed by `portable-pty` `ConPtySystem`.

#![cfg(windows)]

use portable_pty::win::conpty::ConPtySystem;
use portable_pty::{CommandBuilder, MasterPty, PtySize, PtySystem};

use super::transcript::DriverKind;
use crate::testkit::driver::{
    DriverError, DriverSession, ExitStatus, Geometry, LaunchSpec, OutputBatch, RenderSession,
    SettlePolicy, SettledFrame, TerminalDriver, TerminalSnapshot,
};
use crate::testkit::session::{
    POST_KILL_GRACE, SessionIo, apply_env, prepare_posix_base_environment, reap_child,
    snapshot_from_raw, viewport_snapshot_from_raw,
};

/// Windows `ConPTY` driver using `portable-pty` 0.9.0 `ConPtySystem`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConPtyDriver;

impl ConPtyDriver {
    /// Opens a `ConPTY` with the documented clean-environment contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the launch spec is invalid or the PTY cannot be opened.
    pub fn open_clean(spec: &LaunchSpec) -> Result<ConPtySession, DriverError> {
        Self::open_with_env(spec, |cmd, spec| {
            prepare_posix_base_environment(cmd, spec);
            apply_env(cmd, spec);
        })
    }

    fn open_with_env(
        spec: &LaunchSpec,
        configure_env: impl FnOnce(&mut CommandBuilder, &LaunchSpec),
    ) -> Result<ConPtySession, DriverError> {
        spec.validate()?;
        let system = ConPtySystem::default();
        let pair = system
            .openpty(PtySize {
                rows: spec.geometry.rows,
                cols: spec.geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| DriverError::pty(&err))?;

        let mut argv = Vec::with_capacity(spec.argv.len());
        for arg in &spec.argv {
            argv.push(std::ffi::OsString::from(arg));
        }
        let mut cmd = CommandBuilder::from_argv(argv);
        cmd.cwd(&spec.cwd);
        configure_env(&mut cmd, spec);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| DriverError::pty(&err))?;
        drop(pair.slave);

        let raw_writer = pair
            .master
            .take_writer()
            .map_err(|err| DriverError::pty(&err))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| DriverError::pty(&err))?;

        // Wrap in SharedWriter so the DSR auto-responder can share the single
        // PTY master writer (portable-pty allows only one take_writer call).
        let shared = crate::testkit::session::SharedWriter::new(raw_writer);
        let dsr_writer = shared.clone_handle();

        let pump = crate::testkit::session::ReaderPump::from_reader_with_probe_responder(
            reader,
            dsr_writer,
            spec.profile,
        );
        Ok(ConPtySession {
            master: pair.master,
            child: Some(child),
            io: SessionIo::new(Box::new(shared), pump),
            geometry: spec.geometry,
        })
    }
}

impl TerminalDriver for ConPtyDriver {
    type Session = ConPtySession;

    fn kind(&self) -> DriverKind {
        DriverKind::ConPty
    }

    fn open(&self, spec: &LaunchSpec) -> Result<Self::Session, DriverError> {
        Self::open_with_env(spec, apply_env)
    }
}

/// Render-capable `ConPTY` session.
pub struct ConPtySession {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    io: SessionIo,
    geometry: Geometry,
}

impl ConPtySession {
    fn ensure_open(&self) -> Result<(), DriverError> {
        if self.io.closed {
            Err(DriverError::Closed)
        } else {
            Ok(())
        }
    }
}

impl DriverSession for ConPtySession {
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        self.io.write_all(bytes)
    }

    fn read_output<F>(
        &mut self,
        policy: &SettlePolicy,
        predicate: F,
    ) -> Result<OutputBatch, DriverError>
    where
        F: FnMut(&[u8]) -> bool,
    {
        self.io.read_output(policy, predicate)
    }

    fn close(mut self) -> Result<ExitStatus, DriverError> {
        self.ensure_open()?;
        // Serve-mode children wait for the Ctrl+D terminator; offer it before
        // closing the writer so a live child can exit itself instead of eating
        // SIGKILL at the deadline (mirrors the POSIX close).
        let _ = self.io.write_all(b"\n\x04");
        self.io.closed = true;
        self.io.close_writer();
        let mut child = self.child.take().ok_or(DriverError::Closed)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let wait_result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(None) => {
                    if let Err(err) = child.kill() {
                        // Race: the child may have exited between the last
                        // try_wait and the kill (Windows has no std guarantee
                        // for kill on an exited process). Reap the winner
                        // instead of reporting a wait failure.
                        break match child.try_wait() {
                            Ok(Some(status)) => Ok(status),
                            _ => Err(err),
                        };
                    }
                    break match reap_child(&mut child, POST_KILL_GRACE) {
                        Ok(Some(status)) => Ok(status),
                        Ok(None) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "conpty child did not exit after kill",
                        )),
                        Err(err) => Err(err),
                    };
                }
                Err(err) => break Err(err),
            }
        }
        .map_err(|err| {
            DriverError::Io(std::io::Error::new(
                err.kind(),
                format!("conpty child wait failed: {err}"),
            ))
        });
        // Shed before joining: on a post-kill timeout the reader may never see
        // EOF either, so returning here lets Drop detach it. Only a reaped
        // child reaches the join below.
        let status = wait_result?;
        let _ = child.kill();
        let join_result = self.io.join_readers();
        join_result?;
        Ok(status.into())
    }
}

impl RenderSession for ConPtySession {
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), DriverError> {
        self.ensure_open()?;
        let geometry = Geometry::new(cols, rows)?;
        self.master
            .resize(PtySize {
                rows: geometry.rows,
                cols: geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| DriverError::pty(&err))?;
        self.geometry = geometry;
        Ok(())
    }

    fn resize_storm(&mut self, sizes: &[(u16, u16)]) -> Result<(), DriverError> {
        for &(cols, rows) in sizes {
            self.resize(cols, rows)?;
        }
        Ok(())
    }

    fn read_settled_frame<F>(
        &mut self,
        policy: &SettlePolicy,
        predicate: F,
    ) -> Result<SettledFrame, DriverError>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let batch = self.read_output(policy, predicate)?;
        let snapshot = snapshot_from_raw(self.io.ledger.raw_log(), self.geometry);
        Ok(SettledFrame { batch, snapshot })
    }

    fn read_settled_frame_where<F>(
        &mut self,
        policy: &SettlePolicy,
        mut predicate: F,
    ) -> Result<SettledFrame, DriverError>
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        let geometry = self.geometry;
        let mut cached_len = usize::MAX;
        let mut cached = viewport_snapshot_from_raw(&[], geometry);
        let batch = self.io.read_output_where(policy, |ledger| {
            if ledger.raw_log().len() != cached_len {
                cached_len = ledger.raw_log().len();
                cached = viewport_snapshot_from_raw(ledger.raw_log(), geometry);
            }
            predicate(&cached)
        })?;
        let snapshot = viewport_snapshot_from_raw(self.io.ledger.raw_log(), self.geometry);
        Ok(SettledFrame { batch, snapshot })
    }
}

impl Drop for ConPtySession {
    fn drop(&mut self) {
        if !self.io.closed {
            self.io.closed = true;
            self.io.close_writer();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                // Drop cannot report: reap bounded, then join only a reaped
                // child. On timeout the reader detaches in SessionIo::drop.
                if matches!(reap_child(&mut child, POST_KILL_GRACE), Ok(Some(_))) {
                    let _ = self.io.join_readers();
                }
            }
        }
    }
}
