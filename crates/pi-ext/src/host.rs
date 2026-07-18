//! Host executable resolution and spawn specification.
//!
//! The bundled TypeScript extension host is **source-pinned**: Rust only ever
//! launches the host referenced by `PI_EXTENSION_HOST` or the sibling asset
//! installed beside the `pi` binary. It never searches `PATH`, so a stray
//! `pi-extension-host` elsewhere cannot be selected. When no host is available
//! the resolver returns a clear typed error rather than falling back to an
//! untrusted executable.
//!
//! Resolving a host does not start a process; see [`crate::client`].

use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Environment variable selecting an explicit host executable.
pub const ENV_HOST: &str = "PI_EXTENSION_HOST";

/// Default sibling host executable name (without platform extension).
pub const DEFAULT_HOST_NAME: &str = "pi-extension-host";

/// Windows host executable suffix.
#[cfg(windows)]
const HOST_EXE_SUFFIX: &str = ".exe";
#[cfg(not(windows))]
const HOST_EXE_SUFFIX: &str = "";

/// How a host executable was located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostSource {
    /// Selected via [`ENV_HOST`].
    Env(PathBuf),
    /// Discovered beside the running binary.
    InstalledAsset(PathBuf),
}

/// A resolved, spawn-ready host program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    /// How the program was located.
    pub source: HostSource,
    /// Absolute-ish program path to execute.
    pub program: PathBuf,
    /// Extra argv passed after the program path.
    pub args: Vec<String>,
}

/// Host resolution failure.
#[derive(Debug, Error)]
pub enum HostError {
    /// No host configured: neither env var nor installed asset present.
    #[error(
        "extension host not configured: set {env} or install the bundled host beside the binary"
    )]
    NotConfigured {
        /// The environment variable name that would select a host.
        env: &'static str,
    },
    /// The resolved path is not a regular file.
    #[error("extension host path is not a file: {0}")]
    NotAFile(PathBuf),
    /// The host path could not be converted to UTF-8 for argv.
    #[error("extension host path is not valid UTF-8: {0}")]
    NonUtf8(PathBuf),
}

/// Resolve a host using the process environment and the default asset search.
///
/// # Errors
///
/// Returns [`HostError::NotConfigured`] when no host is found.
pub fn resolve_host() -> Result<HostSpec, HostError> {
    let env_value = env::var_os(ENV_HOST);
    let env_str = env_value
        .as_ref()
        .map(|os| os.to_string_lossy().into_owned());
    let asset = default_asset_path();
    let asset_ref = asset.as_deref();
    resolve_with(env_str.as_deref(), asset_ref)
}

/// Resolve a host from an explicit env override and an optional asset path.
///
/// `env` wins when present and points at a file; otherwise `asset` is used.
///
/// # Errors
///
/// Returns [`HostError::NotConfigured`] when neither yields a file, or
/// [`HostError::NotAFile`] / [`HostError::NonUtf8`] for malformed inputs.
pub fn resolve_with(env: Option<&str>, asset: Option<&Path>) -> Result<HostSpec, HostError> {
    if let Some(raw) = env {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            check_file(&path)?;
            return Ok(HostSpec {
                source: HostSource::Env(path.clone()),
                program: path,
                args: Vec::new(),
            });
        }
    }
    if let Some(path) = asset
        && path.is_file()
    {
        check_utf8(path)?;
        return Ok(HostSpec {
            source: HostSource::InstalledAsset(path.to_path_buf()),
            program: path.to_path_buf(),
            args: Vec::new(),
        });
    }
    Err(HostError::NotConfigured { env: ENV_HOST })
}

/// Path of the sibling host asset beside the current executable, if any.
///
/// Returns `None` when the current executable path cannot be determined.
#[must_use]
pub fn default_asset_path() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let name = format!("{DEFAULT_HOST_NAME}{HOST_EXE_SUFFIX}");
    Some(dir.join(name))
}

fn check_file(path: &Path) -> Result<(), HostError> {
    if path.is_file() {
        check_utf8(path)?;
        Ok(())
    } else {
        Err(HostError::NotAFile(path.to_path_buf()))
    }
}

fn check_utf8(path: &Path) -> Result<(), HostError> {
    if path.to_str().is_some() {
        Ok(())
    } else {
        Err(HostError::NonUtf8(path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use tempfile::tempdir;

    type R = Result<(), Box<dyn Error>>;

    #[test]
    fn not_configured_when_neither_present() -> R {
        match resolve_with(None, None) {
            Err(HostError::NotConfigured { env: ENV_HOST }) => Ok(()),
            other => {
                Err(std::io::Error::other(format!("expected NotConfigured, got {other:?}")).into())
            }
        }
    }

    #[test]
    fn env_wins_when_file_exists() -> R {
        let dir = tempdir()?;
        let env_host = dir.path().join("from-env");
        fs::write(&env_host, b"#!bin\n")?;
        let asset = dir.path().join("pi-extension-host");
        fs::write(&asset, b"#!bin\n")?;
        let spec = resolve_with(env_host.to_str(), Some(asset.as_path()))?;
        assert_eq!(spec.source, HostSource::Env(env_host.clone()));
        assert_eq!(spec.program, env_host);
        Ok(())
    }

    #[test]
    fn asset_used_when_no_env() -> R {
        let dir = tempdir()?;
        let asset = dir.path().join("pi-extension-host");
        fs::write(&asset, b"#!bin\n")?;
        let spec = resolve_with(None, Some(asset.as_path()))?;
        assert!(matches!(spec.source, HostSource::InstalledAsset(_)));
        assert_eq!(spec.program, asset);
        Ok(())
    }

    #[test]
    fn env_pointing_at_missing_file_is_not_a_file() -> R {
        let dir = tempdir()?;
        let missing = dir.path().join("nope");
        match resolve_with(missing.to_str(), None) {
            Err(HostError::NotAFile(_)) => Ok(()),
            other => Err(std::io::Error::other(format!("expected NotAFile, got {other:?}")).into()),
        }
    }

    #[test]
    fn empty_env_falls_through_to_asset() -> R {
        let dir = tempdir()?;
        let asset = dir.path().join("pi-extension-host");
        fs::write(&asset, b"#!bin\n")?;
        let spec = resolve_with(Some("  "), Some(asset.as_path()))?;
        assert!(matches!(spec.source, HostSource::InstalledAsset(_)));
        Ok(())
    }

    #[test]
    fn default_asset_path_shape() -> R {
        let path = default_asset_path().ok_or_else(|| std::io::Error::other("no asset path"))?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name == "pi-extension-host" || name == "pi-extension-host.exe",
            "unexpected asset name: {name}"
        );
        Ok(())
    }
}
