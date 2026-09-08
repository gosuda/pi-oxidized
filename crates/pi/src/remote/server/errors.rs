//! Errors crossing the routed server boundary.

use std::error::Error;
use std::fmt;

use pi_agent::service::error::{RemoteServiceErrorCode, ServiceError};

use crate::remote::schemas::ProtocolError;

/// Fixed message used when an implementation failure is not safe to expose.
pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";

/// Operation codes that are safe to send to a remote peer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServerErrorCode {
    /// A service-domain error with a stable Chord code.
    Remote(RemoteServiceErrorCode),
    /// The request was addressed to another logical server.
    WrongServer,
    /// The durable session does not exist.
    SessionNotFound,
    /// The durable identifier was not unique.
    SessionAmbiguous,
    /// The client has no matching live attachment.
    SessionNotAttached,
    /// The server is draining and cannot admit new work.
    ServerDraining,
}

impl ServerErrorCode {
    /// Returns the protocol spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Remote(code) => code.as_str(),
            Self::WrongServer => "wrong_server",
            Self::SessionNotFound => "session_not_found",
            Self::SessionAmbiguous => "session_ambiguous",
            Self::SessionNotAttached => "session_not_attached",
            Self::ServerDraining => "server_draining",
        }
    }
}

impl fmt::Display for ServerErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded server error that may cross the protocol boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerError {
    /// Stable machine-readable code.
    pub code: ServerErrorCode,
    /// Human-readable diagnostic.
    pub message: String,
}

impl ServerError {
    /// Creates an arbitrary server error with a stable code.
    #[must_use]
    pub fn new(code: ServerErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Creates the server-identity fence error.
    #[must_use]
    pub fn wrong_server() -> Self {
        Self::new(
            ServerErrorCode::WrongServer,
            "Request was addressed to another server",
        )
    }

    /// Creates a missing-session error.
    #[must_use]
    pub fn session_not_found(message: impl Into<String>) -> Self {
        Self::new(ServerErrorCode::SessionNotFound, message)
    }

    /// Creates the ambiguous-session error.
    #[must_use]
    pub fn session_ambiguous() -> Self {
        Self::new(
            ServerErrorCode::SessionAmbiguous,
            "Session ID matches more than one session",
        )
    }

    /// Creates the missing-attachment error.
    #[must_use]
    pub fn session_not_attached() -> Self {
        Self::new(
            ServerErrorCode::SessionNotAttached,
            "Session is not attached to this client",
        )
    }

    /// Creates the draining error.
    #[must_use]
    pub fn server_draining() -> Self {
        Self::new(ServerErrorCode::ServerDraining, "Server is draining")
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ServerError {}

/// Host failures retain their local cause while exposing only bounded errors
/// through the remote protocol.
#[derive(Debug)]
pub enum HostError {
    /// A server-owned routing/lifecycle error.
    Server(ServerError),
    /// A service-provider or service-transport error.
    Service(ServiceError),
    /// A malformed service request rejected before provider dispatch.
    Protocol(String),
    /// An implementation failure retained for local reporting.
    Other(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(error) => error.fmt(formatter),
            Self::Service(error) => error.fmt(formatter),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl Error for HostError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Server(error) => Some(error),
            Self::Service(error) => Some(error),
            Self::Protocol(_) => None,
            Self::Other(error) => Some(error.as_ref()),
        }
    }
}

impl From<ServerError> for HostError {
    fn from(error: ServerError) -> Self {
        Self::Server(error)
    }
}

impl From<ServiceError> for HostError {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

impl From<crate::remote::codec::CodecError> for HostError {
    fn from(error: crate::remote::codec::CodecError) -> Self {
        Self::Protocol(error.to_string())
    }
}

/// Maps a host failure to the only error shape that is safe to serialize.
#[must_use]
pub fn to_protocol_error(error: &HostError) -> ProtocolError {
    match error {
        HostError::Server(error) => ProtocolError {
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
        },
        HostError::Service(ServiceError::Remote(error)) => ProtocolError {
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
        },
        HostError::Protocol(message) => ProtocolError {
            code: "invalid_request".to_owned(),
            message: message.clone(),
        },
        HostError::Service(_) | HostError::Other(_) => ProtocolError {
            code: "internal_error".to_owned(),
            message: INTERNAL_SERVER_ERROR_MESSAGE.to_owned(),
        },
    }
}

/// A best-effort duplicate used only when one retained failure is observed by
/// more than one waiter.  The first owner always keeps the original boxed
/// cause; later observers retain its diagnostic as a local source.
pub(crate) fn duplicate_host_error(error: &HostError) -> HostError {
    match error {
        HostError::Server(error) => HostError::Server(error.clone()),
        HostError::Protocol(message) => HostError::Protocol(message.clone()),
        HostError::Service(ServiceError::Remote(error)) => {
            HostError::Service(ServiceError::Remote(error.clone()))
        }
        HostError::Service(ServiceError::Local(message)) => {
            HostError::Service(ServiceError::Local(message.clone()))
        }
        HostError::Service(ServiceError::Disposed(message)) => {
            HostError::Service(ServiceError::Disposed(message.clone()))
        }
        HostError::Service(ServiceError::Cancelled) => {
            HostError::Service(ServiceError::Cancelled)
        }
        HostError::Service(ServiceError::Delta(error)) => {
            HostError::Service(ServiceError::Delta(error.clone()))
        }
        HostError::Service(ServiceError::State(error)) => {
            HostError::Service(ServiceError::State(error.clone()))
        }
        HostError::Service(error) => HostError::Other(Box::new(DiagnosticError(error.to_string()))),
        HostError::Other(error) => HostError::Other(Box::new(DiagnosticError(error.to_string()))),
    }
}

#[derive(Debug)]
struct DiagnosticError(String);

impl fmt::Display for DiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DiagnosticError {}
