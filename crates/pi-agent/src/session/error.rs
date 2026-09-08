use std::error::Error;
use std::sync::Arc;

use super::{EntryId, LaneName};

/// Domain failures of the durable session: five classes, no more.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionError {
    /// Broken durable invariant: non-monotonic seq, duplicate id, missing parent,
    /// no-progress drive, operation/state mismatch.
    #[error("session invariant violated: {0}")]
    Invariant(String),
    /// A branch reference failed validation; `reason` explains which rule was
    /// violated.
    #[error("invalid branch {branch}: {reason}")]
    InvalidBranch {
        /// Lane name that failed validation.
        branch: LaneName,
        /// Explanation of which rule was violated.
        reason: String,
    },
    /// Branch creation refused because the lane name is already taken.
    #[error("branch already exists: {0}")]
    BranchExists(LaneName),
    /// An assistant message with a still-`Pending` stop reason reached a commit
    /// point; only settled responses are durable.
    #[error("assistant message is still pending and cannot be committed")]
    PendingAssistantMessage,
    /// A caller-supplied entry id names no committed entry.
    #[error("unknown target entry: {0}")]
    UnknownTarget(EntryId),
    /// Backend/IO failure. The ONLY variant a backend may introduce.
    #[error(transparent)]
    Backend(#[from] StorageFailure),
}

/// Typed failure reported by a storage backend, carried by
/// [`SessionError::Backend`].
#[derive(Clone, Debug)]
pub struct StorageFailure {
    /// Machine-readable failure class.
    pub code: StorageErrorCode,
    /// Human-readable detail from the backend.
    pub message: String,
    /// Underlying cause when the backend has one; `None` is normal for a
    /// condition the backend detected itself.
    pub source: Option<Arc<dyn Error + Send + Sync>>,
}

impl std::fmt::Display for StorageFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for StorageFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|source| source.as_ref() as &(dyn Error + 'static))
    }
}

impl StorageFailure {
    /// Builds a failure with no underlying cause.
    #[must_use]
    pub fn new(code: StorageErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), source: None }
    }
}

/// Backend failure classes, discriminating how a caller should react.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorCode {
    /// `"not_found"` — the addressed record does not exist.
    NotFound,
    /// `"invalid_header"` — the store's header is unreadable or malformed.
    InvalidHeader,
    /// `"version_mismatch"` — the store's version is unsupported in either
    /// direction. There is no automatic migration: this is a hard error.
    VersionMismatch,
    /// `"corrupt"` — a record exists but cannot be decoded.
    Corrupt,
    /// `"io"` — the underlying read or write failed.
    Io,
    /// `"closed"` — the session or repository was already closed.
    Closed,
    /// `"aborted"` — the operation's cancellation token fired mid-call.
    Aborted
}

impl std::fmt::Display for StorageErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self { Self::NotFound => "not_found", Self::InvalidHeader => "invalid_header", Self::VersionMismatch => "version_mismatch", Self::Corrupt => "corrupt", Self::Io => "io", Self::Closed => "closed", Self::Aborted => "aborted" };
        formatter.write_str(name)
    }
}

/// Rejection from [`SettledAssistantMessage::new`](super::SettledAssistantMessage::new):
/// the message's stop reason is still `Pending`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("assistant message is still pending")]
pub struct PendingAssistantMessage;
