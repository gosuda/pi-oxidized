//! Source provenance for discovered product resources.
//!
//! Port of `.references/pi-2.0/packages/coding-agent/src/core/source-info.ts`.

use super::discovery::PathMetadata;

/// Where a resource sits relative to the project boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceScope {
    /// Global agent directory (`~/.pi/agent` or `PI_CODING_AGENT_DIR`).
    User,
    /// Project-local (typically under `{cwd}/.pi`).
    Project,
    /// Temporary/CLI or synthetic paths.
    Temporary,
}

impl SourceScope {
    /// Wire discriminant used by diagnostics and source info.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Temporary => "temporary",
        }
    }
}

/// Whether a path came from a package or top-level settings/auto discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceOrigin {
    /// Installed or local package root.
    Package,
    /// Settings array, auto-discovery, or CLI temporary path.
    TopLevel,
}

impl SourceOrigin {
    /// Wire discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::TopLevel => "top-level",
        }
    }
}

/// Provenance attached to a loaded skill, prompt, theme, or extension path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceInfo {
    /// Absolute (or synthetic) path of the resource.
    pub path: String,
    /// Source label (`local`, `auto`, `cli`, package id, …).
    pub source: String,
    /// Scope relative to the project boundary.
    pub scope: SourceScope,
    /// Package vs top-level origin.
    pub origin: SourceOrigin,
    /// Optional base directory used for relative resolution.
    pub base_dir: Option<String>,
}

/// Build [`SourceInfo`] from path metadata produced by discovery.
#[must_use]
pub fn create_source_info(path: impl Into<String>, metadata: &PathMetadata) -> SourceInfo {
    SourceInfo {
        path: path.into(),
        source: metadata.source.clone(),
        scope: metadata.scope,
        origin: metadata.origin,
        base_dir: metadata.base_dir.clone(),
    }
}

/// Options for synthetic source info when no discovery metadata exists.
#[derive(Clone, Debug)]
pub struct SyntheticSourceInfoOptions {
    /// Source label.
    pub source: String,
    /// Scope (defaults to temporary).
    pub scope: Option<SourceScope>,
    /// Origin (defaults to top-level).
    pub origin: Option<SourceOrigin>,
    /// Optional base directory.
    pub base_dir: Option<String>,
}

/// Build source info with temporary/top-level defaults.
#[must_use]
pub fn create_synthetic_source_info(
    path: impl Into<String>,
    options: SyntheticSourceInfoOptions,
) -> SourceInfo {
    SourceInfo {
        path: path.into(),
        source: options.source,
        scope: options.scope.unwrap_or(SourceScope::Temporary),
        origin: options.origin.unwrap_or(SourceOrigin::TopLevel),
        base_dir: options.base_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::resources::discovery::PathMetadata;

    #[test]
    fn create_source_info_copies_metadata() {
        let metadata = PathMetadata {
            source: "auto".into(),
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some("/tmp/.pi".into()),
        };
        let info = create_source_info("/tmp/.pi/skills/a.md", &metadata);
        assert_eq!(info.path, "/tmp/.pi/skills/a.md");
        assert_eq!(info.source, "auto");
        assert_eq!(info.scope, SourceScope::Project);
        assert_eq!(info.origin, SourceOrigin::TopLevel);
        assert_eq!(info.base_dir.as_deref(), Some("/tmp/.pi"));
    }

    #[test]
    fn synthetic_defaults_temporary_top_level() {
        let info = create_synthetic_source_info(
            "/x",
            SyntheticSourceInfoOptions {
                source: "local".into(),
                scope: None,
                origin: None,
                base_dir: None,
            },
        );
        assert_eq!(info.scope, SourceScope::Temporary);
        assert_eq!(info.origin, SourceOrigin::TopLevel);
        assert_eq!(info.source, "local");
    }
}
