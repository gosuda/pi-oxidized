//! Driver contracts and shared launch/settle value types.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

pub use super::transcript::{CapabilityProfile, DriverKind, Geometry};

impl Geometry {
    /// Creates a geometry, rejecting zero dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidSpec`] when either dimension is zero.
    pub fn new(cols: u16, rows: u16) -> Result<Self, DriverError> {
        if cols == 0 || rows == 0 {
            return Err(DriverError::InvalidSpec(
                "geometry cols and rows must be non-zero".to_owned(),
            ));
        }
        Ok(Self { cols, rows })
    }
}

/// Quiescence-bounded settle windows.
///
/// Hitting [`Self::ceiling`] is a hard error; elapsed output is never treated
/// as canonical settled content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlePolicy {
    /// Required idle gap after the content predicate matches.
    pub quiet: Duration,
    /// Absolute upper bound for a single settle attempt.
    pub ceiling: Duration,
}

impl SettlePolicy {
    /// Builds a policy, requiring a non-zero ceiling not shorter than quiet.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidSpec`] when the ceiling is zero or shorter than quiet.
    pub fn new(quiet: Duration, ceiling: Duration) -> Result<Self, DriverError> {
        if ceiling == Duration::ZERO {
            return Err(DriverError::InvalidSpec(
                "settle ceiling must be non-zero".to_owned(),
            ));
        }
        if quiet > ceiling {
            return Err(DriverError::InvalidSpec(
                "settle quiet window cannot exceed ceiling".to_owned(),
            ));
        }
        Ok(Self { quiet, ceiling })
    }
}

impl Default for SettlePolicy {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(250),
            ceiling: Duration::from_secs(10),
        }
    }
}

/// Launch parameters for [`TerminalDriver::open`].
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Argv for the child process (`argv[0]` is the program).
    pub argv: Vec<String>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// Extra environment overlays (applied after the capability profile).
    pub env: BTreeMap<String, String>,
    /// Initial PTY or logical geometry.
    pub geometry: Geometry,
    /// Closed capability profile selecting env + probe bytes.
    pub profile: CapabilityProfile,
}

impl LaunchSpec {
    /// Validates argv and geometry invariants before launch.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidSpec`] when argv or geometry is invalid.
    pub fn validate(&self) -> Result<(), DriverError> {
        if self.argv.is_empty() {
            return Err(DriverError::InvalidSpec(
                "argv must be non-empty".to_owned(),
            ));
        }
        if self.argv[0].is_empty() {
            return Err(DriverError::InvalidSpec(
                "argv[0] program must be non-empty".to_owned(),
            ));
        }
        let _ = Geometry::new(self.geometry.cols, self.geometry.rows)?;
        Ok(())
    }
}

/// One normalized output batch collected between input boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBatch {
    /// Raw bytes observed for this boundary, in arrival order.
    pub bytes: Vec<u8>,
}

/// AVT-derived visible frame captured at settle time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    /// Geometry used to rebuild the VT.
    pub geometry: Geometry,
    /// Cursor column reported by AVT.
    pub cursor_col: usize,
    /// Cursor row reported by AVT.
    pub cursor_row: usize,
    /// Whether AVT considers the cursor visible.
    pub cursor_visible: bool,
    /// Scrollback-inclusive lines with trailing spaces trimmed.
    pub lines: Vec<String>,
}

/// Settled output plus an AVT snapshot (render sessions only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledFrame {
    /// Quiescence-bounded output batch for this boundary.
    pub batch: OutputBatch,
    /// Snapshot rebuilt from the full raw log at the current geometry.
    pub snapshot: TerminalSnapshot,
}

/// Child termination status returned from [`DriverSession::close`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitStatus {
    /// Process exit code (signal exits use the platform code when available).
    pub code: u32,
    /// Signal name when the process was terminated by signal.
    pub signal: Option<String>,
}

impl ExitStatus {
    /// Returns true when the process exited with code 0 and no signal.
    #[must_use]
    pub fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }

    /// Builds an exit status from a numeric code.
    #[must_use]
    pub fn from_code(code: u32) -> Self {
        Self { code, signal: None }
    }

    /// Builds an exit status from a signal name.
    pub fn from_signal(signal: impl Into<String>) -> Self {
        Self {
            code: 1,
            signal: Some(signal.into()),
        }
    }
}

impl From<portable_pty::ExitStatus> for ExitStatus {
    fn from(status: portable_pty::ExitStatus) -> Self {
        Self {
            code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
        }
    }
}

impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                return Self {
                    code: u32::try_from(sig).unwrap_or(1),
                    signal: Some(sig.to_string()),
                };
            }
        }
        let code = u32::try_from(status.code().unwrap_or(1)).unwrap_or(1);
        Self { code, signal: None }
    }
}

/// Errors raised by portable terminal drivers.
#[derive(Debug, Error)]
pub enum DriverError {
    /// Launch specification failed validation.
    #[error("invalid launch spec: {0}")]
    InvalidSpec(String),
    /// Underlying OS / PTY I/O failed.
    #[error("driver i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// `portable-pty` reported a failure.
    #[error("pty error: {0}")]
    Pty(String),
    /// Settle ceiling elapsed before the content predicate stayed quiet.
    #[error("settle ceiling elapsed before quiescence")]
    SettleCeiling,
    /// Child ended before the content predicate matched.
    #[error("child ended before settle predicate matched")]
    PrematureExit,
    /// Session was used after close.
    #[error("session already closed")]
    Closed,
}

impl DriverError {
    /// Converts a `portable-pty`/`anyhow` style error into [`DriverError::Pty`].
    #[must_use]
    pub fn pty(error: &impl ToString) -> Self {
        Self::Pty(error.to_string())
    }
}

/// Opens adapter-specific sessions.
pub trait TerminalDriver {
    /// Concrete session type produced by [`Self::open`].
    type Session: DriverSession;

    /// Returns the adapter kind recorded into transcripts.
    fn kind(&self) -> DriverKind;

    /// Spawns the child according to `spec`.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] when validation, spawn, or probe setup fails.
    fn open(&self, spec: &LaunchSpec) -> Result<Self::Session, DriverError>;
}

/// Core verbs available to every adapter, including QEMU smoke.
pub trait DriverSession {
    /// Writes bytes to the child (one canonical input boundary).
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] on I/O failure or if the session is closed.
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError>;

    /// Drains output until `predicate` matches and the quiet window holds.
    ///
    /// All bytes since the previous input/settle boundary are returned as one
    /// [`OutputBatch`]. Hitting the settle ceiling returns
    /// [`DriverError::SettleCeiling`] and must not be treated as canonical
    /// content.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] on I/O, premature exit, ceiling, or closed session.
    fn read_output<F>(
        &mut self,
        policy: &SettlePolicy,
        predicate: F,
    ) -> Result<OutputBatch, DriverError>
    where
        F: FnMut(&[u8]) -> bool;

    /// Closes the session and waits for the child to exit.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if waiting on the child or joining readers fails.
    fn close(self) -> Result<ExitStatus, DriverError>;
}

/// Render-capable extension implemented only by PTY adapters.
pub trait RenderSession: DriverSession {
    /// Resizes the PTY to `cols`×`rows`.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] when geometry is invalid or the resize fails.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), DriverError>;

    /// Applies a back-to-back resize storm as one logical harness action.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] on the first invalid geometry or resize failure.
    fn resize_storm(&mut self, sizes: &[(u16, u16)]) -> Result<(), DriverError>;

    /// Settles output like [`DriverSession::read_output`], then snapshots AVT.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] on settle failure or closed session.
    fn read_settled_frame<F>(
        &mut self,
        policy: &SettlePolicy,
        predicate: F,
    ) -> Result<SettledFrame, DriverError>
    where
        F: FnMut(&[u8]) -> bool;
}
