//! Shared reader pumping, quiescence settling, cleanup, and AVT snapshots.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use avt::Vt;

use crate::testkit::driver::{DriverError, Geometry, OutputBatch, SettlePolicy, TerminalSnapshot};

/// One chunk, or a terminal reader failure that must not validate as settle.
pub(crate) type ReaderEvent = Result<Vec<u8>, std::io::Error>;

/// Byte channel fed by one or more reader threads.
pub(crate) struct ReaderPump {
    rx: Receiver<ReaderEvent>,
    joins: Vec<JoinHandle<()>>,
}

impl ReaderPump {
    /// Builds a pump from a single readable end.
    pub(crate) fn from_reader<R>(reader: R) -> Self
    where
        R: Read + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });
        Self {
            rx,
            joins: vec![join],
        }
    }

    /// Joins every reader thread, surfacing the first panic as an I/O error.
    pub(crate) fn join(&mut self) -> Result<(), DriverError> {
        let mut first_err: Option<DriverError> = None;
        for join in self.joins.drain(..) {
            if let Err(panic) = join.join() {
                let msg = panic_message(&panic);
                if first_err.is_none() {
                    first_err = Some(DriverError::Io(std::io::Error::other(format!(
                        "reader thread panicked: {msg}"
                    ))));
                }
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(msg) = panic.downcast_ref::<&str>() {
        (*msg).to_owned()
    } else if let Some(msg) = panic.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

/// Accumulates raw logs and per-boundary pending bytes.
#[derive(Debug, Default)]
pub(crate) struct OutputLedger {
    raw_log: Vec<u8>,
    pending: Vec<u8>,
}

impl OutputLedger {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.raw_log.extend_from_slice(chunk);
        self.pending.extend_from_slice(chunk);
    }

    pub(crate) fn pending(&self) -> &[u8] {
        &self.pending
    }

    pub(crate) fn raw_log(&self) -> &[u8] {
        &self.raw_log
    }

    pub(crate) fn take_batch(&mut self) -> OutputBatch {
        OutputBatch {
            bytes: std::mem::take(&mut self.pending),
        }
    }
}

/// Shared mutable I/O state for driver sessions.
pub(crate) struct SessionIo {
    pub(crate) writer: Option<Box<dyn Write + Send>>,
    pub(crate) pump: Option<ReaderPump>,
    pub(crate) ledger: OutputLedger,
    pub(crate) closed: bool,
}

impl SessionIo {
    pub(crate) fn new(writer: Box<dyn Write + Send>, pump: ReaderPump) -> Self {
        Self {
            writer: Some(writer),
            pump: Some(pump),
            ledger: OutputLedger::default(),
            closed: false,
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        if self.closed {
            return Err(DriverError::Closed);
        }
        let writer = self.writer.as_mut().ok_or(DriverError::Closed)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub(crate) fn read_output<F>(
        &mut self,
        policy: &SettlePolicy,
        mut predicate: F,
    ) -> Result<OutputBatch, DriverError>
    where
        F: FnMut(&[u8]) -> bool,
    {
        if self.closed {
            return Err(DriverError::Closed);
        }
        let pump = self.pump.as_mut().ok_or(DriverError::Closed)?;
        settle_read(&pump.rx, &mut self.ledger, policy, &mut predicate)?;
        Ok(self.ledger.take_batch())
    }

    pub(crate) fn close_writer(&mut self) {
        drop(self.writer.take());
    }

    /// Joins reader threads. Explicit close must propagate panics; Drop may ignore.
    pub(crate) fn join_readers(&mut self) -> Result<(), DriverError> {
        if let Some(mut pump) = self.pump.take() {
            pump.join()
        } else {
            Ok(())
        }
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        // Never join readers here: a still-live child can keep the reader blocked.
        // Session Drop/close must terminate/wait the child before join_readers().
        self.close_writer();
        self.closed = true;
        // Dropping JoinHandle detaches; EOF arrives after the child exits.
        drop(self.pump.take());
    }
}

/// Quiescence-bounded read: predicate then quiet window, else ceiling error.
pub(crate) fn settle_read<F>(
    rx: &Receiver<ReaderEvent>,
    ledger: &mut OutputLedger,
    policy: &SettlePolicy,
    predicate: &mut F,
) -> Result<(), DriverError>
where
    F: FnMut(&[u8]) -> bool,
{
    let started = Instant::now();
    let mut last_data = Instant::now();
    let mut matched = predicate(ledger.pending());

    loop {
        let elapsed = started.elapsed();
        if elapsed >= policy.ceiling {
            return Err(DriverError::SettleCeiling);
        }

        if matched && last_data.elapsed() >= policy.quiet {
            return Ok(());
        }

        let wait = remaining_wait(policy, started, matched, last_data);
        match rx.recv_timeout(wait) {
            Ok(Ok(chunk)) => {
                ledger.push(&chunk);
                last_data = Instant::now();
                if !matched {
                    matched = predicate(ledger.pending());
                }
            }
            Ok(Err(err)) => {
                return Err(DriverError::Io(std::io::Error::new(
                    err.kind(),
                    format!("reader failed before settle: {err}"),
                )));
            }
            Err(RecvTimeoutError::Timeout) => {
                if matched && last_data.elapsed() >= policy.quiet {
                    return Ok(());
                }
                if started.elapsed() >= policy.ceiling {
                    return Err(DriverError::SettleCeiling);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Clean EOF only. Accept only if the predicate already matched.
                if matched {
                    return Ok(());
                }
                return Err(DriverError::PrematureExit);
            }
        }
    }
}

fn remaining_wait(
    policy: &SettlePolicy,
    started: Instant,
    matched: bool,
    last_data: Instant,
) -> Duration {
    let until_ceiling = policy
        .ceiling
        .checked_sub(started.elapsed())
        .unwrap_or(Duration::ZERO);
    if !matched {
        return until_ceiling;
    }
    let until_quiet = policy
        .quiet
        .checked_sub(last_data.elapsed())
        .unwrap_or(Duration::ZERO);
    until_ceiling.min(until_quiet)
}

/// Rebuilds an AVT snapshot from the full raw log at `geometry`.
pub(crate) fn snapshot_from_raw(raw: &[u8], geometry: Geometry) -> TerminalSnapshot {
    let cols = usize::from(geometry.cols.max(1));
    let rows = usize::from(geometry.rows.max(1));
    let mut vt = Vt::builder()
        .size(cols, rows)
        .scrollback_limit(10_000)
        .build();
    let lossy = String::from_utf8_lossy(raw);
    let _ = vt.feed_str(&lossy);
    let cursor = vt.cursor();
    let mut lines: Vec<String> = vt
        .lines()
        .map(|line| line.text().trim_end().to_owned())
        .collect();
    if lines.iter().all(|line| line.trim().is_empty()) {
        lines = vt
            .text()
            .into_iter()
            .map(|line| line.trim_end().to_owned())
            .collect();
    }
    TerminalSnapshot {
        geometry,
        cursor_col: cursor.col,
        cursor_row: cursor.row,
        cursor_visible: cursor.visible,
        lines,
    }
}

/// Applies profile env then launch-spec overlays onto a portable-pty command.
pub(crate) fn apply_env(
    cmd: &mut portable_pty::CommandBuilder,
    spec: &crate::testkit::driver::LaunchSpec,
) {
    for (key, value) in spec.profile.env() {
        cmd.env(key, value);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
}

/// Applies profile env then launch-spec overlays onto a std Command.
pub(crate) fn apply_std_env(
    cmd: &mut std::process::Command,
    spec: &crate::testkit::driver::LaunchSpec,
) {
    for (key, value) in spec.profile.env() {
        cmd.env(key, value);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
}
