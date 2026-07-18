//! Error types for the agent loop and tool execution seams.

use thiserror::Error;

/// Rare infrastructure failure that escapes a no-throw hook contract.
///
/// Product hooks are documented as returning safe fallbacks. When a hook still
/// fails in a way the loop cannot continue past, the failure is surfaced as
/// [`AgentLoopError`] so the agent wrapper can synthesize a terminal sequence.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AgentLoopError {
    /// Human-readable infrastructure failure.
    #[error("{0}")]
    Message(String),
}

impl AgentLoopError {
    /// Creates an infrastructure failure from a message.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Tool prepare/validate/execute failure.
///
/// The display text becomes the error tool-result content when the loop
/// converts the failure into a transcript tool result.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct ToolError {
    message: String,
}

impl ToolError {
    /// Creates a tool failure from a message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the human-readable failure text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}
