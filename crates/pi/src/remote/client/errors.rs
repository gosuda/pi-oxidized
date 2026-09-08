use crate::remote::codec::CodecError;
use crate::remote::framing::FrameError;
use crate::remote::schemas::ProtocolError;
use crate::remote::transport::TransportError;
use pi_agent::service::state_codec::ServiceCodecError;
use pi_agent::service::wire::WireError;

/// A failure reported by the remote server.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ServerError {
    /// Machine-readable server error code.
    pub code: String,
    /// Human-readable server error message.
    pub message: String,
}

/// The byte transport disconnected before the operation completed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DisconnectedError {
    /// Description of the disconnection.
    pub message: String,
}

/// The client has been disposed and cannot accept more operations.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Client is disposed")]
pub struct ClientDisposedError;

/// A wire or payload value failed protocol validation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProtocolValidationError {
    /// Description of the validation failure.
    pub message: String,
}

/// An in-flight client operation was cancelled.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Operation cancelled")]
pub struct CancelledError;

/// A failure returned by a client operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The server rejected the operation.
    #[error(transparent)]
    Server(ServerError),
    /// The byte transport disconnected.
    #[error(transparent)]
    Disconnected(DisconnectedError),
    /// The client was disposed.
    #[error(transparent)]
    Disposed(ClientDisposedError),
    /// A protocol value or frame failed validation.
    #[error(transparent)]
    Protocol(ProtocolValidationError),
    /// The operation was cancelled.
    #[error(transparent)]
    Cancelled(CancelledError),
}

impl ClientError {
    /// Creates a protocol-validation failure from a message.
    #[must_use]
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(ProtocolValidationError {
            message: message.into(),
        })
    }

    /// Creates a disconnected failure from a message.
    #[must_use]
    pub fn disconnected(message: impl Into<String>) -> Self {
        Self::Disconnected(DisconnectedError {
            message: message.into(),
        })
    }
}

impl From<ProtocolError> for ClientError {
    fn from(error: ProtocolError) -> Self {
        Self::Server(ServerError {
            code: error.code,
            message: error.message,
        })
    }
}

impl From<CodecError> for ClientError {
    fn from(error: CodecError) -> Self {
        Self::Protocol(ProtocolValidationError {
            message: error.to_string(),
        })
    }
}

impl From<FrameError> for ClientError {
    fn from(error: FrameError) -> Self {
        Self::Protocol(ProtocolValidationError {
            message: error.to_string(),
        })
    }
}
impl From<WireError> for ClientError {
    fn from(error: WireError) -> Self {
        Self::Protocol(ProtocolValidationError {
            message: error.to_string(),
        })
    }
}

impl From<ServiceCodecError> for ClientError {
    fn from(error: ServiceCodecError) -> Self {
        Self::Protocol(ProtocolValidationError {
            message: error.to_string(),
        })
    }
}


impl From<TransportError> for ClientError {
    fn from(error: TransportError) -> Self {
        Self::disconnected(error.to_string())
    }
}

/// A client construction option was invalid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClientOptionsError {
    /// The configured server identifier was invalid.
    #[error("Invalid server id")]
    InvalidServerId,
    /// The configured maximum frame length was invalid.
    #[error("Invalid max frame length: {value}")]
    InvalidMaxFrameLength {
        /// Rejected maximum frame length.
        value: usize,
    },
}
