use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use percent_encoding::{percent_decode_str, percent_encode_byte};
use pi_agent::session::{SessionError, StorageErrorCode, StorageFailure};

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
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
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
        .map(|character| {
            if matches!(character, ':' | '.') {
                '-'
            } else {
                character
            }
        })
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
    let path_for_worker_error = directory.clone();
    tokio::task::spawn_blocking(move || {
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
                    io_failure(
                        &path_for_error,
                        "failed to inspect sessions directory",
                        source,
                    )
                })?;
                if entry
                    .file_type()
                    .map_err(|source| {
                        io_failure(&path_for_error, "failed to inspect session file", source)
                    })?
                    .is_dir()
                {
                    continue;
                }
                if entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(&duplicate_suffix)
                {
                    return Err(SessionError::Invariant(format!(
                        "Session already exists: {id_for_error}"
                    )));
                }
            }
        }
        fs::create_dir_all(&directory).map_err(|source| {
            io_failure(
                &path_for_error,
                "failed to create sessions directory",
                source,
            )
        })?;
        Ok(directory.join(file_name))
    })
    .await
    .map_err(|source| io_failure(&path_for_worker_error, "session path worker failed", source))?
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
    let path_for_worker_error = root.clone();
    tokio::task::spawn_blocking(move || {
        let directories = if let Some(target) = target {
            vec![root.join(target)]
        } else {
            let entries = match fs::read_dir(&root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(source) => {
                    return Err(io_failure(
                        &path_for_error,
                        "failed to list sessions root",
                        source,
                    ));
                }
            };
            let mut directories = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|source| {
                    io_failure(&path_for_error, "failed to inspect sessions root", source)
                })?;
                let is_directory = entry
                    .file_type()
                    .map_err(|source| {
                        io_failure(
                            &path_for_error,
                            "failed to inspect session directory",
                            source,
                        )
                    })?
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
                    .map_err(|source| {
                        io_failure(&directory, "failed to inspect session file", source)
                    })?
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
    .map_err(|source| {
        io_failure(
            &path_for_worker_error,
            "session listing worker failed",
            source,
        )
    })?
}

/// Removes a session file. A missing file is an error.
pub(super) async fn remove_session_file(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    let path_for_worker_error = path.clone();
    tokio::task::spawn_blocking(move || {
        fs::remove_file(&path)
            .map_err(|source| io_failure(&path_for_error, "failed to remove session", source))
    })
    .await
    .map_err(|source| {
        io_failure(
            &path_for_worker_error,
            "session removal worker failed",
            source,
        )
    })?
}

/// Removes a session file left behind by a failed admission. A missing file is
/// tolerated; other filesystem failures remain errors.
pub(super) async fn discard_session_file(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    let path_for_worker_error = path.clone();
    tokio::task::spawn_blocking(move || match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_failure(
            &path_for_error,
            "failed to remove session",
            source,
        )),
    })
    .await
    .map_err(|source| {
        io_failure(
            &path_for_worker_error,
            "session removal worker failed",
            source,
        )
    })?
}

/// Verifies that `path` is the repository-owned session file for `id` below
/// `root`'s encoded `cwd` directory.
///
/// `JsonlSessionMetadata.path` is caller-supplied (the record is
/// deserializable), so the root, candidate file, and expected directory are
/// canonicalized before comparison. The directory must remain the root's
/// expected direct child, and the file must sit directly inside that directory.
/// The complete id after the first underscore must match, raw for legacy files
/// or percent-encoded. The timestamp prefix is deliberately not compared: a
/// legacy header can normalize to a creation time that does not reproduce the
/// original file name.
pub(super) async fn verify_owned_session_path(
    root: &Path,
    cwd: &str,
    id: &str,
    path: &Path,
) -> Result<(), SessionError> {
    let directory = root.join(session_directory_name(cwd));
    let root = root.to_path_buf();
    let id = id.to_owned();
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    let path_for_worker_error = path.clone();
    tokio::task::spawn_blocking(move || {
        let canonical_file = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SessionError::Backend(StorageFailure::new(
                    StorageErrorCode::NotFound,
                    format!("session file does not exist: {}", path_for_error.display()),
                )));
            }
            Err(source) => {
                return Err(io_failure(
                    &path_for_error,
                    "failed to resolve session path",
                    source,
                ));
            }
        };
        let owned = fs::canonicalize(&root).is_ok_and(|canonical_root| {
            fs::canonicalize(&directory).is_ok_and(|canonical_dir| {
                canonical_dir.parent() == Some(canonical_root.as_path())
                    && canonical_dir.file_name() == directory.file_name()
                    && canonical_file.starts_with(canonical_root.as_path())
                    && canonical_file.parent() == Some(canonical_dir.as_path())
                    && canonical_file
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| name.strip_suffix(".jsonl"))
                        .and_then(|stem| stem.split_once('_'))
                        .is_some_and(|(timestamp, file_id)| {
                            !timestamp.is_empty()
                                && (file_id == id
                                    || (file_id == encode_session_id(&id)
                                        && percent_decode_str(file_id)
                                            .decode_utf8()
                                            .is_ok_and(|decoded| decoded == id)))
                        })
            })
        });
        if owned {
            Ok(())
        } else {
            Err(SessionError::Invariant(format!(
                "Session file is not owned by this repository: {}",
                path_for_error.display()
            )))
        }
    })
    .await
    .map_err(|source| io_failure(&path_for_worker_error, "session path worker failed", source))?
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn ownership_matches_the_complete_id() -> Result<(), Box<dyn Error>> {
        let root = tempdir()?;
        let directory = root.path().join(session_directory_name("/cwd"));
        fs::create_dir_all(&directory)?;
        for id in ["q_abc", "q_bc", "space id", "日本語", "literal%20"] {
            let path = directory.join(session_file_name(0, id));
            fs::write(&path, "")?;
            verify_owned_session_path(root.path(), "/cwd", id, &path).await?;
            let result = verify_owned_session_path(root.path(), "/cwd", "bc", &path).await;
            assert!(matches!(result, Err(SessionError::Invariant(_))));
        }
        let legacy = directory.join("different-timestamp_raw space_id.jsonl");
        fs::write(&legacy, "")?;
        verify_owned_session_path(root.path(), "/cwd", "raw space_id", &legacy).await?;

        // A shorter id must not match the trailing component of an id that
        // contains an underscore.
        let q_abc = directory.join(session_file_name(0, "q_abc"));
        let result = verify_owned_session_path(root.path(), "/cwd", "abc", &q_abc).await;
        assert!(matches!(result, Err(SessionError::Invariant(_))));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ownership_rejects_storage_symlink_outside_root() -> Result<(), Box<dyn Error>> {
        let root = tempdir()?;
        let foreign = tempdir()?;
        let directory = root.path().join(session_directory_name("/cwd"));
        std::os::unix::fs::symlink(foreign.path(), &directory)?;
        let path = directory.join(session_file_name(0, "shared-id"));
        fs::write(&path, "foreign session")?;
        let result = verify_owned_session_path(root.path(), "/cwd", "shared-id", &path).await;
        assert!(matches!(result, Err(SessionError::Invariant(_))));
        assert_eq!(fs::read_to_string(&path)?, "foreign session");
        Ok(())
    }
}
