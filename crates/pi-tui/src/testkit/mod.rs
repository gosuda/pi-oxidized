//! Feature-gated portable terminal drivers for transcript harness work.
//!
//! This module is product-agnostic: adapters know how to launch and settle a
//! child under a PTY or piped stdio, not how `pi` interprets that output.

pub mod driver;
pub mod profile;
pub mod session;

/// Deterministic k-run comparison for transcript producers.
pub mod repeat;
/// Schema-v1 transcript types, canonical encoding, and recorder normalization.
pub mod transcript;
/// Schema-v1 transcript artifact validation and evidence rules.
pub mod validate;

#[cfg(windows)]
pub mod conpty;
#[cfg(unix)]
pub mod posix;
pub mod qemu;

pub use driver::{
    DriverError, DriverSession, ExitStatus, Geometry, LaunchSpec, OutputBatch, RenderSession,
    SettlePolicy, SettledFrame, TerminalDriver, TerminalSnapshot,
};
pub use profile::CapabilityProfile;
pub use session::{RecordingError, RecordingSession};
pub use transcript::DriverKind;

#[cfg(windows)]
pub use conpty::{ConPtyDriver, ConPtySession};
#[cfg(unix)]
pub use posix::{PosixPtyDriver, PosixPtySession};
pub use qemu::{QemuUserSmokeDriver, QemuUserSmokeSession};
