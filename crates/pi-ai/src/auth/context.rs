//! Default and overlay [`AuthContext`] implementations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;

use super::types::{AuthContext, ProviderEnv};

/// Process-environment auth context with optional home-directory expansion.
///
/// `env` reads `std::env` and treats missing/blank values as unset. `file_exists`
/// expands a leading `~` with the configured home directory when present.
#[derive(Clone, Debug, Default)]
pub struct DefaultAuthContext {
    home_dir: Option<PathBuf>,
}

impl DefaultAuthContext {
    /// Create a context that expands `~` with `home_dir` when provided.
    #[must_use]
    pub fn new(home_dir: Option<PathBuf>) -> Self {
        Self { home_dir }
    }

    fn expand_path(&self, path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix('~')
            && let Some(home) = &self.home_dir
        {
            return home.join(rest.trim_start_matches('/').trim_start_matches('\\'));
        }
        PathBuf::from(path)
    }
}

impl AuthContext for DefaultAuthContext {
    fn env<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            match std::env::var(name) {
                Ok(value) if !value.trim().is_empty() => Some(value),
                _ => None,
            }
        })
    }

    fn file_exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let resolved = self.expand_path(path);
            Path::new(&resolved).exists()
        })
    }
}

/// Auth context that overlays explicit provider env values on a base context.
#[derive(Clone)]
pub struct OverlayEnvAuthContext {
    base: Arc<dyn AuthContext>,
    env: ProviderEnv,
}

impl OverlayEnvAuthContext {
    /// Overlay `env` values on top of `base`.
    ///
    /// Non-empty overlay values win. Empty overlay values fall through to the
    /// base context, matching the reference `env[name] || base.env(name)` rule.
    #[must_use]
    pub fn new(base: Arc<dyn AuthContext>, env: ProviderEnv) -> Self {
        Self { base, env }
    }
}

impl AuthContext for OverlayEnvAuthContext {
    fn env<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if let Some(value) = self.env.get(name)
                && !value.is_empty()
            {
                return Some(value.clone());
            }
            self.base.env(name).await
        })
    }

    fn file_exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, bool> {
        self.base.file_exists(path)
    }
}

/// Build an env-overlaid auth context.
#[must_use]
pub fn overlay_env_auth_context(
    base: Arc<dyn AuthContext>,
    env: ProviderEnv,
) -> OverlayEnvAuthContext {
    OverlayEnvAuthContext::new(base, env)
}

/// In-memory auth context for tests and pure resolution helpers.
#[derive(Clone, Debug, Default)]
pub struct MapAuthContext {
    env: BTreeMap<String, String>,
    existing_files: BTreeMap<String, bool>,
    home_dir: Option<PathBuf>,
}

impl MapAuthContext {
    /// Create an empty map-backed context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set an environment value.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Record whether a path exists (after `~` expansion when configured).
    #[must_use]
    pub fn with_file(mut self, path: impl Into<String>, exists: bool) -> Self {
        self.existing_files.insert(path.into(), exists);
        self
    }

    /// Configure home-directory expansion for leading `~`.
    #[must_use]
    pub fn with_home_dir(mut self, home_dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(home_dir.into());
        self
    }

    fn expand_path(&self, path: &str) -> String {
        if let Some(rest) = path.strip_prefix('~')
            && let Some(home) = &self.home_dir
        {
            return home
                .join(rest.trim_start_matches('/').trim_start_matches('\\'))
                .to_string_lossy()
                .into_owned();
        }
        path.to_owned()
    }
}

impl AuthContext for MapAuthContext {
    fn env<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            self.env
                .get(name)
                .filter(|value| !value.trim().is_empty())
                .cloned()
        })
    }

    fn file_exists<'a>(&'a self, path: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let resolved = self.expand_path(path);
            self.existing_files.get(&resolved).copied().unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn env_overlay_precedence_prefers_non_empty_overlay() {
        let base = Arc::new(
            MapAuthContext::new()
                .with_env("TOKEN", "base-token")
                .with_env("ONLY_BASE", "base-only"),
        );
        let overlay = overlay_env_auth_context(
            base,
            BTreeMap::from([
                ("TOKEN".into(), "overlay-token".into()),
                ("EMPTY".into(), String::new()),
                ("ONLY_OVERLAY".into(), "overlay-only".into()),
            ]),
        );

        assert_eq!(overlay.env("TOKEN").await.as_deref(), Some("overlay-token"));
        assert_eq!(overlay.env("ONLY_BASE").await.as_deref(), Some("base-only"));
        assert_eq!(
            overlay.env("ONLY_OVERLAY").await.as_deref(),
            Some("overlay-only")
        );
        // Empty overlay values fall through.
        assert_eq!(overlay.env("EMPTY").await, None);
    }

    #[tokio::test]
    async fn map_auth_context_expands_home_for_file_exists() {
        let ctx = MapAuthContext::new()
            .with_home_dir("/home/user")
            .with_file("/home/user/.config/gcloud/adc.json", true);

        assert!(ctx.file_exists("~/.config/gcloud/adc.json").await);
        assert!(!ctx.file_exists("~/.missing").await);
    }
}
