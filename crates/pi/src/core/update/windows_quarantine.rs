//! Windows-compatible quarantine for a running executable.
//!
//! The TypeScript application quarantines loaded native addons. Native pi has
//! no addon graph, so the equivalent locked artifact is the running executable.

use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

use super::self_update::UpdateFileSystem;

/// Quarantine directory name retained for compatibility with pi cleanup.
pub const QUARANTINE_DIR_NAME: &str = ".pi-native-quarantine";

/// Details needed to inspect or clean one quarantine operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineRecord {
    /// Run-specific quarantine directory.
    pub run_dir: PathBuf,
    /// Renamed executable inside the quarantine.
    pub quarantined_binary: PathBuf,
}

/// Quarantine failure.
#[derive(Debug, Error)]
pub enum QuarantineError {
    /// Running path has no filename or parent directory.
    #[error("running binary path is not a file path: {0}")]
    InvalidPath(PathBuf),
    /// Filesystem operation failed.
    #[error("quarantine file operation failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Return the quarantine root beside the executable.
#[must_use]
pub fn quarantine_root(running_binary: &Path) -> Option<PathBuf> {
    running_binary
        .parent()
        .map(|parent| parent.join(QUARANTINE_DIR_NAME))
}

/// Best-effort removal of stale quarantines. Repeated calls are harmless.
///
/// Mirrors the TypeScript `cleanupWindowsSelfUpdateQuarantine` which wraps
/// `rmSync` in a `try/catch` that swallows all errors: a previous pi process
/// may still be exiting and holding a file lock.
pub fn cleanup_windows_self_update_quarantine(
    filesystem: &dyn UpdateFileSystem,
    running_binary: &Path,
) {
    if let Some(root) = quarantine_root(running_binary).filter(|r| filesystem.exists(r)) {
        // Swallow errors like the TypeScript counterpart.
        let _ = filesystem.remove_dir_all(&root);
    }
}

/// Rename the running binary away, then copy it back so the current process can
/// continue while an installer replaces the original path.
///
/// # Errors
///
/// Returns [`QuarantineError::InvalidPath`] when the path has no parent or
/// filename, and [`QuarantineError::Io`] for rename, copy, or rollback failure.
pub fn quarantine_running_binary_for_update(
    filesystem: &dyn UpdateFileSystem,
    running_binary: &Path,
) -> Result<QuarantineRecord, QuarantineError> {
    let parent = running_binary
        .parent()
        .ok_or_else(|| QuarantineError::InvalidPath(running_binary.to_path_buf()))?;
    let filename = running_binary
        .file_name()
        .ok_or_else(|| QuarantineError::InvalidPath(running_binary.to_path_buf()))?;
    let run_dir = parent.join(QUARANTINE_DIR_NAME).join(format!(
        "{}-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_second(),
        Uuid::new_v4()
    ));
    let quarantined_binary = run_dir.join(filename);
    filesystem.create_dir_all(&run_dir)?;
    filesystem.rename(running_binary, &quarantined_binary)?;
    if let Err(copy_error) = filesystem.copy(&quarantined_binary, running_binary) {
        return match filesystem.rename(&quarantined_binary, running_binary) {
            Ok(()) => Err(QuarantineError::Io(copy_error)),
            Err(rollback_error) => Err(QuarantineError::Io(std::io::Error::new(
                rollback_error.kind(),
                format!("copy failed ({copy_error}); rollback failed ({rollback_error})"),
            ))),
        };
    }
    Ok(QuarantineRecord {
        run_dir,
        quarantined_binary,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::core::update::self_update::StdUpdateFileSystem;

    #[test]
    fn quarantine_preserves_live_path_and_cleanup_is_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let binary = temp.path().join("pi.exe");
        fs::write(&binary, b"running")?;
        let record = quarantine_running_binary_for_update(&StdUpdateFileSystem, &binary)?;
        assert_eq!(fs::read(&binary)?, b"running");
        assert_eq!(fs::read(&record.quarantined_binary)?, b"running");
        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        assert!(!record.run_dir.exists());
        Ok(())
    }

    #[test]
    fn quarantine_root_returns_parent_quarantine_dir() {
        let root = quarantine_root(Path::new("/opt/pi/bin/pi.exe"));
        assert_eq!(
            root,
            Some(PathBuf::from("/opt/pi/bin/.pi-native-quarantine"))
        );

        let root = quarantine_root(Path::new("pi"));
        assert_eq!(root, Some(PathBuf::from(".pi-native-quarantine")));
    }

    #[test]
    fn quarantine_root_returns_none_for_root_path() {
        // On all platforms, "/" has no parent.
        let root = quarantine_root(Path::new("/"));
        assert!(root.is_none());
    }

    #[test]
    #[cfg(windows)]
    fn quarantine_root_returns_none_for_windows_drive_root() {
        let root = quarantine_root(Path::new("C:\\"));
        assert!(root.is_none());
    }

    #[test]
    fn cleanup_is_noop_when_no_parent() {
        // Path has no parent; cleanup does nothing and does not panic.
        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, Path::new("/"));
    }

    #[test]
    fn cleanup_succeeds_when_no_quarantine_exists() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let binary = temp.path().join("pi.exe");
        fs::write(&binary, b"running")?;
        // No quarantine has been created yet.
        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        Ok(())
    }

    /// Filesystem that always fails on `remove_dir_all`, simulating a locked file.
    struct RemoveDirFailingFS;
    impl UpdateFileSystem for RemoveDirFailingFS {
        fn exists(&self, _path: &Path) -> bool {
            true
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            std::fs::rename(from, to)
        }
        fn copy(&self, from: &Path, to: &Path) -> std::io::Result<u64> {
            std::fs::copy(from, to)
        }
        fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            std::fs::remove_file(path)
        }
        fn remove_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "locked",
            ))
        }
        fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
            std::fs::create_dir_all(path)
        }
        fn permissions(&self, path: &Path) -> std::io::Result<std::fs::Permissions> {
            std::fs::metadata(path).map(|m| m.permissions())
        }
        fn set_permissions(&self, path: &Path, perm: std::fs::Permissions) -> std::io::Result<()> {
            std::fs::set_permissions(path, perm)
        }
    }

    #[test]
    fn cleanup_swallows_remove_dir_all_failure() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let binary = temp.path().join("pi.exe");
        fs::write(&binary, b"running")?;
        let record = quarantine_running_binary_for_update(&StdUpdateFileSystem, &binary)?;
        assert!(record.run_dir.exists());
        // Failing FS must not cause a panic; the TS counterpart wraps rmSync in
        // try/catch for exactly this case (a previous pi process still holds a lock).
        cleanup_windows_self_update_quarantine(&RemoveDirFailingFS, &binary);
        // The quarantine dir is still there because remove_dir_all failed.
        assert!(record.run_dir.exists());
        // Clean up with the real FS so the temp dir can be removed.
        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        Ok(())
    }

    #[test]
    fn quarantine_fails_when_binary_does_not_exist() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let missing = temp.path().join("nonexistent.exe");
        let result = quarantine_running_binary_for_update(&StdUpdateFileSystem, &missing);
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn quarantine_creates_unique_run_directory() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let binary = temp.path().join("pi.exe");
        fs::write(&binary, b"running")?;

        let record1 = quarantine_running_binary_for_update(&StdUpdateFileSystem, &binary)?;
        // Restore the binary for a second quarantine.
        fs::write(&binary, b"running2")?;
        let record2 = quarantine_running_binary_for_update(&StdUpdateFileSystem, &binary)?;

        // Each quarantine gets a unique run directory.
        assert_ne!(record1.run_dir, record2.run_dir);
        // Both quarantined binaries exist in their own directories.
        assert!(record1.quarantined_binary.exists());
        assert!(record2.quarantined_binary.exists());
        assert_eq!(fs::read(&record1.quarantined_binary)?, b"running");
        assert_eq!(fs::read(&record2.quarantined_binary)?, b"running2");

        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        Ok(())
    }

    #[test]
    fn quarantine_record_paths_are_consistent() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let binary = temp.path().join("pi.exe");
        fs::write(&binary, b"running")?;
        let record = quarantine_running_binary_for_update(&StdUpdateFileSystem, &binary)?;

        // quarantined_binary lives inside run_dir.
        assert!(record.quarantined_binary.starts_with(&record.run_dir));
        // The filename is preserved.
        assert_eq!(record.quarantined_binary.file_name(), binary.file_name());

        cleanup_windows_self_update_quarantine(&StdUpdateFileSystem, &binary);
        Ok(())
    }
}
