//! QEMU user-mode smoke adapter over piped stdio.
//!
//! The session intentionally implements only [`DriverSession`]. Render verbs are
//! inexpressible at the type level.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use super::transcript::DriverKind;
use crate::testkit::driver::{
    DriverError, DriverSession, ExitStatus, LaunchSpec, OutputBatch, SettlePolicy, TerminalDriver,
};
use crate::testkit::session::{ReaderPump, SessionIo, apply_std_env};

/// QEMU user-mode contingency driver.
///
/// `qemu_prefix` is prepended to [`LaunchSpec::argv`] (for example
/// `["qemu-x86_64", "-L", "/usr/gnemul/qemu-x86_64"]`).
#[derive(Debug, Clone)]
pub struct QemuUserSmokeDriver {
    qemu_prefix: Vec<String>,
}

impl QemuUserSmokeDriver {
    /// Creates a driver with an explicit QEMU argv prefix.
    pub fn new(qemu_prefix: Vec<String>) -> Result<Self, DriverError> {
        if qemu_prefix.is_empty() || qemu_prefix[0].is_empty() {
            return Err(DriverError::InvalidSpec(
                "qemu prefix must include a non-empty emulator binary".to_owned(),
            ));
        }
        Ok(Self { qemu_prefix })
    }

    /// Returns the configured QEMU argv prefix.
    pub fn qemu_prefix(&self) -> &[String] {
        &self.qemu_prefix
    }
}

impl TerminalDriver for QemuUserSmokeDriver {
    type Session = QemuUserSmokeSession;

    fn kind(&self) -> DriverKind {
        DriverKind::QemuUserSmoke
    }

    fn open(&self, spec: &LaunchSpec) -> Result<Self::Session, DriverError> {
        spec.validate()?;

        let mut argv = Vec::with_capacity(self.qemu_prefix.len() + spec.argv.len());
        argv.extend(self.qemu_prefix.iter().cloned());
        argv.extend(spec.argv.iter().cloned());

        let program = argv.remove(0);
        let mut cmd = Command::new(program);
        cmd.args(argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_std_env(&mut cmd, spec);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            DriverError::Io(std::io::Error::other("qemu child stdin missing"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DriverError::Io(std::io::Error::other("qemu child stdout missing"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DriverError::Io(std::io::Error::other("qemu child stderr missing"))
        })?;

        // Probe replies are PTY-oriented; for pipes they are best-effort stdin writes.
        let mut writer: Box<dyn std::io::Write + Send> = Box::new(stdin);
        let probe = spec.profile.probe_reply();
        if !probe.is_empty() {
            writer.write_all(probe)?;
            writer.flush()?;
        }

        let pump = ReaderPump::from_readers(vec![
            Box::new(stdout) as Box<dyn std::io::Read + Send>,
            Box::new(stderr) as Box<dyn std::io::Read + Send>,
        ]);

        Ok(QemuUserSmokeSession {
            child: Some(child),
            io: SessionIo::new(writer, pump),
        })
    }
}

/// Piped QEMU smoke session (non-render).
pub struct QemuUserSmokeSession {
    child: Option<std::process::Child>,
    io: SessionIo,
}

impl QemuUserSmokeSession {
    fn ensure_open(&self) -> Result<(), DriverError> {
        if self.io.closed {
            Err(DriverError::Closed)
        } else {
            Ok(())
        }
    }
}

impl DriverSession for QemuUserSmokeSession {
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
        self.io.shutdown_readers();
        let mut child = self.child.take().ok_or(DriverError::Closed)?;
        let status = child.wait()?;
        Ok(status.into())
    }
}

impl Drop for QemuUserSmokeSession {
    fn drop(&mut self) {
        if !self.io.closed {
            self.io.closed = true;
            self.io.shutdown_readers();
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
