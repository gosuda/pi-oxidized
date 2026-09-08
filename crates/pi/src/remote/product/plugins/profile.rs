//! Durable product plugin package profiles.
//!
//! Profiles are server/session scoped, written atomically with restrictive
//! permissions, and store normalized package paths only. An absent configured
//! list restores the saved profile; an explicitly empty list removes it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;

use crate::remote::schemas::ServerId;

/// Current on-disk plugin profile schema.
pub const PLUGIN_PACKAGE_PROFILE_VERSION: u32 = 1;

/// Profile operation failure.
#[derive(Debug, Error)]
pub enum PluginProfileError {
    /// Filesystem access failed.
    #[error("plugin profile I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error("plugin profile JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    /// The profile shape or version is not supported.
    #[error("invalid plugin package profile: {0}")]
    Invalid(String),
    /// A configured path could not be represented as UTF-8.
    #[error("plugin package path is not valid UTF-8: {0}")]
    NonUtf8(PathBuf),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PluginPackageProfile {
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_path: Option<String>,
    package_paths: Vec<String>,
}

/// Restores or writes the server-level package profile.
///
/// `configured == None` restores the persisted profile (or an empty list when
/// none exists). `Some(&[])` removes the profile. A non-empty explicit list is
/// normalized, persisted, and returned.
pub async fn restore_server_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
    configured: Option<&[String]>,
) -> Result<Vec<String>, PluginProfileError> {
    match configured {
        None => Ok(read_server_plugin_package_profile(directory, server_id).await?.unwrap_or_default()),
        Some(paths) if paths.is_empty() => {
            remove_server_plugin_package_profile(directory, server_id).await?;
            Ok(Vec::new())
        }
        Some(paths) => write_server_plugin_package_profile(directory, server_id, paths).await,
    }
}

/// Reads the server-scoped package profile, returning `None` when absent.
pub async fn read_server_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
) -> Result<Option<Vec<String>>, PluginProfileError> {
    let path = server_profile_path(directory, server_id);
    match read_profile(&path).await {
        Ok(profile) => {
            if profile.session_path.is_some() {
                return Err(PluginProfileError::Invalid("server profile has a session path".to_owned()));
            }
            if profile.package_paths.is_empty() {
                return Err(PluginProfileError::Invalid("server profile package paths must not be empty".to_owned()));
            }
            Ok(Some(profile.package_paths))
        }
        Err(PluginProfileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes the server-scoped package profile after path normalization.
pub async fn write_server_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
    package_paths: &[String],
) -> Result<Vec<String>, PluginProfileError> {
    let package_paths = normalize_paths(package_paths)?;
    let path = server_profile_path(directory, server_id);
    write_profile(
        &path,
        &PluginPackageProfile { version: PLUGIN_PACKAGE_PROFILE_VERSION, session_path: None, package_paths: package_paths.clone() },
    )
    .await?;
    Ok(package_paths)
}

/// Removes the server-scoped package profile. Missing files are already absent.
pub async fn remove_server_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
) -> Result<(), PluginProfileError> {
    remove_profile(&server_profile_path(directory, server_id)).await
}

/// Reads the session-scoped profile for one session path.
pub async fn read_session_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
    session_path: &str,
) -> Result<Option<Vec<String>>, PluginProfileError> {
    let path = session_profile_path(directory, server_id, session_path);
    match read_profile(&path).await {
        Ok(profile) => {
            if profile.session_path.as_deref() != Some(session_path) {
                return Err(PluginProfileError::Invalid("session path does not match profile name".to_owned()));
            }
            Ok(Some(profile.package_paths))
        }
        Err(PluginProfileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes a session-scoped profile after path normalization.
pub async fn write_session_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
    session_path: &str,
    package_paths: &[String],
) -> Result<Vec<String>, PluginProfileError> {
    if session_path.is_empty() {
        return Err(PluginProfileError::Invalid("session path must not be empty".to_owned()));
    }
    let package_paths = normalize_paths(package_paths)?;
    let path = session_profile_path(directory, server_id, session_path);
    write_profile(
        &path,
        &PluginPackageProfile {
            version: PLUGIN_PACKAGE_PROFILE_VERSION,
            session_path: Some(session_path.to_owned()),
            package_paths: package_paths.clone(),
        },
    )
    .await?;
    Ok(package_paths)
}

/// Removes a session-scoped profile. Missing files are already absent.
pub async fn remove_session_plugin_package_profile(
    directory: &Path,
    server_id: &ServerId,
    session_path: &str,
) -> Result<(), PluginProfileError> {
    remove_profile(&session_profile_path(directory, server_id, session_path)).await
}

/// Returns the deterministic server profile path.
#[must_use]
pub fn server_profile_path(directory: &Path, server_id: &ServerId) -> PathBuf {
    directory.join(format!("plugin-packages-{}.json", server_id.as_str()))
}

/// Returns the deterministic session profile path.
#[must_use]
pub fn session_profile_path(directory: &Path, server_id: &ServerId, session_path: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(session_path.as_bytes());
    let digest = hasher.finalize();
    directory.join(format!(
        "session-plugin-packages-{}-{}.json",
        server_id.as_str(),
        hex_prefix(&digest, 24)
    ))
}

async fn read_profile(path: &Path) -> Result<PluginPackageProfile, PluginProfileError> {
    let contents = fs::read_to_string(path).await?;
    let profile: PluginPackageProfile = serde_json::from_str(&contents)?;
    if profile.version != PLUGIN_PACKAGE_PROFILE_VERSION {
        return Err(PluginProfileError::Invalid(format!("unsupported version {}", profile.version)));
    }
    if profile.package_paths.iter().any(|path| path.is_empty()) {
        return Err(PluginProfileError::Invalid("package path must not be empty".to_owned()));
    }
    let mut paths = profile.package_paths.clone();
    paths.sort();
    paths.dedup();
    if paths.len() != profile.package_paths.len() {
        return Err(PluginProfileError::Invalid("package paths must be unique".to_owned()));
    }
    if profile.package_paths.iter().any(|path| !Path::new(path).is_absolute()) {
        return Err(PluginProfileError::Invalid("package paths must be absolute".to_owned()));
    }
    Ok(profile)
}

async fn write_profile(path: &Path, profile: &PluginPackageProfile) -> Result<(), PluginProfileError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(profile)?;
    fs::write(&temporary, bytes).await?;
    set_private_permissions(&temporary).await?;
    fs::rename(&temporary, path).await?;
    set_private_permissions(path).await?;
    Ok(())
}

async fn remove_profile(path: &Path) -> Result<(), PluginProfileError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn set_private_permissions(path: &Path) -> Result<(), PluginProfileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn normalize_paths(paths: &[String]) -> Result<Vec<String>, PluginProfileError> {
    let mut normalized = Vec::with_capacity(paths.len());
    for path in paths {
        if path.trim().is_empty() {
            return Err(PluginProfileError::Invalid("package path must not be empty".to_owned()));
        }
        let candidate = absolute_lexical(Path::new(path));
        let value = candidate
            .to_str()
            .ok_or_else(|| PluginProfileError::NonUtf8(candidate.clone()))?
            .to_owned();
        if normalized.iter().any(|item| item == &value) {
            return Err(PluginProfileError::Invalid("package paths must be unique".to_owned()));
        }
        normalized.push(value);
    }
    Ok(normalized)
}

fn absolute_lexical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| PathBuf::from("."), |cwd| cwd.join(path))
    };
    let mut output = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                output.pop();
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => output.push(component.as_os_str()),
            std::path::Component::Normal(value) => output.push(value),
        }
    }
    output
}

fn hex_prefix(bytes: &[u8], digits: usize) -> String {
    let mut output = String::with_capacity(digits);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
        if output.len() >= digits {
            output.truncate(digits);
            break;
        }
    }
    output
}

