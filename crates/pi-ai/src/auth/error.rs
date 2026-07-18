//! Auth and model-collection error types.
//!
//! [`ModelsError`] is the shared failure type for auth resolution, catalog
//! loading, and provider/stream setup. Store and interactive login failures use
//! the narrower [`StoreError`] and [`AuthError`] types until a caller lifts
//! them into a [`ModelsError`].

use std::fmt;

/// Machine-readable models/auth error classification.
///
/// Wire/text form matches the TypeScript `ModelsErrorCode` union.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelsErrorCode {
    /// Model catalog/source failure.
    ModelSource,
    /// Model payload failed validation.
    ModelValidation,
    /// Provider registration or lookup failure.
    Provider,
    /// Streaming infrastructure failure.
    Stream,
    /// Credential storage or ambient auth failure.
    Auth,
    /// OAuth login/refresh failure.
    Oauth,
}

impl ModelsErrorCode {
    /// Stable `snake_case` code string used by the TypeScript surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelSource => "model_source",
            Self::ModelValidation => "model_validation",
            Self::Provider => "provider",
            Self::Stream => "stream",
            Self::Auth => "auth",
            Self::Oauth => "oauth",
        }
    }
}

impl fmt::Display for ModelsErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ModelsErrorCode {
    type Err = ModelsErrorCodeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "model_source" => Ok(Self::ModelSource),
            "model_validation" => Ok(Self::ModelValidation),
            "provider" => Ok(Self::Provider),
            "stream" => Ok(Self::Stream),
            "auth" => Ok(Self::Auth),
            "oauth" => Ok(Self::Oauth),
            other => Err(ModelsErrorCodeParseError {
                value: other.to_owned(),
            }),
        }
    }
}

/// Failure parsing a [`ModelsErrorCode`] string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelsErrorCodeParseError {
    value: String,
}

impl fmt::Display for ModelsErrorCodeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown models error code: {}", self.value)
    }
}

impl std::error::Error for ModelsErrorCodeParseError {}

/// Shared models/auth failure with a stable code.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ModelsError {
    /// Stable error classification.
    pub code: ModelsErrorCode,
    message: String,
    cancelled: bool,
}

impl ModelsError {
    /// Create a models error from a code and message.
    #[must_use]
    pub fn new(code: ModelsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            cancelled: false,
        }
    }

    /// Create a request-cancellation error without expanding the stable
    /// [`ModelsErrorCode`] wire contract.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            code: ModelsErrorCode::Oauth,
            message: "Login cancelled".to_owned(),
            cancelled: true,
        }
    }

    /// Whether auth resolution stopped because its request was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Human-readable error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Interactive login/auth-flow failure.
#[derive(Clone, Debug, thiserror::Error)]
pub enum AuthError {
    /// User cancelled the login flow.
    #[error("Login cancelled")]
    Cancelled,
    /// Flow-specific failure message.
    #[error("{0}")]
    Message(String),
}

impl AuthError {
    /// Create a message-carrying auth error.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Credential-store persistence or mutation failure.
#[derive(Clone, Debug, thiserror::Error)]
pub enum StoreError {
    /// Generic storage failure.
    #[error("{0}")]
    Message(String),
    /// Failure raised by a `modify` callback.
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl StoreError {
    /// Create a message-carrying store error.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_error_code_roundtrips_snake_case() -> Result<(), ModelsErrorCodeParseError> {
        for code in [
            ModelsErrorCode::ModelSource,
            ModelsErrorCode::ModelValidation,
            ModelsErrorCode::Provider,
            ModelsErrorCode::Stream,
            ModelsErrorCode::Auth,
            ModelsErrorCode::Oauth,
        ] {
            let text = code.as_str();
            let parsed: ModelsErrorCode = text.parse()?;
            assert_eq!(parsed, code);
            assert_eq!(parsed.to_string(), text);
        }
        Ok(())
    }

    #[test]
    fn models_error_preserves_code_and_message() {
        let err = ModelsError::new(ModelsErrorCode::Oauth, "refresh failed");
        assert_eq!(err.code, ModelsErrorCode::Oauth);
        assert_eq!(err.message(), "refresh failed");
        assert_eq!(err.to_string(), "refresh failed");
    }
}
