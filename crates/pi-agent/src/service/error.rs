//! Errors crossing the native service boundary.

use std::error::Error;

use thiserror::Error;

use crate::context::Cancelled;

use super::delta::DeltaError;
use super::state_codec::ServiceStateError;

/// The only errors whose codes are part of the remote service protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RemoteServiceErrorCode {
    /// The caller attempted to use a service that is not allowlisted.
    ServiceNotAllowed,
    /// The requested service or provider instance does not exist.
    ServiceNotFound,
    /// The requested operation uses the wrong service mode.
    ServiceModeMismatch,
    /// The requested member does not exist.
    ServiceMemberNotFound,
    /// A member has the wrong kind, or a replacement changed its shape.
    ServiceMemberMismatch,
    /// A keyed instance does not exist.
    ServiceInstanceNotFound,
    /// A keyed instance generation is stale.
    ServiceStaleInstance,
    /// A value at a remote boundary is not strict JSON.
    ServiceInvalidValue,
}

impl RemoteServiceErrorCode {
    /// Returns the source protocol spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServiceNotAllowed => "service_not_allowed",
            Self::ServiceNotFound => "service_not_found",
            Self::ServiceModeMismatch => "service_mode_mismatch",
            Self::ServiceMemberNotFound => "service_member_not_found",
            Self::ServiceMemberMismatch => "service_member_mismatch",
            Self::ServiceInstanceNotFound => "service_instance_not_found",
            Self::ServiceStaleInstance => "service_stale_instance",
            Self::ServiceInvalidValue => "service_invalid_value",
        }
    }

    /// Parses one source protocol spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "service_not_allowed" => Self::ServiceNotAllowed,
            "service_not_found" => Self::ServiceNotFound,
            "service_mode_mismatch" => Self::ServiceModeMismatch,
            "service_member_not_found" => Self::ServiceMemberNotFound,
            "service_member_mismatch" => Self::ServiceMemberMismatch,
            "service_instance_not_found" => Self::ServiceInstanceNotFound,
            "service_stale_instance" => Self::ServiceStaleInstance,
            "service_invalid_value" => Self::ServiceInvalidValue,
            _ => return None,
        })
    }
}

impl std::fmt::Display for RemoteServiceErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Returns whether `value` is one of the eight source protocol codes.
#[must_use]
pub fn is_remote_service_error_code(value: &str) -> bool {
    RemoteServiceErrorCode::parse(value).is_some()
}

/// A coded service failure that may be mapped to a remote protocol error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct RemoteServiceError {
    /// Stable protocol code.
    pub code: RemoteServiceErrorCode,
    /// Human-readable diagnostic.  Products may mask it for remote callers.
    pub message: String,
}

impl RemoteServiceError {
    /// Creates a coded service failure.
    #[must_use]
    pub fn new(code: RemoteServiceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Failures raised by native service lifecycle and transport operations.
///
/// Only [`ServiceError::Remote`] carries one of the eight protocol codes.
/// Local definition failures, disposal, cancellation, and implementation
/// causes remain distinguishable so a product can mask them rather than
/// accidentally serializing arbitrary local errors as remote failures.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// A protocol-level service failure.
    #[error(transparent)]
    Remote(#[from] RemoteServiceError),
    /// A local definition or usage error analogous to a source `TypeError`.
    #[error("{0}")]
    Local(String),
    /// The provider, subscription, or endpoint has been disposed.
    #[error("{0}")]
    Disposed(String),
    /// The caller's context was cancelled.
    #[error("context cancelled")]
    Cancelled,
    /// A delta operation could not be applied or encoded.
    #[error(transparent)]
    Delta(#[from] DeltaError),
    /// A replicated-state validation failure.
    #[error(transparent)]
    State(#[from] ServiceStateError),
    /// A user method or callback failed; the original cause is retained.
    #[error("service handler failed: {source}")]
    Handler {
        /// Original handler failure.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// A transport implementation failed; the original cause is retained.
    #[error("service transport failed: {source}")]
    Transport {
        /// Original transport failure.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// An internal service failure with an optional retained cause.
    #[error("{message}")]
    Internal {
        /// Stable local diagnostic.
        message: String,
        /// Original cause, when one exists.
        #[source]
        source: Option<Box<dyn Error + Send + Sync>>,
    },
}

impl ServiceError {
    /// Creates a local definition or lifecycle usage failure.
    #[must_use]
    pub fn local(message: impl Into<String>) -> Self {
        Self::Local(message.into())
    }

    /// Creates a disposal failure.
    #[must_use]
    pub fn disposed(message: impl Into<String>) -> Self {
        Self::Disposed(message.into())
    }

    /// Creates a cancellation failure.
    #[must_use]
    pub const fn cancelled() -> Self {
        Self::Cancelled
    }

    /// Creates a coded remote failure.
    #[must_use]
    pub fn remote(code: RemoteServiceErrorCode, message: impl Into<String>) -> Self {
        Self::Remote(RemoteServiceError::new(code, message))
    }

    /// Creates an implementation failure while preserving its cause.
    #[must_use]
    pub fn handler<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Handler {
            source: Box::new(source),
        }
    }

    /// Creates a transport failure while preserving its cause.
    #[must_use]
    pub fn transport<E>(source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Transport {
            source: Box::new(source),
        }
    }

    /// Creates an internal failure with an optional retained cause.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            source: None,
        }
    }

    /// Creates an internal failure while preserving its cause.
    #[must_use]
    pub fn internal_with_source<E>(message: impl Into<String>, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Internal {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Returns the protocol code when this error is safe to map remotely.
    #[must_use]
    pub const fn remote_code(&self) -> Option<RemoteServiceErrorCode> {
        match self {
            Self::Remote(error) => Some(error.code),
            Self::Local(_)
            | Self::Disposed(_)
            | Self::Cancelled
            | Self::Delta(_)
            | Self::State(_)
            | Self::Handler { .. }
            | Self::Transport { .. }
            | Self::Internal { .. } => None,
        }
    }
}

impl From<Cancelled> for ServiceError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}
