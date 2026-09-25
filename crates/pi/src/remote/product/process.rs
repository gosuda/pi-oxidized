//! Native process primitives for the development-only remote product.
//!
//! Internal roles are always re-executed from the current executable.  The
//! executable is deliberately not looked up through `PATH`: a role must run
//! the exact build that started its parent.  Control messages use compact
//! JSON lines with an explicit byte bound so a malformed peer cannot grow a
//! line buffer without limit.

use std::fmt;
use std::io;
use std::process::Stdio;

use serde::Serialize;
use thiserror::Error;
use tokio::process::{Child, Command};

/// Environment variable carrying the role of an internally spawned process.
pub const INTERNAL_PROCESS_ENV: &str = "__PI_INTERNAL_SPAWN";

/// Maximum encoded internal control-line size (128 MiB).
pub const MAX_CONTROL_LINE_BYTES: usize = 128 * 1024 * 1024;

/// A role implemented by the development-only native executable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InternalProcessRole {
    /// Coordinator process owning the public-to-generation route.
    Coordinator,
    /// Server process owning one generation listener.
    Server,
    /// Session-worker process owning durable session effects.
    SessionWorker,
}

impl InternalProcessRole {
    /// Returns the exact environment/wire spelling of this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Server => "server",
            Self::SessionWorker => "session-worker",
        }
    }

    /// Parses one exact role spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "coordinator" => Some(Self::Coordinator),
            "server" => Some(Self::Server),
            "session-worker" => Some(Self::SessionWorker),
            _ => None,
        }
    }
}

impl fmt::Display for InternalProcessRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Errors produced while reading an internal role or encoding a control line.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The role environment variable was present but did not contain a known
    /// exact role spelling.
    #[error("Unsupported internal process role: {role}")]
    UnsupportedRole {
        /// The invalid environment value (lossily rendered when it was not
        /// valid UTF-8).
        role: String,
    },
    /// The encoded JSON line exceeded [`MAX_CONTROL_LINE_BYTES`].
    #[error("Internal control message is too large")]
    ControlMessageTooLarge,
    /// The message could not be serialized as JSON.
    #[error("failed to encode internal control message: {0}")]
    Encode(#[source] serde_json::Error),
}

/// Reads and validates the role without changing the process environment.
///
/// # Errors
///
/// Returns [`ProcessError::UnsupportedRole`] when the environment variable is
/// present but is not one of the exact role spellings.
pub fn internal_process_role() -> Result<Option<InternalProcessRole>, ProcessError> {
    let Some(value) = std::env::var_os(INTERNAL_PROCESS_ENV) else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    InternalProcessRole::parse(value.as_ref())
        .map(Some)
        .ok_or_else(|| ProcessError::UnsupportedRole {
            role: value.into_owned(),
        })
}

/// Options applied in addition to the inherited environment of an internal
/// child process.
#[derive(Clone, Debug, Default)]
pub struct InternalProcessSpawnOptions {
    /// Environment entries applied after inheriting the parent environment.
    /// Later entries for the same key win, as with repeated `Command::env`.
    pub env: Vec<(String, String)>,
}

/// Re-executes the current executable as one internal process role.
///
/// The child receives the caller's current working directory, null standard
/// streams, and an explicit role environment value.  On Unix it is placed in
/// a fresh process group so later termination can target the detached process
/// without attaching it to the caller's terminal.  Dropping the returned
/// handle leaves the child running, matching the source `unref` contract.
///
/// # Errors
///
/// Returns the operating-system error from resolving the current executable,
/// current directory, or spawning the child.
pub fn spawn_internal_process(
    role: InternalProcessRole,
    args: &[String],
    options: &InternalProcessSpawnOptions,
) -> io::Result<Child> {
    let executable = std::env::current_exe()?;
    let current_directory = std::env::current_dir()?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(current_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env(INTERNAL_PROCESS_ENV, role.as_str());
    for (key, value) in &options.env {
        command.env(key, value);
    }
    // Keep the role assignment authoritative even if an options entry used
    // the reserved key.  A spawned role must never silently become another
    // role because caller-provided environment ordering changed.
    command.env(INTERNAL_PROCESS_ENV, role.as_str());

    #[cfg(unix)]
    command.process_group(0);

    command.spawn()
}

/// Force an internal child to exit and reap it.
///
/// A child that has already exited (and therefore has no pid or is returned by
/// `try_wait`) needs no further action.  Otherwise a best-effort SIGKILL is
/// issued and `wait` reaps the child; a kill error is returned unless a second
/// poll observes that the child exited concurrently.
///
/// # Errors
///
/// Returns an operating-system error from polling or reaping the child.
pub async fn terminate_internal_process(child: &mut Child) -> io::Result<()> {
    if child.id().is_none() {
        return Ok(());
    }
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Err(error) = child.start_kill() {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        return Err(error);
    }
    child.wait().await.map(|_| ())
}

/// Serializes a control message to one compact, newline-terminated JSON line.
///
/// # Errors
///
/// Returns [`ProcessError::Encode`] when `message` cannot be serialized, or
/// [`ProcessError::ControlMessageTooLarge`] when the UTF-8 line length is over
/// [`MAX_CONTROL_LINE_BYTES`].
pub fn encode_control_line<T: Serialize + ?Sized>(message: &T) -> Result<Vec<u8>, ProcessError> {
    let mut line = serde_json::to_vec(message).map_err(ProcessError::Encode)?;
    let size = line
        .len()
        .checked_add(1)
        .ok_or(ProcessError::ControlMessageTooLarge)?;
    if size > MAX_CONTROL_LINE_BYTES {
        return Err(ProcessError::ControlMessageTooLarge);
    }
    line.push(b'\n');
    Ok(line)
}
