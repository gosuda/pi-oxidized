//! Byte-transport seam — portable port of upstream `transport.ts`, plus the
//! endpoint-spec surface that turns an [`EndpointSpec`] into a
//! [`ByteTransportFactory`] (R3).
//!
//! The trait pair mirrors upstream exactly:
//!
//! - [`ByteTransport`] sends outbound byte chunks in invocation order and
//!   closes idempotently.
//! - [`ByteTransportHandlers`] receives inbound chunks plus exactly one
//!   terminal callback (`on_close` or `on_error`).
//! - [`ByteTransportFactory`] creates a fresh connected transport at client
//!   connection time. There is no automatic reconnect: the client asks the
//!   factory once per connection attempt.
//!
//! Platform contract (AR2): this module and the in-memory adapter are
//! portable and compile on every tier, including `x86_64-pc-windows-msvc`.
//! The Unix-domain adapter lives in `transport/unix.rs` and is
//! `#[cfg(unix)]`-gated; building a Unix endpoint on any other tier fails
//! eagerly with the typed
//! [`EndpointSpecError::UnsupportedOnPlatform`] — a construction error
//! distinct from the five runtime classes the client can originate.

use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

mod in_memory;

#[cfg(unix)]
mod unix;

pub use in_memory::{InMemoryEndpoint, InMemoryListener, InMemoryTransport};

#[cfg(unix)]
pub use unix::{UnixByteTransport, UnixTransportOptions};
/// Failure reported by a byte transport adapter: underlying stream I/O
/// failure, closure before or during the operation, pending-byte budget
/// exhaustion, or adapter-specific failure text.
#[derive(Debug)]
pub enum TransportError {
    /// Underlying stream I/O failure.
    Io(std::io::Error),
    /// The transport was closed before or during the operation.
    Closed,
    /// The pending-byte backpressure budget was exceeded.
    PendingBytesExceeded,
    /// Adapter-specific failure text.
    Message(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transport I/O failure: {error}"),
            Self::Closed => write!(f, "transport is closed"),
            Self::PendingBytesExceeded => {
                write!(f, "transport exceeded its pending byte limit")
            }
            Self::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl Clone for TransportError {
    fn clone(&self) -> Self {
        match self {
            Self::Io(error) => Self::Io(std::io::Error::new(error.kind(), error.to_string())),
            other => match other {
                Self::Closed => Self::Closed,
                Self::PendingBytesExceeded => Self::PendingBytesExceeded,
                Self::Message(message) => Self::Message(message.clone()),
                Self::Io(_) => unreachable!("covered above"),
            },
        }
    }
}

/// Future returned by [`ByteTransport::send`].
pub type SendFuture = Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send>>;

/// Future returned by a [`ByteTransportFactory`].
pub type ConnectFuture =
    Pin<Box<dyn Future<Output = Result<Arc<dyn ByteTransport>, TransportError>> + Send>>;

/// Receives data and terminal events from one side of a [`ByteTransport`].
///
/// Exactly one terminal callback (`on_close` or `on_error`) is expected over
/// the lifetime of the handlers.
pub trait ByteTransportHandlers: Send + Sync + 'static {
    /// Delivers an arbitrary inbound byte chunk.
    fn on_data(&self, chunk: Vec<u8>);
    /// Reports an orderly terminal close.
    fn on_close(&self);
    /// Reports a terminal transport failure.
    fn on_error(&self, error: TransportError);
}

/// A connected, ordered byte pipe (port of upstream `ByteTransport`).
///
/// `send` chunks must be delivered in invocation order; `close` must make
/// repeated calls harmless.
pub trait ByteTransport: Send + Sync + 'static {
    /// Sends one byte chunk. Resolves once the chunk is accepted by the
    /// underlying stream.
    fn send(&self, chunk: Vec<u8>) -> SendFuture;
    /// Closes the transport and stops delivering handler events for the
    /// local close.
    fn close(&self);
}

/// Creates a fresh connected transport for one client connection attempt
/// (port of upstream `ByteTransportFactory`).
pub type ByteTransportFactory =
    Arc<dyn Fn(Arc<dyn ByteTransportHandlers>) -> ConnectFuture + Send + Sync>;

/// Platform-gated endpoint families.
///
/// Only the Unix-domain family is platform-gated today; a future server-side
/// Unix listener spec shares this error surface (one typed error owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// A Unix-domain socket endpoint (client connect or server listen).
    Unix,
}

impl fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix => write!(f, "unix"),
        }
    }
}

/// Declares where a client connects. The enum is declared unconditionally;
/// only [`build_transport`] decides which specs a platform can build.
#[derive(Debug, Clone)]
pub enum EndpointSpec {
    /// A paired in-memory byte pipe to an [`InMemoryListener`].
    InMemory {
        /// Address of the in-process listener.
        endpoint: InMemoryEndpoint,
    },
    /// A Unix-domain socket endpoint (Unix tier only).
    Unix {
        /// Socket path, validated eagerly by [`build_transport`].
        path: PathBuf,
        /// Outbound backpressure budget in bytes
        /// (defaults to four times the 16 MiB frame bound).
        max_pending_bytes: Option<usize>,
    },
}

/// Construction-time failure for [`build_transport`] — distinct from the five
/// runtime classes the client can originate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointSpecError {
    /// The endpoint family cannot be built on this platform.
    UnsupportedOnPlatform {
        /// The gated endpoint family.
        kind: EndpointKind,
        /// `std::env::consts::OS` of the compiling-and-running platform.
        os: &'static str,
    },
    /// The Unix socket path is the empty string.
    EmptyPath,
    /// The Unix socket path exceeds the platform's `sun_path` budget.
    PathTooLong {
        /// Maximum allowed path length in UTF-8 bytes.
        max: usize,
    },
    /// `max_pending_bytes` was zero or non-representable.
    InvalidMaxPendingBytes,
}

impl fmt::Display for EndpointSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedOnPlatform { kind, os } => {
                write!(f, "{kind} endpoint is unsupported on platform {os}")
            }
            Self::EmptyPath => write!(f, "unix transport path must not be empty"),
            Self::PathTooLong { max } => write!(
                f,
                "unix transport path is too long; maximum is {max} UTF-8 bytes"
            ),
            Self::InvalidMaxPendingBytes => {
                write!(
                    f,
                    "unix transport maxPendingBytes must be a positive integer"
                )
            }
        }
    }
}

impl std::error::Error for EndpointSpecError {}

/// Maximum Unix socket path length in bytes: Linux allocates 108 bytes for
/// `sun_path` (107 usable plus NUL); every other Unix leaves 104 (103 plus
/// NUL). Mirrors upstream `unix.ts`.
///
/// Defined on every tier: the eager Unix-spec validation in
/// [`build_transport`] runs on non-Unix builds too, so the limit must be
/// nameable there.
const MAX_UNIX_SOCKET_PATH_BYTES: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// The single fallible surface turning an [`EndpointSpec`] into a
/// [`ByteTransportFactory`].
///
/// Validation is eager, mirroring upstream `createUnixTransportFactory`:
/// empty and over-long paths and non-positive pending-byte budgets are
/// rejected before any connection is attempted, and a Unix spec on a
/// non-Unix tier returns the typed
/// [`EndpointSpecError::UnsupportedOnPlatform`] instead of panicking or
/// leaking an untyped string.
///
/// # Errors
///
/// Returns [`EndpointSpecError::EmptyPath`] if the Unix path is empty,
/// [`EndpointSpecError::PathTooLong`] if the path exceeds the platform limit,
/// [`EndpointSpecError::InvalidMaxPendingBytes`] if the budget is zero, or
/// [`EndpointSpecError::UnsupportedOnPlatform`] on non-Unix tiers.
pub fn build_transport(spec: &EndpointSpec) -> Result<ByteTransportFactory, EndpointSpecError> {
    match spec {
        EndpointSpec::InMemory { endpoint } => Ok(endpoint.factory()),
        EndpointSpec::Unix {
            path,
            max_pending_bytes,
        } => {
            let path = path.clone();
            if path.as_os_str().is_empty() {
                return Err(EndpointSpecError::EmptyPath);
            }
            if path.as_os_str().len() > MAX_UNIX_SOCKET_PATH_BYTES {
                return Err(EndpointSpecError::PathTooLong {
                    max: MAX_UNIX_SOCKET_PATH_BYTES,
                });
            }
            let max_pending_bytes = match max_pending_bytes {
                Some(0) => return Err(EndpointSpecError::InvalidMaxPendingBytes),
                other => other.to_owned(),
            };
            #[cfg(unix)]
            {
                Ok(unix::factory(path, max_pending_bytes))
            }
            #[cfg(not(unix))]
            {
                let _ = (path, max_pending_bytes);
                Err(EndpointSpecError::UnsupportedOnPlatform {
                    kind: EndpointKind::Unix,
                    os: std::env::consts::OS,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::expect_used,
        reason = "asserting that build_transport rejects an empty path"
    )]
    #[test]
    fn build_transport_rejects_empty_unix_path() {
        let error = build_transport(&EndpointSpec::Unix {
            path: PathBuf::new(),
            max_pending_bytes: None,
        })
        .map(|_| ())
        .expect_err("expected error");
        assert_eq!(error, EndpointSpecError::EmptyPath);
    }

    #[expect(
        clippy::expect_used,
        reason = "asserting that build_transport rejects an over-long path"
    )]
    #[test]
    fn build_transport_rejects_over_long_unix_path() {
        let max = if cfg!(target_os = "linux") { 107 } else { 103 };
        let long = "a".repeat(max + 1);
        let error = build_transport(&EndpointSpec::Unix {
            path: PathBuf::from(long),
            max_pending_bytes: None,
        })
        .map(|_| ())
        .expect_err("expected error");
        assert_eq!(error, EndpointSpecError::PathTooLong { max });
    }

    #[expect(
        clippy::expect_used,
        reason = "asserting that build_transport rejects a zero byte budget"
    )]
    #[test]
    fn build_transport_rejects_zero_pending_bytes() {
        let error = build_transport(&EndpointSpec::Unix {
            path: PathBuf::from("/tmp/pi.sock"),
            max_pending_bytes: Some(0),
        })
        .map(|_| ())
        .expect_err("expected error");
        assert_eq!(error, EndpointSpecError::InvalidMaxPendingBytes);
    }

    #[test]
    fn build_transport_builds_in_memory_factory_on_every_platform() {
        let (_listener, endpoint) = InMemoryListener::new();
        let factory = build_transport(&EndpointSpec::InMemory { endpoint });
        assert!(factory.is_ok(), "in-memory endpoint must build everywhere");
    }

    #[cfg(unix)]
    #[test]
    fn build_transport_builds_unix_factory_on_the_unix_tier() {
        let factory = build_transport(&EndpointSpec::Unix {
            path: PathBuf::from("/tmp/pi-remote-client-test.sock"),
            max_pending_bytes: None,
        });
        assert!(factory.is_ok(), "unix endpoints build on the unix tier");
    }

    /// Pins the typed non-Unix result: no panic, no untyped string. Compiled
    /// (and run) only off the Unix tier; the Windows-target compile check
    /// keeps this test compiling with the unix module absent.
    #[expect(
        clippy::expect_used,
        reason = "asserting that build_transport returns UnsupportedOnPlatform off-Unix"
    )]
    #[cfg(not(unix))]
    #[test]
    fn build_transport_returns_typed_unsupported_on_platform_off_unix() {
        let error = build_transport(&EndpointSpec::Unix {
            path: PathBuf::from("/tmp/pi.sock"),
            max_pending_bytes: None,
        })
        .map(|_| ())
        .expect_err("expected error");
        assert_eq!(
            error,
            EndpointSpecError::UnsupportedOnPlatform {
                kind: EndpointKind::Unix,
                os: std::env::consts::OS,
            }
        );
        assert_eq!(
            error.to_string(),
            "unix endpoint is unsupported on platform windows"
        );
    }

    #[test]
    fn transport_error_display_is_stable() {
        assert_eq!(TransportError::Closed.to_string(), "transport is closed");
        assert_eq!(
            TransportError::PendingBytesExceeded.to_string(),
            "transport exceeded its pending byte limit"
        );
        assert_eq!(TransportError::Message("boom".into()).to_string(), "boom");
    }
}
