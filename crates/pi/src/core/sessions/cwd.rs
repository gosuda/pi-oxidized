//! Missing stored-cwd detection and error helpers.
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/session-cwd.ts`.

use std::path::Path;

use thiserror::Error;

use super::entries::path_exists;

/// Issue describing a stored session cwd that no longer exists on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCwdIssue {
    /// Session file path when known.
    pub session_file: Option<String>,
    /// Working directory recorded in the session header.
    pub session_cwd: String,
    /// Fallback cwd the caller would continue in.
    pub fallback_cwd: String,
}

/// Source of session cwd / file path for missing-cwd checks.
pub trait SessionCwdSource {
    /// Resolved session working directory.
    fn get_cwd(&self) -> &str;
    /// Session file path, if any.
    fn get_session_file(&self) -> Option<&str>;
}

/// Return a missing-cwd issue when the session file is set, the stored cwd is
/// non-empty, and that path does not exist.
#[must_use]
pub fn get_missing_session_cwd_issue(
    session_manager: &impl SessionCwdSource,
    fallback_cwd: &str,
) -> Option<SessionCwdIssue> {
    let session_file = session_manager.get_session_file()?;
    let session_cwd = session_manager.get_cwd();
    if session_cwd.is_empty() || path_exists(Path::new(session_cwd)) {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file.to_owned()),
        session_cwd: session_cwd.to_owned(),
        fallback_cwd: fallback_cwd.to_owned(),
    })
}

/// Format the controlled missing-cwd error string (exact TypeScript text).
#[must_use]
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map_or(String::new(), |p| format!("\nSession file: {p}"));
    format!(
        "Stored session working directory does not exist: {}{}\nCurrent working directory: {}",
        issue.session_cwd, session_file, issue.fallback_cwd
    )
}

/// Format the interactive missing-cwd prompt (exact TypeScript text).
#[must_use]
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// Error thrown when a stored session cwd is missing on disk.
///
/// The error name is `"MissingSessionCwdError"` (via [`std::any::type_name`]
/// consumers / Display); the message matches TypeScript exactly.
#[derive(Debug, Error)]
#[error("{}", format_missing_session_cwd_error(&self.issue))]
pub struct MissingSessionCwdError {
    /// The detected missing-cwd issue.
    pub issue: SessionCwdIssue,
}

impl MissingSessionCwdError {
    /// Construct from a [`SessionCwdIssue`].
    #[must_use]
    pub fn new(issue: SessionCwdIssue) -> Self {
        Self { issue }
    }
}

/// Assert the session cwd exists; return [`MissingSessionCwdError`] otherwise.
///
/// # Errors
///
/// Returns [`MissingSessionCwdError`] when the session has a stored file path
/// and a non-empty cwd that no longer exists on disk.
pub fn assert_session_cwd_exists(
    session_manager: &impl SessionCwdSource,
    fallback_cwd: &str,
) -> Result<(), MissingSessionCwdError> {
    if let Some(issue) = get_missing_session_cwd_issue(session_manager, fallback_cwd) {
        return Err(MissingSessionCwdError::new(issue));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource {
        cwd: String,
        file: Option<String>,
    }

    impl SessionCwdSource for FakeSource {
        fn get_cwd(&self) -> &str {
            &self.cwd
        }
        fn get_session_file(&self) -> Option<&str> {
            self.file.as_deref()
        }
    }

    #[test]
    fn detects_missing_cwd() -> Result<(), &'static str> {
        let src = FakeSource {
            cwd: "/tmp/pi-oxidized-definitely-missing-cwd-xyz".to_owned(),
            file: Some("/tmp/session.jsonl".to_owned()),
        };
        let issue = get_missing_session_cwd_issue(&src, "/tmp/fallback").ok_or("expected issue")?;
        assert_eq!(issue.session_cwd, src.cwd);
        assert_eq!(issue.fallback_cwd, "/tmp/fallback");
        assert_eq!(issue.session_file.as_deref(), Some("/tmp/session.jsonl"));

        let err = format_missing_session_cwd_error(&issue);
        assert!(err.contains("Stored session working directory does not exist:"));
        assert!(err.contains("Session file: /tmp/session.jsonl"));
        assert!(err.contains("Current working directory: /tmp/fallback"));

        let prompt = format_missing_session_cwd_prompt(&issue);
        assert!(prompt.contains("cwd from session file does not exist"));
        assert!(prompt.contains("continue in current cwd"));
        Ok(())
    }

    #[test]
    fn no_issue_when_no_session_file() {
        let src = FakeSource {
            cwd: "/tmp/missing".to_owned(),
            file: None,
        };
        assert!(get_missing_session_cwd_issue(&src, "/tmp").is_none());
    }

    #[test]
    fn no_issue_when_cwd_exists() {
        let src = FakeSource {
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            file: Some("/tmp/session.jsonl".to_owned()),
        };
        assert!(get_missing_session_cwd_issue(&src, "/tmp").is_none());
    }

    #[test]
    fn assert_throws_missing_cwd_error() -> Result<(), &'static str> {
        let src = FakeSource {
            cwd: "/tmp/pi-oxidized-definitely-missing-cwd-xyz".to_owned(),
            file: Some("/tmp/session.jsonl".to_owned()),
        };
        let Err(err) = assert_session_cwd_exists(&src, "/tmp/fallback") else {
            return Err("expected MissingSessionCwdError");
        };
        assert_eq!(err.issue.session_cwd, src.cwd);
        Ok(())
    }
}
