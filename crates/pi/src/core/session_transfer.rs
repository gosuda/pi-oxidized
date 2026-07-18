//! JSONL branch export and import preparation.
//!
//! Runtime teardown and replacement are deliberately outside this module. An
//! import returns a typed handoff request for the owning runtime to apply.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use super::config::resolve_path;
use super::sessions::{
    CURRENT_SESSION_VERSION, MissingSessionCwdError, SessionError, SessionHeader, SessionManager,
    assert_session_cwd_exists, now_iso,
};

/// A missing JSONL import source.
#[derive(Debug, Error)]
#[error("File not found: {file_path}")]
pub struct SessionImportFileNotFoundError {
    /// Fully resolved source path.
    pub file_path: String,
}

impl SessionImportFileNotFoundError {
    /// Construct from a resolved path.
    #[must_use]
    pub fn new(file_path: impl Into<String>) -> Self {
        Self {
            file_path: file_path.into(),
        }
    }
}

/// Session transfer failures.
#[derive(Debug, Error)]
pub enum SessionTransferError {
    /// Import source does not exist.
    #[error(transparent)]
    ImportFileNotFound(#[from] SessionImportFileNotFoundError),
    /// Imported session records a cwd that no longer exists.
    #[error(transparent)]
    MissingSessionCwd(#[from] MissingSessionCwdError),
    /// Session parsing, migration, or validation failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// JSON serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// Path being accessed.
        path: String,
        /// Underlying error.
        source: std::io::Error,
    },
}

/// Runtime action requested after an import is copied, opened, and validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionHandoffReason {
    /// Resume the imported session.
    Resume,
}

/// Validated import result handed to the runtime owner.
#[derive(Debug)]
pub struct SessionImportHandoff {
    /// Runtime switch reason.
    pub reason: SessionHandoffReason,
    /// Resolved import source.
    pub source_path: PathBuf,
    /// Session file used by the opened manager.
    pub destination_path: PathBuf,
    /// Opened and cwd-validated session manager.
    pub session_manager: SessionManager,
}

fn io_error(path: &Path, source: std::io::Error) -> SessionTransferError {
    SessionTransferError::Io {
        path: path.to_string_lossy().into_owned(),
        source,
    }
}

fn output_path(output_path: Option<&str>) -> PathBuf {
    output_path.map_or_else(
        || {
            let timestamp = now_iso().replace([':', '.'], "-");
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(format!("session-{timestamp}.jsonl"))
        },
        resolve_path,
    )
}

/// Export only the current branch as linear v3 JSONL.
///
/// A fresh header is followed by root-to-leaf branch entries. Every `parentId`
/// is replaced so the output is independent of branches omitted from export.
/// Parent directories are created recursively.
///
/// # Errors
///
/// Returns a JSON or filesystem error.
pub fn export_branch_to_jsonl(
    session: &SessionManager,
    requested_output: Option<&str>,
) -> Result<String, SessionTransferError> {
    let path = output_path(requested_output);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }

    let mut header =
        SessionHeader::new(session.get_session_id(), now_iso(), session.get_cwd(), None);
    header.version = Some(CURRENT_SESSION_VERSION);

    let branch = session.get_branch(None);
    let mut document = String::new();
    document.push_str(&serde_json::to_string(&header)?);
    document.push('\n');
    let mut previous_id: Option<String> = None;
    for entry in branch {
        let mut value = serde_json::to_value(entry)?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "parentId".to_owned(),
                previous_id.clone().map_or(Value::Null, Value::String),
            );
        }
        document.push_str(&serde_json::to_string(&value)?);
        document.push('\n');
        previous_id = entry.id().map(str::to_owned);
    }

    fs::write(&path, document).map_err(|source| io_error(&path, source))?;
    Ok(path.to_string_lossy().into_owned())
}

fn paths_refer_to_same_file(source: &Path, destination: &Path) -> bool {
    if source == destination {
        return true;
    }
    same_file::is_same_file(source, destination).unwrap_or(false)
}

/// Copy, open, and cwd-validate an imported JSONL session.
///
/// This function performs no lifecycle hooks and does not tear down the active
/// runtime. The caller may run its cancelable `session_before_switch` hook
/// before calling this primitive, then consume the returned handoff.
///
/// # Errors
///
/// Returns a distinct file-not-found error, copy/open failures, or the existing
/// typed missing-session-cwd error.
pub fn prepare_jsonl_import(
    input: &str,
    session_dir: &str,
    cwd_override: Option<&str>,
    fallback_cwd: &str,
) -> Result<SessionImportHandoff, SessionTransferError> {
    let source = resolve_path(input);
    if !source.exists() {
        return Err(SessionImportFileNotFoundError {
            file_path: source.to_string_lossy().into_owned(),
        }
        .into());
    }

    let directory = resolve_path(session_dir);
    fs::create_dir_all(&directory).map_err(|error| io_error(&directory, error))?;
    let file_name = source.file_name().ok_or_else(|| SessionTransferError::Io {
        path: source.to_string_lossy().into_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "import path has no file name",
        ),
    })?;
    let destination = directory.join(file_name);

    if !paths_refer_to_same_file(&source, &destination) {
        fs::copy(&source, &destination).map_err(|error| io_error(&destination, error))?;
    }

    let destination_text = destination.to_string_lossy().into_owned();
    let directory_text = directory.to_string_lossy().into_owned();
    let session_manager =
        SessionManager::open(&destination_text, Some(&directory_text), cwd_override)?;
    assert_session_cwd_exists(&session_manager, fallback_cwd)?;

    Ok(SessionImportHandoff {
        reason: SessionHandoffReason::Resume,
        source_path: source,
        destination_path: destination,
        session_manager,
    })
}

/// Compatibility name for the import preparation primitive.
///
/// # Errors
///
/// See [`prepare_jsonl_import`].
pub fn import_jsonl_into_session_dir(
    input: &str,
    session_dir: &str,
    cwd_override: Option<&str>,
    fallback_cwd: &str,
) -> Result<SessionImportHandoff, SessionTransferError> {
    prepare_jsonl_import(input, session_dir, cwd_override, fallback_cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_fixture(
        path: &Path,
        cwd: &Path,
        entries: &[Value],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "fixture-session",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd,
        });
        let mut text = serde_json::to_string(&header)?;
        text.push('\n');
        for entry in entries {
            text.push_str(&serde_json::to_string(entry)?);
            text.push('\n');
        }
        fs::write(path, text)?;
        Ok(())
    }

    fn message(id: &str, parent_id: Option<&str>, text: &str) -> Value {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent_id,
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {"role": "user", "content": text, "timestamp": 1}
        })
    }

    #[test]
    fn exports_current_branch_with_new_header_and_linear_parents()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let source = root.path().join("source.jsonl");
        write_fixture(
            &source,
            root.path(),
            &[
                message("root", None, "root"),
                message("discarded", Some("root"), "discarded"),
                message("leaf", Some("root"), "leaf"),
            ],
        )?;
        let manager = SessionManager::open(
            &source.to_string_lossy(),
            Some(&root.path().to_string_lossy()),
            None,
        )?;
        let output = root.path().join("nested/export.jsonl");
        export_branch_to_jsonl(&manager, Some(&output.to_string_lossy()))?;
        let values: Vec<Value> = fs::read_to_string(output)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["type"], "session");
        assert_eq!(values[0]["version"], 3);
        assert_eq!(values[0]["id"], "fixture-session");
        assert_eq!(values[1]["id"], "root");
        assert_eq!(values[1]["parentId"], Value::Null);
        assert_eq!(values[2]["id"], "leaf");
        assert_eq!(values[2]["parentId"], "root");
        Ok(())
    }

    #[test]
    fn default_export_name_matches_pi_pattern() {
        let path = output_path(None);
        let name = path.file_name().and_then(|value| value.to_str());
        assert!(name.is_some_and(|value| {
            value.starts_with("session-") && value.ends_with("Z.jsonl") && !value.contains(':')
        }));
    }

    #[test]
    fn missing_import_has_exact_typed_error() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let missing = root.path().join("missing.jsonl");
        let result = prepare_jsonl_import(
            &missing.to_string_lossy(),
            &root.path().join("sessions").to_string_lossy(),
            None,
            &root.path().to_string_lossy(),
        );
        let error = result.err().ok_or("expected missing-file error")?;
        assert!(matches!(error, SessionTransferError::ImportFileNotFound(_)));
        assert_eq!(
            error.to_string(),
            format!("File not found: {}", missing.display())
        );
        Ok(())
    }

    #[test]
    fn import_copies_opens_and_returns_typed_handoff() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let source = root.path().join("source.jsonl");
        write_fixture(&source, root.path(), &[message("entry", None, "hello")])?;
        let session_dir = root.path().join("sessions");
        let handoff = prepare_jsonl_import(
            &source.to_string_lossy(),
            &session_dir.to_string_lossy(),
            None,
            &root.path().to_string_lossy(),
        )?;
        assert_eq!(handoff.reason, SessionHandoffReason::Resume);
        assert_eq!(handoff.destination_path, session_dir.join("source.jsonl"));
        assert_eq!(handoff.session_manager.get_session_id(), "fixture-session");
        assert_eq!(handoff.session_manager.get_entries().len(), 1);
        Ok(())
    }

    #[test]
    fn import_same_file_skips_copy_and_preserves_contents() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let source = root.path().join("same.jsonl");
        write_fixture(&source, root.path(), &[message("entry", None, "same")])?;
        let before = fs::read(&source)?;
        let handoff = prepare_jsonl_import(
            &source.to_string_lossy(),
            &root.path().to_string_lossy(),
            None,
            &root.path().to_string_lossy(),
        )?;
        assert_eq!(handoff.source_path, handoff.destination_path);
        assert_eq!(fs::read(source)?, before);
        Ok(())
    }

    #[test]
    fn import_asserts_stored_cwd_and_accepts_override() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let missing_cwd = root.path().join("gone");
        let source = root.path().join("missing-cwd.jsonl");
        write_fixture(&source, &missing_cwd, &[])?;
        let first_dir = root.path().join("first");
        let result = prepare_jsonl_import(
            &source.to_string_lossy(),
            &first_dir.to_string_lossy(),
            None,
            &root.path().to_string_lossy(),
        );
        assert!(matches!(
            result,
            Err(SessionTransferError::MissingSessionCwd(_))
        ));

        let second_dir = root.path().join("second");
        let handoff = prepare_jsonl_import(
            &source.to_string_lossy(),
            &second_dir.to_string_lossy(),
            Some(&root.path().to_string_lossy()),
            &root.path().to_string_lossy(),
        )?;
        assert_eq!(
            handoff.session_manager.get_cwd(),
            root.path().to_string_lossy()
        );
        Ok(())
    }
}
