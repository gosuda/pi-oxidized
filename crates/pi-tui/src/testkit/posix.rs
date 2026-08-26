//! POSIX PTY adapter backed by `portable-pty` `UnixPtySystem`.

#![cfg(unix)]

use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsFd;

use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
use portable_pty::unix::UnixPtySystem;
use portable_pty::{CommandBuilder, MasterPty, PtySize, PtySystem};

use super::transcript::DriverKind;
use crate::testkit::driver::{
    DriverError, DriverSession, ExitStatus, Geometry, LaunchSpec, OutputBatch, RenderSession,
    SettlePolicy, SettledFrame, TerminalDriver,
};
use crate::testkit::session::{SessionIo, apply_env, snapshot_from_raw};

/// POSIX PTY driver using `portable-pty`'s Unix backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct PosixPtyDriver;

impl TerminalDriver for PosixPtyDriver {
    type Session = PosixPtySession;

    fn kind(&self) -> DriverKind {
        DriverKind::PosixPty
    }

    fn open(&self, spec: &LaunchSpec) -> Result<Self::Session, DriverError> {
        spec.validate()?;
        let system = UnixPtySystem::default();
        let pair = system
            .openpty(PtySize {
                rows: spec.geometry.rows,
                cols: spec.geometry.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(DriverError::pty)?;

        let mut argv = Vec::with_capacity(spec.argv.len());
        for arg in &spec.argv {
            argv.push(std::ffi::OsString::from(arg));
        }
        let mut cmd = CommandBuilder::from_argv(argv);
        cmd.cwd(&spec.cwd);
        apply_env(&mut cmd, spec);

        let child = pair.slave.spawn_command(cmd).map_err(DriverError::pty)?;
        drop(pair.slave);

        disable_pty_echo(pair.master.as_ref())?;

        let mut writer = pair.master.take_writer().map_err(DriverError::pty)?;
        let reader = pair.master.try_clone_reader().map_err(DriverError::pty)?;

        let probe = spec.profile.probe_reply();
        if !probe.is_empty() {
            writer.write_all(probe)?;
            writer.flush()?;
        }

        let pump = crate::testkit::session::ReaderPump::from_reader(reader);
        Ok(PosixPtySession {
            master: pair.master,
            child: Some(child),
            io: SessionIo::new(writer, pump),
            geometry: spec.geometry,
        })
    }
}

/// Render-capable POSIX PTY session.
pub struct PosixPtySession {
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    io: SessionIo,
    geometry: Geometry,
}

impl PosixPtySession {
    fn ensure_open(&self) -> Result<(), DriverError> {
        if self.io.closed {
            Err(DriverError::Closed)
        } else {
            Ok(())
        }
    }
}

impl DriverSession for PosixPtySession {
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
        self.io.closed = true;
        // Writer EOF first, then wait for the child, then join the reader.
        self.io.close_writer();
        let mut child = self.child.take().ok_or(DriverError::Closed)?;
        let wait_result = child.wait().map_err(|err| {
            DriverError::Io(std::io::Error::new(
                err.kind(),
                format!("posix pty child wait failed: {err}"),
            ))
        });
        let join_result = self.io.join_readers();
        let status = wait_result?;
        join_result?;
        Ok(status.into())
    }
}

impl RenderSession for PosixPtySession {
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
            .map_err(DriverError::pty)?;
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
}

impl Drop for PosixPtySession {
    fn drop(&mut self) {
        if !self.io.closed {
            self.io.closed = true;
            self.io.close_writer();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = self.io.join_readers();
        }
    }
}

fn disable_pty_echo(master: &dyn MasterPty) -> Result<(), DriverError> {
    let raw = master.as_raw_fd().ok_or_else(|| {
        DriverError::Pty("posix pty master has no raw fd for echo disable".to_owned())
    })?;
    // Re-open the master descriptor without unsafe FromRawFd.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/fd/{raw}"))?;
    let mut termios = tcgetattr(file.as_fd()).map_err(|err| {
        DriverError::Io(std::io::Error::other(format!(
            "tcgetattr failed while disabling echo: {err}"
        )))
    })?;
    termios.local_flags.remove(LocalFlags::ECHO);
    termios.local_flags.remove(LocalFlags::ECHOE);
    termios.local_flags.remove(LocalFlags::ECHOK);
    termios.local_flags.remove(LocalFlags::ECHONL);
    tcsetattr(file.as_fd(), SetArg::TCSANOW, &termios).map_err(|err| {
        DriverError::Io(std::io::Error::other(format!(
            "tcsetattr failed while disabling echo: {err}"
        )))
    })?;
    Ok(())
}
