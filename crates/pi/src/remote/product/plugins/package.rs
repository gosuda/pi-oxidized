//! Source-C plugin package paths and build requests.
//!
//! The package builder is transport-agnostic: the extension-host process owns
//! bundling and this module only derives its deterministic output location and
//! forwards the real build request supplied by the caller.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_agent::service::value::JsonValue;
use pi_ext::facet::{FacetBundleBuildRequest, FacetBundleBuildResponse};
use pi_ext::host::HostError;
use sha2::{Digest, Sha256};

/// Default facet source entries applied by the product host.
pub const DEFAULT_PLUGIN_SESSION_ENTRY: &str = "src/session.ts";
/// Default presentation facet source entry applied by the product host.
pub const DEFAULT_PLUGIN_TUI_ENTRY: &str = "src/tui.ts";
/// Bundle manifest filename emitted by Chord.
pub const FACET_BUNDLE_MANIFEST_FILE: &str = "chord-facets.json";

/// Request callback used by [`RemotePluginPackageBuilder`].
pub type PluginBuildRequest = Arc<
    dyn Fn(
            FacetBundleBuildRequest,
        ) -> BoxFuture<'static, Result<FacetBundleBuildResponse, HostError>>
        + Send
        + Sync,
>;

/// The minimal package/build seam consumed by product service providers.
pub trait PluginPackageBuilder: Send + Sync {
    /// Returns the deterministic manifest path for one package.
    fn manifest_path(&self, package_path: &str) -> PathBuf;
    /// Builds the package and returns transportable `tui` artifacts.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] if the host build fails or produces no artifacts.
    fn build(&self, package_path: &str) -> BoxFuture<'_, Result<Vec<JsonValue>, HostError>>;
}

/// Real extension-host-backed package builder.
///
/// The callback is the request path to the running host. Keeping it injected
/// lets `pi` own routing and lifecycle while this value owns path policy.
pub struct RemotePluginPackageBuilder {
    directory: PathBuf,
    server_id: String,
    build_request: PluginBuildRequest,
}

impl RemotePluginPackageBuilder {
    /// Creates a builder for one server's plugin-build namespace.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        server_id: impl Into<String>,
        build_request: PluginBuildRequest,
    ) -> Self {
        Self {
            directory: directory.into(),
            server_id: server_id.into(),
            build_request,
        }
    }

    /// Returns the output directory used in the host build request.
    #[must_use]
    pub fn build_directory(&self, package_path: &str) -> PathBuf {
        self.directory
            .join("plugin-builds")
            .join(&self.server_id)
            .join(plugin_build_directory_name(package_path))
    }
}

impl PluginPackageBuilder for RemotePluginPackageBuilder {
    fn manifest_path(&self, package_path: &str) -> PathBuf {
        self.build_directory(package_path)
            .join(FACET_BUNDLE_MANIFEST_FILE)
    }

    /// Builds the package through the configured host request.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] if the host build fails or produces no artifacts.
    fn build(&self, package_path: &str) -> BoxFuture<'_, Result<Vec<JsonValue>, HostError>> {
        let request = FacetBundleBuildRequest {
            package_path: package_path.to_owned(),
            outdir: self
                .build_directory(package_path)
                .to_string_lossy()
                .into_owned(),
            default_facets: [
                (
                    "session".to_owned(),
                    DEFAULT_PLUGIN_SESSION_ENTRY.to_owned(),
                ),
                ("tui".to_owned(), DEFAULT_PLUGIN_TUI_ENTRY.to_owned()),
            ]
            .into_iter()
            .collect(),
        };
        let build_request = Arc::clone(&self.build_request);
        Box::pin(async move { Ok((build_request)(request).await?.tui_artifacts) })
    }
}

/// Resolves package paths to absolute lexical paths.
///
/// Callers that accept untrusted configuration should use
/// [`try_normalize_plugin_package_paths`] to reject empty and duplicate
/// entries before calling this infallible projection.
#[must_use]
pub fn normalize_plugin_package_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| normalized_package_path(path).to_string_lossy().into_owned())
        .collect()
}

/// Fallible package-path normalization for configuration boundaries.
///
/// # Errors
///
/// Returns [`HostError::NotConfigured`] when a path is empty or duplicated.
pub fn try_normalize_plugin_package_paths(paths: &[String]) -> Result<Vec<String>, HostError> {
    let normalized = normalize_plugin_package_paths(paths);
    if paths.iter().any(|path| path.trim().is_empty()) {
        return Err(HostError::NotConfigured {
            env: "PI_PLUGIN_PACKAGE",
        });
    }
    if normalized.windows(2).any(|pair| pair[0] == pair[1]) || {
        let mut unique = normalized.clone();
        unique.sort();
        unique.dedup();
        unique.len() != normalized.len()
    } {
        return Err(HostError::NotConfigured {
            env: "PI_PLUGIN_PACKAGE",
        });
    }
    Ok(normalized)
}

/// Derives the stable output directory leaf used by the source plugin host.
#[must_use]
pub fn plugin_build_directory_name(package_path: &str) -> String {
    let normalized = normalized_package_path(package_path);
    let package_leaf = if normalized
        .file_name()
        .is_some_and(|name| name == "package.json")
    {
        normalized
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_else(|| OsStr::new("plugin"))
            .to_string_lossy()
            .into_owned()
    } else {
        normalized
            .file_name()
            .unwrap_or_else(|| OsStr::new("plugin"))
            .to_string_lossy()
            .into_owned()
    };
    let sanitized: String = package_leaf
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let stem = if sanitized.ends_with("-plugin") {
        sanitized
    } else {
        format!("{sanitized}-plugin")
    };
    let mut hasher = Sha256::new();
    hasher.update(normalized.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("{stem}-{}", hex_prefix(&digest, 12))
}

fn normalized_package_path(package_path: &str) -> PathBuf {
    let candidate = Path::new(package_path);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| PathBuf::from("."), |cwd| cwd.join(candidate))
    };
    normalize_components(&absolute)
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::RootDir | Component::Prefix(_) => output.push(component.as_os_str()),
            Component::Normal(value) => output.push(value),
        }
    }
    output
}

fn hex_prefix(bytes: &[u8], digits: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if digits == 0 {
        return String::new();
    }
    let mut output = String::with_capacity(digits);
    for byte in bytes {
        let high = (byte >> 4) as usize;
        output.push(HEX[high] as char);
        if output.len() >= digits {
            output.truncate(digits);
            break;
        }
        let low = (byte & 0x0f) as usize;
        output.push(HEX[low] as char);
        if output.len() >= digits {
            output.truncate(digits);
            break;
        }
    }
    output
}
