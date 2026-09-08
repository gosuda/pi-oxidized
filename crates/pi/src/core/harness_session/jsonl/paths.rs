use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pi_agent::session::{SessionError, StorageErrorCode, StorageFailure};
use percent_encoding::percent_encode_byte;

use crate::core::sessions::{encode_cwd_for_session_dir, entries::iso_from_millis};

/// Encodes a caller-provided session id exactly as JavaScript
/// `encodeURIComponent` does.
///
/// The unescaped set is ASCII letters, digits, `-`, `_`, `.`, `!`, `~`, `*`,
/// `'`, and `(` or `)`. Every other UTF-8 byte is emitted as an uppercase
/// percent escape, including path separators and NUL bytes.
#[must_use]
pub(super) fn encode_session_id(id: &str) -> String {
    let mut encoded = String::with_capacity(id.len());
    for &byte in id.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(percent_encode_byte(byte));
        }
    }
    encoded
}

/// Returns the directory component shared by the product's session stores.
///
/// This is the same lossy cwd encoding used by the legacy session manager:
/// one leading slash or backslash is removed, then separators and drive-colons
/// become `-` and the result is wrapped in `--`.
#[must_use]
pub(super) fn session_directory_name(cwd: &str) -> String {
    encode_cwd_for_session_dir(cwd)
}

/// Builds a session filename from its millisecond creation time and id.
#[must_use]
pub(super) fn session_file_name(created_at: i64, id: &str) -> String {
    let timestamp: String = iso_from_millis(created_at)
        .chars()
        .map(|character| if matches!(character, ':' | '.') { '-' } else { character })
        .collect();
    format!("{timestamp}_{}.jsonl", encode_session_id(id))
}

/// Resolves a fresh session path, creating its directory and rejecting an id
/// already represented by a file in that directory.
///
/// Filesystem work is moved to Tokio's blocking pool because this helper is
/// used from asynchronous repository methods.
pub(super) async fn resolve_new_session_path(
    directory: &Path,
    created_at: i64,
    id: &str,
) -> Result<PathBuf, SessionError> {
    let directory = directory.to_path_buf();
    let file_name = session_file_name(created_at, id);
    let duplicate_suffix = format!("_{}.jsonl", encode_session_id(id));
    let id_for_error = id.to_owned();
    let path_for_error = directory.clone();
    let result = tokio::task::spawn_blocking(move || {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(io_failure(
                    &path_for_error,
                    "failed to list sessions directory",
                    source,
                ));
            }
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry = entry.map_err(|source| {
                    io_failure(&path_for_error, "failed to inspect sessions directory", source)
                })?;
                if entry
                    .file_type()
                    .map_err(|source| io_failure(&path_for_error, "failed to inspect session file", source))?
                    .is_dir()
                {
                    continue;
                }
                if entry.file_name().to_string_lossy().ends_with(&duplicate_suffix) {
                    return Err(SessionError::Invariant(format!(
                        "Session already exists: {id_for_error}"
                    )));
                }
            }
        }
        fs::create_dir_all(&directory)
            .map_err(|source| io_failure(&path_for_error, "failed to create sessions directory", source))?;
        Ok(directory.join(file_name))
    })
    .await
    .map_err(|source| io_failure(&path_for_error, "session path worker failed", source))?;
    result
}

/// Lists session files below encoded cwd directories.
///
/// When `cwd` is `Some`, only that encoded directory is inspected. With
/// `None`, every directory directly below `root` is inspected — the source
/// repository scans all directories rather than filtering on the `--…--`
/// encoding. Missing roots or target directories produce an empty list. Only
/// non-directory entries ending in `.jsonl` are returned.
pub(super) async fn list_session_files(
    root: &Path,
    cwd: Option<&str>,
) -> Result<Vec<PathBuf>, SessionError> {
    let root = root.to_path_buf();
    let target = cwd.map(session_directory_name);
    let path_for_error = root.clone();
    tokio::task::spawn_blocking(move || {
        let directories = if let Some(target) = target {
            vec![root.join(target)]
        } else {
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(source) => {
                    return Err(io_failure(&path_for_error, "failed to list sessions root", source));
                }
            };
            let mut directories = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|source| {
                    io_failure(&path_for_error, "failed to inspect sessions root", source)
                })?;
                let is_directory = entry
                    .file_type()
                    .map_err(|source| io_failure(&path_for_error, "failed to inspect session directory", source))?
                    .is_dir();
                if is_directory {
                    directories.push(entry.path());
                }
            }
            directories
        };

        let mut files = Vec::new();
        for directory in directories {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(io_failure(
                        &directory,
                        "failed to list sessions directory",
                        source,
                    ));
                }
            };
            for entry in entries {
                let entry = entry.map_err(|source| {
                    io_failure(&directory, "failed to inspect session file", source)
                })?;
                if entry
                    .file_type()
                    .map_err(|source| io_failure(&directory, "failed to inspect session file", source))?
                    .is_dir()
                {
                    continue;
                }
                if entry.file_name().to_string_lossy().ends_with(".jsonl") {
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        Ok(files)
    })
    .await
    .map_err(|source| io_failure(&path_for_error, "session listing worker failed", source))?
}

/// Removes a session file. A missing file is an error.
pub(super) async fn remove_session_file(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    tokio::task::spawn_blocking(move || {
        fs::remove_file(&path)
            .map_err(|source| io_failure(&path_for_error, "failed to remove session", source))
    })
    .await
    .map_err(|source| io_failure(&path_for_error, "session removal worker failed", source))?
}

/// Removes a session file left behind by a failed admission. A missing file is
/// tolerated; other filesystem failures remain errors.
pub(super) async fn discard_session_file(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    tokio::task::spawn_blocking(move || {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_failure(&path_for_error, "failed to remove session", source)),
        }
    })
    .await
    .map_err(|source| io_failure(&path_for_error, "session removal worker failed", source))?
}

/// Wraps an owned filesystem or worker error without discarding its cause.
pub(super) fn io_failure(
    path: &Path,
    action: &str,
    source: impl Error + Send + Sync + 'static,
) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Io,
        message: format!("{action}: {}", path.display()),
        source: Some(std::sync::Arc::new(source)),
    })
}
