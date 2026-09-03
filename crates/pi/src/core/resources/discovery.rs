//! Package path resolution and resource discovery (resolve-side only).
//!
//! Port of the resolve/discovery surface from
//! `.references/pi-2.0/packages/coding-agent/src/core/package-manager.ts`.
//! Network install/update is intentionally out of scope for Phase 3.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use globset::{Glob, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::core::config::{
    CONFIG_DIR_NAME, PathInputOptions, canonicalize_path, is_local_path, resolve_path_with,
};
use crate::core::resources::source_info::{SourceOrigin, SourceScope};
use crate::core::settings::{PackageSource, PackageSourceFilter, Settings, SettingsManager};

/// Path metadata attached to a resolved resource entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathMetadata {
    /// Source label (`local`, `auto`, package id, `cli`, …).
    pub source: String,
    /// Scope relative to the project boundary.
    pub scope: SourceScope,
    /// Package vs top-level origin.
    pub origin: SourceOrigin,
    /// Optional base directory used for relative resolution.
    pub base_dir: Option<String>,
}

/// One discovered resource path with enablement and provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedResource {
    /// Absolute (or resolved) resource path.
    pub path: String,
    /// Whether the resource is enabled after pattern filters.
    pub enabled: bool,
    /// Discovery provenance.
    pub metadata: PathMetadata,
}

/// Fully resolved resource path sets for all resource kinds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolvedPaths {
    /// Extension paths (`.ts` / `.js` or package roots).
    pub extensions: Vec<ResolvedResource>,
    /// Skill paths (markdown / skill directories).
    pub skills: Vec<ResolvedResource>,
    /// Prompt template markdown paths.
    pub prompts: Vec<ResolvedResource>,
    /// Theme JSON paths.
    pub themes: Vec<ResolvedResource>,
}

/// Resource kind used during package discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ResourceKind {
    /// TypeScript/JavaScript extension.
    Extensions,
    /// Skill markdown.
    Skills,
    /// Prompt template markdown.
    Prompts,
    /// Theme JSON.
    Themes,
}

impl ResourceKind {
    /// Wire directory / settings key name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    fn all() -> [Self; 4] {
        [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes]
    }
}

/// Skill discovery mode for `collect_skill_entries`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillDiscoveryMode {
    /// Pi mode: root-level `.md` files are also skills.
    Pi,
    /// Agents mode: only `SKILL.md` skill roots.
    Agents,
}

/// Errors raised while resolving package paths.
#[derive(Debug, Error)]
pub enum PackageResolveError {
    /// Project-scoped package storage was requested while the project is untrusted.
    #[error("Project is not trusted; refusing to access project package storage")]
    ProjectNotTrusted,
    /// Managed package path escaped its install root.
    #[error("Refusing to use path outside package install root: {0}")]
    PathEscape(String),
}

/// Convert a path to an owned string (lossy).
#[must_use]
pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Numeric precedence rank: lower rank = higher precedence (0..4).
///
/// - `0` project + local
/// - `1` project + auto/other
/// - `2` user + local
/// - `3` user + auto/other
/// - `4` package origin
#[must_use]
pub fn resource_precedence_rank(metadata: &PathMetadata) -> u8 {
    if metadata.origin == SourceOrigin::Package {
        return 4;
    }
    let scope_base: u8 = if metadata.scope == SourceScope::Project {
        0
    } else {
        2
    };
    let source_bump = u8::from(metadata.source != "local");
    scope_base + source_bump
}

/// Resolve-side package path resolver (no network install).
pub struct PackagePathResolver<'a> {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings_manager: &'a SettingsManager,
    /// Cached `npm root -g` / bun global root, keyed by npmCommand argv.
    global_npm_root: Mutex<Option<(String, String)>>,
}

impl<'a> PackagePathResolver<'a> {
    /// Create a resolver rooted at `cwd` / `agent_dir` with live settings.
    #[must_use]
    pub fn new(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        settings_manager: &'a SettingsManager,
    ) -> Self {
        Self {
            cwd: resolve_path_with(
                &path_to_string(cwd.as_ref()),
                Path::new("."),
                PathInputOptions::new().trim(true),
            ),
            agent_dir: resolve_path_with(
                &path_to_string(agent_dir.as_ref()),
                Path::new("."),
                PathInputOptions::new().trim(true),
            ),
            settings_manager,
            global_npm_root: Mutex::new(None),
        }
    }

    /// Resolve all configured packages, local settings paths, and auto-discovery.
    ///
    /// Missing npm/git installs are skipped (no network install in Phase 3).
    ///
    /// # Errors
    ///
    /// Returns [`PackageResolveError::ProjectNotTrusted`] when project package
    /// storage is accessed while the project is untrusted.
    pub fn resolve(&self) -> Result<ResolvedPaths, PackageResolveError> {
        let mut accumulator = ResourceAccumulator::new();
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();

        let mut all_packages = Vec::new();
        for pkg in project_settings.packages.clone().unwrap_or_default() {
            all_packages.push(ScopedPackage {
                pkg,
                scope: SourceScope::Project,
            });
        }
        for pkg in global_settings.packages.clone().unwrap_or_default() {
            all_packages.push(ScopedPackage {
                pkg,
                scope: SourceScope::User,
            });
        }
        let package_sources = self.dedupe_packages(all_packages)?;
        self.resolve_package_sources(&package_sources, &mut accumulator)?;

        let global_base_dir = self.agent_dir.clone();
        let project_base_dir = self.cwd.join(CONFIG_DIR_NAME);

        for kind in ResourceKind::all() {
            let project_entries = settings_resource_paths(&project_settings, kind);
            let global_entries = settings_resource_paths(&global_settings, kind);
            Self::resolve_local_entries(
                &project_entries,
                kind,
                accumulator.target_mut(kind),
                &PathMetadata {
                    source: "local".into(),
                    scope: SourceScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                },
                &project_base_dir,
            );
            Self::resolve_local_entries(
                &global_entries,
                kind,
                accumulator.target_mut(kind),
                &PathMetadata {
                    source: "local".into(),
                    scope: SourceScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                },
                &global_base_dir,
            );
        }

        self.add_auto_discovered_resources(
            &mut accumulator,
            &global_settings,
            &project_settings,
            &global_base_dir,
            &project_base_dir,
        );

        Ok(accumulator.into_resolved_paths())
    }

    /// Resolve temporary/CLI extension package sources.
    ///
    /// # Errors
    ///
    /// Returns [`PackageResolveError`] when project package storage is accessed
    /// while untrusted (should not happen for temporary scope).
    pub fn resolve_extension_sources(
        &self,
        sources: &[String],
        temporary: bool,
        local: bool,
    ) -> Result<ResolvedPaths, PackageResolveError> {
        let mut accumulator = ResourceAccumulator::new();
        let scope = if temporary {
            SourceScope::Temporary
        } else if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let package_sources: Vec<ScopedPackage> = sources
            .iter()
            .map(|source| ScopedPackage {
                pkg: PackageSource::Source(source.clone()),
                scope,
            })
            .collect();
        self.resolve_package_sources(&package_sources, &mut accumulator)?;
        Ok(accumulator.into_resolved_paths())
    }

    fn resolve_package_sources(
        &self,
        sources: &[ScopedPackage],
        accumulator: &mut ResourceAccumulator,
    ) -> Result<(), PackageResolveError> {
        for entry in sources {
            let source_str = package_source_string(&entry.pkg);
            let filter = package_source_filter(&entry.pkg);
            let delta_base = self.find_autoload_delta_base(&entry.pkg, entry.scope, sources)?;
            let resolved_source = delta_base
                .as_ref()
                .map_or(source_str.as_str(), |base| base.source.as_str());
            let resolved_scope = delta_base.as_ref().map_or(entry.scope, |base| base.scope);
            let parsed = parse_package_source(resolved_source);
            let mut metadata = PathMetadata {
                source: source_str,
                scope: entry.scope,
                origin: SourceOrigin::Package,
                base_dir: None,
            };

            match parsed {
                ParsedSource::Local { path } => {
                    let base_dir = self.base_dir_for_scope(resolved_scope)?;
                    Self::resolve_local_extension_source(
                        &path,
                        accumulator,
                        filter.as_ref(),
                        &mut metadata,
                        &base_dir,
                    );
                }
                ParsedSource::Npm { name, .. } => {
                    let installed = match self.npm_install_path(&name, resolved_scope) {
                        Ok(path) => path,
                        Err(PackageResolveError::ProjectNotTrusted) => {
                            return Err(PackageResolveError::ProjectNotTrusted);
                        }
                        Err(other) => return Err(other),
                    };
                    if !installed.exists() {
                        continue;
                    }
                    metadata.base_dir = Some(path_to_string(&installed));
                    let _ = Self::collect_package_resources(
                        &installed,
                        accumulator,
                        filter.as_ref(),
                        &metadata,
                    );
                }
                ParsedSource::Git { host, path, .. } => {
                    let installed = self.git_install_path(&host, &path, resolved_scope)?;
                    if !installed.exists() {
                        continue;
                    }
                    metadata.base_dir = Some(path_to_string(&installed));
                    let _ = Self::collect_package_resources(
                        &installed,
                        accumulator,
                        filter.as_ref(),
                        &metadata,
                    );
                }
            }
        }
        Ok(())
    }

    fn find_autoload_delta_base(
        &self,
        pkg: &PackageSource,
        scope: SourceScope,
        sources: &[ScopedPackage],
    ) -> Result<Option<DeltaBase>, PackageResolveError> {
        let PackageSource::Filtered(filter) = pkg else {
            return Ok(None);
        };
        if scope != SourceScope::Project || filter.autoload != Some(false) {
            return Ok(None);
        }
        let identity = package_identity(
            &filter.source,
            Some(scope),
            &self.cwd,
            &self.agent_dir,
            self.settings_manager.is_project_trusted(),
        )?;
        for entry in sources {
            if entry.scope != SourceScope::User {
                continue;
            }
            let source = package_source_string(&entry.pkg);
            let entry_identity = package_identity(
                &source,
                Some(SourceScope::User),
                &self.cwd,
                &self.agent_dir,
                self.settings_manager.is_project_trusted(),
            )?;
            if entry_identity == identity {
                return Ok(Some(DeltaBase {
                    source,
                    scope: SourceScope::User,
                }));
            }
        }
        Ok(None)
    }

    fn resolve_local_extension_source(
        source_path: &str,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageSourceFilter>,
        metadata: &mut PathMetadata,
        base_dir: &Path,
    ) {
        let resolved = resolve_path_from_base(source_path, base_dir);
        if !resolved.exists() {
            return;
        }
        let Ok(stats) = fs::metadata(&resolved) else {
            return;
        };
        if stats.is_file() {
            metadata.base_dir = resolved.parent().map(path_to_string);
            accumulator.add(
                ResourceKind::Extensions,
                path_to_string(&resolved),
                metadata.clone(),
                true,
            );
            return;
        }
        if stats.is_dir() {
            metadata.base_dir = Some(path_to_string(&resolved));
            let has_resources =
                Self::collect_package_resources(&resolved, accumulator, filter, metadata);
            if !has_resources {
                accumulator.add(
                    ResourceKind::Extensions,
                    path_to_string(&resolved),
                    metadata.clone(),
                    true,
                );
            }
        }
    }

    fn collect_package_resources(
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageSourceFilter>,
        metadata: &PathMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for kind in ResourceKind::all() {
                let patterns = filter_patterns(filter, kind);
                let target = accumulator.target_mut(kind);
                if filter.autoload == Some(false) {
                    apply_package_delta_filter(
                        package_root,
                        patterns.as_deref().unwrap_or(&[]),
                        kind,
                        target,
                        metadata,
                    );
                } else if let Some(patterns) = patterns {
                    apply_package_filter(package_root, &patterns, kind, target, metadata);
                } else {
                    collect_default_resources(package_root, kind, target, metadata);
                }
            }
            return true;
        }

        if let Some(manifest) = read_pi_manifest(package_root) {
            for kind in ResourceKind::all() {
                let entries = manifest_entries(&manifest, kind);
                add_manifest_entries(
                    entries.as_deref(),
                    package_root,
                    kind,
                    accumulator.target_mut(kind),
                    metadata,
                );
            }
            return true;
        }

        let mut has_any_dir = false;
        for kind in ResourceKind::all() {
            let dir = package_root.join(kind.as_str());
            if dir.exists() {
                let files = collect_resource_files(&dir, kind);
                for file in files {
                    accumulator.add(kind, file, metadata.clone(), true);
                }
                has_any_dir = true;
            }
        }
        has_any_dir
    }

    fn resolve_local_entries(
        entries: &[String],
        kind: ResourceKind,
        target: &mut HashMap<String, AccumEntry>,
        metadata: &PathMetadata,
        base_dir: &Path,
    ) {
        if entries.is_empty() {
            return;
        }
        let (plain, patterns) = split_patterns(entries);
        let resolved_plain: Vec<PathBuf> = plain
            .iter()
            .map(|entry| resolve_path_from_base(entry, base_dir))
            .collect();
        let all_files = collect_files_from_paths(&resolved_plain, kind);
        let all_path_strings: Vec<String> = all_files.iter().map(|p| path_to_string(p)).collect();
        let enabled = apply_patterns(&all_path_strings, &patterns, base_dir);
        for file in all_path_strings {
            let is_enabled = enabled.contains(&file);
            add_resource(target, file, metadata.clone(), is_enabled);
        }
    }

    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        global_settings: &Settings,
        project_settings: &Settings,
        global_base_dir: &Path,
        project_base_dir: &Path,
    ) {
        let user_metadata = PathMetadata {
            source: "auto".into(),
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(path_to_string(global_base_dir)),
        };
        let project_metadata = PathMetadata {
            source: "auto".into(),
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(path_to_string(project_base_dir)),
        };
        let user_overrides = ResourceOverrides {
            extensions: settings_resource_paths(global_settings, ResourceKind::Extensions),
            skills: settings_resource_paths(global_settings, ResourceKind::Skills),
            prompts: settings_resource_paths(global_settings, ResourceKind::Prompts),
            themes: settings_resource_paths(global_settings, ResourceKind::Themes),
        };
        let project_overrides = ResourceOverrides {
            extensions: settings_resource_paths(project_settings, ResourceKind::Extensions),
            skills: settings_resource_paths(project_settings, ResourceKind::Skills),
            prompts: settings_resource_paths(project_settings, ResourceKind::Prompts),
            themes: settings_resource_paths(project_settings, ResourceKind::Themes),
        };
        let user_dirs = ResourceDirs {
            extensions: global_base_dir.join("extensions"),
            skills: global_base_dir.join("skills"),
            prompts: global_base_dir.join("prompts"),
            themes: global_base_dir.join("themes"),
        };
        let project_dirs = ResourceDirs {
            extensions: project_base_dir.join("extensions"),
            skills: project_base_dir.join("skills"),
            prompts: project_base_dir.join("prompts"),
            themes: project_base_dir.join("themes"),
        };

        let user_agents_skills_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".agents")
            .join("skills");
        if self.settings_manager.is_project_trusted() {
            self.add_project_auto_resources(
                accumulator,
                &project_metadata,
                &project_overrides,
                &project_dirs,
                project_base_dir,
                &user_agents_skills_dir,
            );
        }
        Self::add_user_auto_resources(
            accumulator,
            &user_metadata,
            &user_overrides,
            &user_dirs,
            global_base_dir,
            &user_agents_skills_dir,
        );
    }

    fn add_project_auto_resources(
        &self,
        accumulator: &mut ResourceAccumulator,
        metadata: &PathMetadata,
        overrides: &ResourceOverrides,
        dirs: &ResourceDirs,
        base_dir: &Path,
        user_agents_skills_dir: &Path,
    ) {
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Extensions,
            collect_auto_extension_entries(&dirs.extensions),
            metadata,
            &overrides.extensions,
            base_dir,
        );
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Skills,
            collect_skill_entries(&dirs.skills, SkillDiscoveryMode::Pi, None, None),
            metadata,
            &overrides.skills,
            base_dir,
        );
        for agents_skills_dir in collect_ancestor_agents_skill_dirs(&self.cwd)
            .into_iter()
            .filter(|dir| canonicalize_path(dir) != canonicalize_path(user_agents_skills_dir))
        {
            let agents_base_dir = agents_skills_dir
                .parent()
                .map_or_else(|| agents_skills_dir.clone(), Path::to_path_buf);
            let mut agents_metadata = metadata.clone();
            agents_metadata.base_dir = Some(path_to_string(&agents_base_dir));
            Self::add_discovered_entries(
                accumulator,
                ResourceKind::Skills,
                collect_skill_entries(&agents_skills_dir, SkillDiscoveryMode::Agents, None, None),
                &agents_metadata,
                &overrides.skills,
                &agents_base_dir,
            );
        }
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Prompts,
            collect_auto_prompt_entries(&dirs.prompts),
            metadata,
            &overrides.prompts,
            base_dir,
        );
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Themes,
            collect_auto_theme_entries(&dirs.themes),
            metadata,
            &overrides.themes,
            base_dir,
        );
    }

    fn add_user_auto_resources(
        accumulator: &mut ResourceAccumulator,
        metadata: &PathMetadata,
        overrides: &ResourceOverrides,
        dirs: &ResourceDirs,
        base_dir: &Path,
        user_agents_skills_dir: &Path,
    ) {
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Extensions,
            collect_auto_extension_entries(&dirs.extensions),
            metadata,
            &overrides.extensions,
            base_dir,
        );
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Skills,
            collect_skill_entries(&dirs.skills, SkillDiscoveryMode::Pi, None, None),
            metadata,
            &overrides.skills,
            base_dir,
        );
        let agents_base_dir = user_agents_skills_dir
            .parent()
            .map_or_else(|| user_agents_skills_dir.to_path_buf(), Path::to_path_buf);
        let mut agents_metadata = metadata.clone();
        agents_metadata.base_dir = Some(path_to_string(&agents_base_dir));
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Skills,
            collect_skill_entries(
                user_agents_skills_dir,
                SkillDiscoveryMode::Agents,
                None,
                None,
            ),
            &agents_metadata,
            &overrides.skills,
            &agents_base_dir,
        );
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Prompts,
            collect_auto_prompt_entries(&dirs.prompts),
            metadata,
            &overrides.prompts,
            base_dir,
        );
        Self::add_discovered_entries(
            accumulator,
            ResourceKind::Themes,
            collect_auto_theme_entries(&dirs.themes),
            metadata,
            &overrides.themes,
            base_dir,
        );
    }

    fn add_discovered_entries(
        accumulator: &mut ResourceAccumulator,
        kind: ResourceKind,
        paths: Vec<String>,
        metadata: &PathMetadata,
        overrides: &[String],
        base_dir: &Path,
    ) {
        for path in paths {
            let enabled = is_enabled_by_overrides(&path, overrides, base_dir);
            accumulator.add(kind, path, metadata.clone(), enabled);
        }
    }

    fn dedupe_packages(
        &self,
        packages: Vec<ScopedPackage>,
    ) -> Result<Vec<ScopedPackage>, PackageResolveError> {
        let mut result: Vec<ScopedPackage> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for entry in packages {
            let source = package_source_string(&entry.pkg);
            let identity = package_identity(
                &source,
                Some(entry.scope),
                &self.cwd,
                &self.agent_dir,
                self.settings_manager.is_project_trusted(),
            )?;
            if let Some(&index) = seen.get(&identity) {
                let existing_scope = result[index].scope;
                let existing_is_delta = matches!(
                    &result[index].pkg,
                    PackageSource::Filtered(filter) if filter.autoload == Some(false)
                );
                if existing_scope == SourceScope::Project && entry.scope == SourceScope::User {
                    if existing_is_delta {
                        result.push(entry);
                    }
                } else if entry.scope == SourceScope::Project {
                    result[index] = entry;
                }
            } else {
                seen.insert(identity, result.len());
                result.push(entry);
            }
        }
        Ok(result)
    }

    fn base_dir_for_scope(&self, scope: SourceScope) -> Result<PathBuf, PackageResolveError> {
        match scope {
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self.cwd.join(CONFIG_DIR_NAME))
            }
            SourceScope::User => Ok(self.agent_dir.clone()),
            SourceScope::Temporary => Ok(self.cwd.clone()),
        }
    }

    fn assert_project_trusted_for_scope(
        &self,
        scope: SourceScope,
    ) -> Result<(), PackageResolveError> {
        if scope == SourceScope::Project && !self.settings_manager.is_project_trusted() {
            return Err(PackageResolveError::ProjectNotTrusted);
        }
        Ok(())
    }

    fn npm_install_path(
        &self,
        name: &str,
        scope: SourceScope,
    ) -> Result<PathBuf, PackageResolveError> {
        let managed = self.managed_npm_install_path(name, scope)?;
        if scope != SourceScope::User || managed.exists() {
            return Ok(managed);
        }
        // TS getLegacyGlobalNpmInstallPath: pnpm global path ?? npm root -g / name
        if let Some(legacy) = self.legacy_global_npm_install_path(name)
            && legacy.exists()
        {
            return Ok(legacy);
        }
        Ok(managed)
    }

    fn managed_npm_install_path(
        &self,
        name: &str,
        scope: SourceScope,
    ) -> Result<PathBuf, PackageResolveError> {
        match scope {
            SourceScope::Temporary => {
                // join(getTemporaryDir("npm"), "node_modules", name)
                let temp = self.temporary_dir("npm", None)?;
                Ok(temp.join("node_modules").join(name))
            }
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(self
                    .cwd
                    .join(CONFIG_DIR_NAME)
                    .join("npm")
                    .join("node_modules")
                    .join(name))
            }
            SourceScope::User => Ok(self.agent_dir.join("npm").join("node_modules").join(name)),
        }
    }

    fn git_install_path(
        &self,
        host: &str,
        path: &str,
        scope: SourceScope,
    ) -> Result<PathBuf, PackageResolveError> {
        if scope == SourceScope::Temporary {
            // getTemporaryDir(`git-${host}`, path)
            return self.temporary_dir(&format!("git-{host}"), Some(path));
        }
        let install_root = match scope {
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                self.cwd.join(CONFIG_DIR_NAME).join("git")
            }
            SourceScope::User => self.agent_dir.join("git"),
            SourceScope::Temporary => {
                return self.temporary_dir(&format!("git-{host}"), Some(path));
            }
        };
        resolve_managed_path(&install_root, &[host, path])
    }

    /// Exact TS `getTemporaryDir(prefix, suffix?)`.
    fn temporary_dir(
        &self,
        prefix: &str,
        suffix: Option<&str>,
    ) -> Result<PathBuf, PackageResolveError> {
        let root = resolve_managed_path(&extension_temp_folder(&self.agent_dir), &[prefix])?;
        let hash = temporary_dir_hash(prefix, suffix.unwrap_or(""));
        match suffix {
            Some(sfx) if !sfx.is_empty() => resolve_managed_path(&root, &[&hash, sfx]),
            _ => resolve_managed_path(&root, &[&hash]),
        }
    }

    fn npm_command(&self) -> (String, Vec<String>) {
        match self.settings_manager.get_npm_command() {
            Some(parts) if !parts.is_empty() => {
                let mut iter = parts.into_iter();
                let command = iter.next().unwrap_or_else(|| "npm".to_owned());
                (command, iter.collect())
            }
            _ => ("npm".to_owned(), Vec::new()),
        }
    }

    fn package_manager_name(&self) -> String {
        let (command, args) = self.npm_command();
        let mut parts = vec![command];
        parts.extend(args);
        let separator = parts.iter().rposition(|p| p == "--");
        let pm = match separator {
            Some(i) => parts.get(i + 1).cloned().unwrap_or_default(),
            None => parts.first().cloned().unwrap_or_default(),
        };
        Path::new(&pm)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(pm)
            .trim_end_matches(".cmd")
            .trim_end_matches(".exe")
            .to_owned()
    }

    fn run_npm_command_sync(&self, extra_args: &[&str]) -> Result<String, PackageResolveError> {
        let (command, mut args) = self.npm_command();
        for a in extra_args {
            args.push((*a).to_owned());
        }
        let output = Command::new(&command)
            .args(&args)
            .output()
            .map_err(|e| PackageResolveError::PathEscape(format!("npm command failed: {e}")))?;
        if !output.status.success() {
            return Err(PackageResolveError::PathEscape(format!(
                "npm command exited {}",
                output.status
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn global_npm_root(&self) -> Result<String, PackageResolveError> {
        let (command, args) = self.npm_command();
        let mut key_parts = vec![command];
        key_parts.extend(args);
        let command_key = key_parts.join("\0");
        if let Ok(guard) = self.global_npm_root.lock()
            && let Some((cached_key, root)) = guard.as_ref()
            && cached_key == &command_key
        {
            return Ok(root.clone());
        }
        let root = if self.package_manager_name() == "bun" {
            let bin_dir = self.run_npm_command_sync(&["pm", "bin", "-g"])?;
            let parent = Path::new(&bin_dir)
                .parent()
                .map_or_else(|| PathBuf::from(&bin_dir), Path::to_path_buf);
            path_to_string(&parent.join("install").join("global").join("node_modules"))
        } else {
            self.run_npm_command_sync(&["root", "-g"])?
        };
        if let Ok(mut guard) = self.global_npm_root.lock() {
            *guard = Some((command_key, root.clone()));
        }
        Ok(root)
    }

    fn pnpm_global_package_path(&self, package_name: &str) -> Option<PathBuf> {
        if self.package_manager_name() != "pnpm" {
            return None;
        }
        let output = self
            .run_npm_command_sync(&["list", "-g", "--depth", "0", "--json"])
            .ok()?;
        let entries: Value = serde_json::from_str(&output).ok()?;
        let arr = entries.as_array()?;
        for entry in arr {
            if let Some(deps) = entry.get("dependencies").and_then(Value::as_object)
                && let Some(path) = deps
                    .get(package_name)
                    .and_then(|d| d.get("path"))
                    .and_then(Value::as_str)
            {
                return Some(PathBuf::from(path));
            }
        }
        None
    }

    fn legacy_global_npm_install_path(&self, name: &str) -> Option<PathBuf> {
        // try { pnpm path ?? join(globalNpmRoot, name) } catch { undefined }
        match self.pnpm_global_package_path(name) {
            Some(path) => Some(path),
            None => match self.global_npm_root() {
                Ok(root) if !root.is_empty() => Some(Path::new(&root).join(name)),
                _ => None,
            },
        }
    }
}

/// Parse a package source string into npm / git / local.
#[must_use]
pub fn parse_package_source(source: &str) -> ParsedSource {
    let trimmed = source.trim();
    if let Some(spec) = trimmed.strip_prefix("npm:") {
        let spec = spec.trim();
        let (name, version) = parse_npm_spec(spec);
        return ParsedSource::Npm {
            spec: spec.to_owned(),
            name,
            version,
        };
    }
    if is_local_path(trimmed) {
        return ParsedSource::Local {
            path: trimmed.to_owned(),
        };
    }
    if let Some(git) = parse_git_url(trimmed) {
        return git;
    }
    ParsedSource::Local {
        path: trimmed.to_owned(),
    }
}

/// Stable package identity for dedupe (`npm:name`, `git:host/path`, `local:abs`).
///
/// # Errors
///
/// Returns [`PackageResolveError::ProjectNotTrusted`] when a project-scoped
/// local identity needs project package storage while untrusted.
pub fn package_identity(
    source: &str,
    scope: Option<SourceScope>,
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
) -> Result<String, PackageResolveError> {
    match parse_package_source(source) {
        ParsedSource::Npm { name, .. } => Ok(format!("npm:{name}")),
        ParsedSource::Git { host, path, .. } => Ok(format!("git:{host}/{path}")),
        ParsedSource::Local { path } => {
            let base = match scope {
                Some(SourceScope::Project) => {
                    if !project_trusted {
                        return Err(PackageResolveError::ProjectNotTrusted);
                    }
                    cwd.join(CONFIG_DIR_NAME)
                }
                Some(SourceScope::User) => agent_dir.to_path_buf(),
                Some(SourceScope::Temporary) | None => cwd.to_path_buf(),
            };
            let resolved = resolve_path_from_base(&path, &base);
            Ok(format!("local:{}", path_to_string(&resolved)))
        }
    }
}

/// Walk ancestors looking for a `.git` directory.
#[must_use]
pub fn find_git_repo_root(start_dir: impl AsRef<Path>) -> Option<PathBuf> {
    let mut dir = resolve_path_with(
        &path_to_string(start_dir.as_ref()),
        Path::new("."),
        PathInputOptions::new(),
    );
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        let parent = dir.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// Collect `.agents/skills` dirs from `start` up to the git root (inclusive).
#[must_use]
pub fn collect_ancestor_agents_skill_dirs(start_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut skill_dirs = Vec::new();
    let resolved_start = resolve_path_with(
        &path_to_string(start_dir.as_ref()),
        Path::new("."),
        PathInputOptions::new(),
    );
    let git_repo_root = find_git_repo_root(&resolved_start);
    let mut dir = resolved_start;
    loop {
        skill_dirs.push(dir.join(".agents").join("skills"));
        if git_repo_root.as_ref().is_some_and(|root| root == &dir) {
            break;
        }
        let parent = dir.parent().map(Path::to_path_buf);
        match parent {
            Some(parent) if parent != dir => dir = parent,
            _ => break,
        }
    }
    skill_dirs
}

/// Collect skill entries under `dir`.
///
/// When a directory contains `SKILL.md`, that file is the skill root and
/// recursion stops. In [`SkillDiscoveryMode::Pi`], root-level `.md` files are
/// also collected.
#[must_use]
pub fn collect_skill_entries(
    dir: impl AsRef<Path>,
    mode: SkillDiscoveryMode,
    ignore_matcher: Option<&Gitignore>,
    root_dir: Option<&Path>,
) -> Vec<String> {
    let dir = dir.as_ref();
    let mut entries = Vec::new();
    if !dir.exists() {
        return entries;
    }
    let root = root_dir.unwrap_or(dir);
    let owned_ignore;
    let ig = if let Some(matcher) = ignore_matcher {
        matcher
    } else {
        owned_ignore = build_ignore_matcher(dir, root);
        &owned_ignore
    };
    // Re-add rules for this directory level when using shared matcher.
    let mut local_builder = GitignoreBuilder::new(root);
    // Start from parent matcher patterns by reading ignore files at this level only.
    // `ignore` crate doesn't clone easily; rebuild cumulative via helper.
    let ig = {
        let _ = ig;
        owned_or_extended(ignore_matcher, dir, root, &mut local_builder)
    };

    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    dir_entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in &dir_entries {
        let name = entry.file_name();
        if name != "SKILL.md" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let rel = to_posix_path(&relative_path(root, &full_path));
        if ig.matched(&rel, false).is_ignore() {
            continue;
        }
        entries.push(path_to_string(&full_path));
        return entries;
    }

    for entry in &dir_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        let is_dir = meta.is_dir();
        let is_file = meta.is_file();
        let rel = to_posix_path(&relative_path(root, &full_path));
        if mode == SkillDiscoveryMode::Pi
            && dir == root
            && is_file
            && has_extension(&name_str, "md")
            && !ig.matched(&rel, false).is_ignore()
        {
            entries.push(path_to_string(&full_path));
            continue;
        }
        if !is_dir {
            continue;
        }
        if ig.matched(format!("{rel}/"), true).is_ignore() {
            continue;
        }
        entries.extend(collect_skill_entries(
            &full_path,
            mode,
            Some(&ig),
            Some(root),
        ));
    }
    entries
}

/// Non-recursive auto prompt discovery (`.md` files).
#[must_use]
pub fn collect_auto_prompt_entries(dir: impl AsRef<Path>) -> Vec<String> {
    collect_nonrecursive_entries(dir.as_ref(), |name| has_extension(name, "md"))
}

/// Non-recursive auto theme discovery (`.json` files).
#[must_use]
pub fn collect_auto_theme_entries(dir: impl AsRef<Path>) -> Vec<String> {
    collect_nonrecursive_entries(dir.as_ref(), |name| has_extension(name, "json"))
}

/// Smart auto extension discovery (index files / package.json extensions).
#[must_use]
pub fn collect_auto_extension_entries(dir: impl AsRef<Path>) -> Vec<String> {
    let dir = dir.as_ref();
    let mut entries = Vec::new();
    if !dir.exists() {
        return entries;
    }
    if let Some(root_entries) = resolve_extension_entries(dir) {
        return root_entries;
    }
    let ig = build_ignore_matcher(dir, dir);
    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    dir_entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in dir_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        let is_dir = meta.is_dir();
        let is_file = meta.is_file();
        let rel = to_posix_path(&relative_path(dir, &full_path));
        let ignore_path = if is_dir {
            format!("{rel}/")
        } else {
            rel.clone()
        };
        if ig.matched(&ignore_path, is_dir).is_ignore() {
            continue;
        }
        if is_file && (name_str.ends_with(".ts") || name_str.ends_with(".js")) {
            entries.push(path_to_string(&full_path));
        } else if is_dir && let Some(resolved) = resolve_extension_entries(&full_path) {
            entries.extend(resolved);
        }
    }
    entries
}

/// Apply include/exclude/force patterns and return enabled paths.
#[must_use]
pub fn apply_patterns(
    all_paths: &[String],
    patterns: &[String],
    base_dir: &Path,
) -> HashSet<String> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for pattern in patterns {
        if let Some(rest) = pattern.strip_prefix('+') {
            force_includes.push(rest.to_owned());
        } else if let Some(rest) = pattern.strip_prefix('-') {
            force_excludes.push(rest.to_owned());
        } else if let Some(rest) = pattern.strip_prefix('!') {
            excludes.push(rest.to_owned());
        } else {
            includes.push(pattern.clone());
        }
    }

    let mut result: Vec<String> = if includes.is_empty() {
        all_paths.to_vec()
    } else {
        all_paths
            .iter()
            .filter(|path| matches_any_pattern(path, &includes, base_dir))
            .cloned()
            .collect()
    };
    if !excludes.is_empty() {
        result.retain(|path| !matches_any_pattern(path, &excludes, base_dir));
    }
    if !force_includes.is_empty() {
        for path in all_paths {
            if !result.iter().any(|existing| existing == path)
                && matches_any_exact_pattern(path, &force_includes, base_dir)
            {
                result.push(path.clone());
            }
        }
    }
    if !force_excludes.is_empty() {
        result.retain(|path| !matches_any_exact_pattern(path, &force_excludes, base_dir));
    }
    result.into_iter().collect()
}

/// Whether an auto-discovered path is enabled given override patterns.
#[must_use]
pub fn is_enabled_by_overrides(file_path: &str, patterns: &[String], base_dir: &Path) -> bool {
    let overrides: Vec<&String> = patterns
        .iter()
        .filter(|pattern| {
            pattern.starts_with('!') || pattern.starts_with('+') || pattern.starts_with('-')
        })
        .collect();
    let excludes: Vec<String> = overrides
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('!').map(str::to_owned))
        .collect();
    let force_includes: Vec<String> = overrides
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('+').map(str::to_owned))
        .collect();
    let force_excludes: Vec<String> = overrides
        .iter()
        .filter_map(|pattern| pattern.strip_prefix('-').map(str::to_owned))
        .collect();

    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(file_path, &excludes, base_dir) {
        enabled = false;
    }
    if !force_includes.is_empty() && matches_any_exact_pattern(file_path, &force_includes, base_dir)
    {
        enabled = true;
    }
    if !force_excludes.is_empty() && matches_any_exact_pattern(file_path, &force_excludes, base_dir)
    {
        enabled = false;
    }
    enabled
}

/// Parsed package source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParsedSource {
    /// `npm:name[@version]`.
    Npm {
        /// Full npm spec without the `npm:` prefix.
        spec: String,
        /// Package name (including scope).
        name: String,
        /// Optional version / range.
        version: Option<String>,
    },
    /// Git source.
    Git {
        /// Clone URL.
        repo: String,
        /// Host domain.
        host: String,
        /// Repository path (`user/repo`).
        path: String,
        /// Optional ref.
        ref_name: Option<String>,
        /// Whether a ref was specified.
        pinned: bool,
    },
    /// Local filesystem path.
    Local {
        /// Path string as configured.
        path: String,
    },
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ScopedPackage {
    pkg: PackageSource,
    scope: SourceScope,
}

#[derive(Clone, Debug)]
struct DeltaBase {
    source: String,
    scope: SourceScope,
}

#[derive(Clone, Debug)]
struct AccumEntry {
    metadata: PathMetadata,
    enabled: bool,
}

#[derive(Default)]
struct ResourceAccumulator {
    extensions: HashMap<String, AccumEntry>,
    skills: HashMap<String, AccumEntry>,
    prompts: HashMap<String, AccumEntry>,
    themes: HashMap<String, AccumEntry>,
}

impl ResourceAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn target_mut(&mut self, kind: ResourceKind) -> &mut HashMap<String, AccumEntry> {
        match kind {
            ResourceKind::Extensions => &mut self.extensions,
            ResourceKind::Skills => &mut self.skills,
            ResourceKind::Prompts => &mut self.prompts,
            ResourceKind::Themes => &mut self.themes,
        }
    }

    fn add(&mut self, kind: ResourceKind, path: String, metadata: PathMetadata, enabled: bool) {
        add_resource(self.target_mut(kind), path, metadata, enabled);
    }

    fn into_resolved_paths(self) -> ResolvedPaths {
        ResolvedPaths {
            extensions: map_to_resolved(self.extensions),
            skills: map_to_resolved(self.skills),
            prompts: map_to_resolved(self.prompts),
            themes: map_to_resolved(self.themes),
        }
    }
}

struct ResourceOverrides {
    extensions: Vec<String>,
    skills: Vec<String>,
    prompts: Vec<String>,
    themes: Vec<String>,
}

struct ResourceDirs {
    extensions: PathBuf,
    skills: PathBuf,
    prompts: PathBuf,
    themes: PathBuf,
}

#[derive(Default)]
struct PiManifest {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

fn map_to_resolved(entries: HashMap<String, AccumEntry>) -> Vec<ResolvedResource> {
    let mut resolved: Vec<ResolvedResource> = entries
        .into_iter()
        .map(|(path, entry)| ResolvedResource {
            path,
            enabled: entry.enabled,
            metadata: entry.metadata,
        })
        .collect();
    resolved.sort_by_key(|entry| resource_precedence_rank(&entry.metadata));
    let mut seen = HashSet::new();
    resolved
        .into_iter()
        .filter(|entry| {
            let canonical = path_to_string(&canonicalize_path(&entry.path));
            seen.insert(canonical)
        })
        .collect()
}

fn add_resource(
    map: &mut HashMap<String, AccumEntry>,
    path: String,
    metadata: PathMetadata,
    enabled: bool,
) {
    if path.is_empty() {
        return;
    }
    map.entry(path).or_insert(AccumEntry { metadata, enabled });
}

fn package_source_string(pkg: &PackageSource) -> String {
    match pkg {
        PackageSource::Source(source) => source.clone(),
        PackageSource::Filtered(filter) => filter.source.clone(),
    }
}

fn package_source_filter(pkg: &PackageSource) -> Option<PackageSourceFilter> {
    match pkg {
        PackageSource::Source(_) => None,
        PackageSource::Filtered(filter) => Some(filter.clone()),
    }
}

fn settings_resource_paths(settings: &Settings, kind: ResourceKind) -> Vec<String> {
    match kind {
        ResourceKind::Extensions => settings.extensions.clone().unwrap_or_default(),
        ResourceKind::Skills => settings.skills.clone().unwrap_or_default(),
        ResourceKind::Prompts => settings.prompts.clone().unwrap_or_default(),
        ResourceKind::Themes => settings.themes.clone().unwrap_or_default(),
    }
}

fn filter_patterns(filter: &PackageSourceFilter, kind: ResourceKind) -> Option<Vec<String>> {
    match kind {
        ResourceKind::Extensions => filter.extensions.clone(),
        ResourceKind::Skills => filter.skills.clone(),
        ResourceKind::Prompts => filter.prompts.clone(),
        ResourceKind::Themes => filter.themes.clone(),
    }
}

fn resolve_path_from_base(input: &str, base_dir: &Path) -> PathBuf {
    resolve_path_with(input, base_dir, PathInputOptions::new().trim(true))
}

fn extension_temp_folder(agent_dir: &Path) -> PathBuf {
    agent_dir.join("tmp").join("extensions")
}

/// Exact TypeScript: SHA-256 of `prefix`, a hyphen, and `suffix`; first eight hex characters.
#[must_use]
pub fn temporary_dir_hash(prefix: &str, suffix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{prefix}-{suffix}").as_bytes());
    let digest = hasher.finalize();
    // hex encode first 4 bytes = 8 hex chars
    let mut out = String::with_capacity(8);
    for byte in digest.iter().take(4) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn resolve_managed_path(root: &Path, parts: &[&str]) -> Result<PathBuf, PackageResolveError> {
    let resolved_root = resolve_path_with(
        &path_to_string(root),
        Path::new("."),
        PathInputOptions::new(),
    );
    let mut resolved = resolved_root.clone();
    for part in parts {
        for component in Path::new(part).components() {
            match component {
                Component::Normal(seg) => resolved.push(seg),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(PackageResolveError::PathEscape(path_to_string(&resolved)));
                }
            }
        }
    }
    if resolved != resolved_root && !resolved.starts_with(&resolved_root) {
        return Err(PackageResolveError::PathEscape(path_to_string(&resolved)));
    }
    Ok(resolved)
}

fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    // `@scope/name@version` or `name@version`
    if let Some(at) = spec.rfind('@')
        && at > 0
    {
        let name = &spec[..at];
        let version = &spec[at + 1..];
        if !name.is_empty() && !version.is_empty() {
            if name.starts_with('@') {
                return (name.to_owned(), Some(version.to_owned()));
            }
            if !name.contains('@') {
                return (name.to_owned(), Some(version.to_owned()));
            }
        }
    }
    // scoped without version: @scope/name
    (spec.to_owned(), None)
}

fn parse_git_url(source: &str) -> Option<ParsedSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed["git:".len()..].trim()
    } else {
        trimmed
    };

    // hosted-git shorthand pass for github:owner/repo and gitlab:owner/repo
    // (with optional #committish). Other hosts fall through to the generic
    // parser, matching hostedGitInfo's narrow shorthand support.
    let expanded = expand_hosted_git_shorthand(url);
    let url = expanded.as_deref().unwrap_or(url);

    if !has_git_prefix
        && !url.starts_with("https://")
        && !url.starts_with("http://")
        && !url.starts_with("ssh://")
        && !url.starts_with("git://")
    {
        return None;
    }
    parse_generic_git_url(url)
}

fn expand_hosted_git_shorthand(url: &str) -> Option<String> {
    let (domain, after) = if let Some(after) = url.strip_prefix("github:") {
        ("github.com", after)
    } else {
        let after = url.strip_prefix("gitlab:")?;
        ("gitlab.com", after)
    };
    if after.is_empty() {
        return None;
    }
    // The generic parser splits refs with '@' for URLs, so map the shorthand's
    // '#committish' marker to '@' so split_ref / parse_generic_git_url pick
    // it up and produce a clean clone URL without the fragment.
    let after = after.replacen('#', "@", 1);
    Some(format!("https://{domain}/{after}"))
}

fn parse_generic_git_url(url: &str) -> Option<ParsedSource> {
    let (repo_without_ref, ref_name) = split_ref(url);
    let mut repo = repo_without_ref.clone();
    let (host, path) = if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host.to_owned(), path.to_owned())
    } else if repo_without_ref.starts_with("https://")
        || repo_without_ref.starts_with("http://")
        || repo_without_ref.starts_with("ssh://")
        || repo_without_ref.starts_with("git://")
    {
        let parsed = url::Url::parse(&repo_without_ref).ok()?;
        let host = parsed.host_str()?.to_owned();
        let path = parsed.path().trim_start_matches('/').to_owned();
        (host, path)
    } else {
        let slash = repo_without_ref.find('/')?;
        let host = repo_without_ref[..slash].to_owned();
        let path = repo_without_ref[slash + 1..].to_owned();
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        repo = format!("https://{repo_without_ref}");
        (host, path)
    };
    build_git_source(repo, host, &path, ref_name)
}

fn split_ref(url: &str) -> (String, Option<String>) {
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path_with_ref)) = rest.split_once(':')
            && let Some((repo_path, ref_name)) = path_with_ref.split_once('@')
            && !repo_path.is_empty()
            && !ref_name.is_empty()
        {
            return (format!("git@{host}:{repo_path}"), Some(ref_name.to_owned()));
        }
        return (url.to_owned(), None);
    }
    if url.contains("://") {
        if let Ok(mut parsed) = url::Url::parse(url) {
            let path_with_ref = parsed.path().trim_start_matches('/').to_owned();
            if let Some((repo_path, ref_name)) = path_with_ref.split_once('@')
                && !repo_path.is_empty()
                && !ref_name.is_empty()
            {
                parsed.set_path(&format!("/{repo_path}"));
                let mut repo = parsed.to_string();
                if repo.ends_with('/') {
                    repo.pop();
                }
                return (repo, Some(ref_name.to_owned()));
            }
        }
        return (url.to_owned(), None);
    }
    let Some(slash) = url.find('/') else {
        return (url.to_owned(), None);
    };
    let host = &url[..slash];
    let path_with_ref = &url[slash + 1..];
    if let Some((repo_path, ref_name)) = path_with_ref.split_once('@')
        && !repo_path.is_empty()
        && !ref_name.is_empty()
    {
        return (format!("{host}/{repo_path}"), Some(ref_name.to_owned()));
    }
    (url.to_owned(), None)
}

fn build_git_source(
    repo: String,
    host: String,
    path: &str,
    ref_name: Option<String>,
) -> Option<ParsedSource> {
    if path.starts_with('/') {
        return None;
    }
    let normalized = path
        .trim_start_matches('/')
        .trim_end_matches(".git")
        .to_owned();
    if host.is_empty() || normalized.is_empty() || normalized.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(&host, false) || has_unsafe_git_install_part(&normalized, true) {
        return None;
    }
    let pinned = ref_name.is_some();
    Some(ParsedSource::Git {
        repo,
        host,
        path: normalized,
        ref_name,
        pinned,
    })
}

fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let decoded = urlencoding_decode(value);
    let candidates = match &decoded {
        Some(decoded) => vec![value, decoded.as_str()],
        None => return true,
    };
    for candidate in candidates {
        if candidate.contains('\0') || candidate.contains('\\') || candidate.starts_with('/') {
            return true;
        }
        if !allow_slash && candidate.contains('/') {
            return true;
        }
        if candidate.split('/').any(|part| part == "..") {
            return true;
        }
    }
    false
}

fn urlencoding_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h1 = (bytes[i + 1] as char).to_digit(16)?;
                let h2 = (bytes[i + 2] as char).to_digit(16)?;
                out.push(u8::try_from((h1 << 4) | h2).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn has_extension(name: &str, extension: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

fn collect_resource_files(dir: &Path, kind: ResourceKind) -> Vec<String> {
    match kind {
        ResourceKind::Skills => collect_skill_entries(dir, SkillDiscoveryMode::Pi, None, None),
        ResourceKind::Extensions => collect_auto_extension_entries(dir),
        ResourceKind::Prompts => {
            collect_files(dir, |name| has_extension(name, "md"), true, None, None)
        }
        ResourceKind::Themes => {
            collect_files(dir, |name| has_extension(name, "json"), true, None, None)
        }
    }
}

fn collect_files(
    dir: &Path,
    file_pred: impl Fn(&str) -> bool + Copy,
    skip_node_modules: bool,
    ignore_matcher: Option<&Gitignore>,
    root_dir: Option<&Path>,
) -> Vec<String> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    let root = root_dir.unwrap_or(dir);
    let mut local_builder = GitignoreBuilder::new(root);
    let ig = owned_or_extended(ignore_matcher, dir, root, &mut local_builder);
    let Ok(read_dir) = fs::read_dir(dir) else {
        return files;
    };
    let mut dir_entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    dir_entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in dir_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if skip_node_modules && name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        let is_dir = meta.is_dir();
        let is_file = meta.is_file();
        let rel = to_posix_path(&relative_path(root, &full_path));
        let ignore_path = if is_dir {
            format!("{rel}/")
        } else {
            rel.clone()
        };
        if ig.matched(&ignore_path, is_dir).is_ignore() {
            continue;
        }
        if is_dir {
            files.extend(collect_files(
                &full_path,
                file_pred,
                skip_node_modules,
                Some(&ig),
                Some(root),
            ));
        } else if is_file && file_pred(&name_str) {
            files.push(path_to_string(&full_path));
        }
    }
    files
}

fn collect_nonrecursive_entries(dir: &Path, file_pred: impl Fn(&str) -> bool) -> Vec<String> {
    let mut entries = Vec::new();
    if !dir.exists() {
        return entries;
    }
    let ig = build_ignore_matcher(dir, dir);
    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };
    let mut dir_entries: Vec<_> = read_dir.filter_map(Result::ok).collect();
    dir_entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in dir_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "node_modules" {
            continue;
        }
        let full_path = entry.path();
        let Ok(meta) = fs::metadata(&full_path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let rel = to_posix_path(&relative_path(dir, &full_path));
        if ig.matched(&rel, false).is_ignore() {
            continue;
        }
        if file_pred(&name_str) {
            entries.push(path_to_string(&full_path));
        }
    }
    entries
}

fn resolve_extension_entries(dir: &Path) -> Option<Vec<String>> {
    let manifest_file = dir.join("pi-extension.json");
    if manifest_file.exists() {
        return Some(vec![path_to_string(dir)]);
    }
    let package_json = dir.join("package.json");
    if package_json.exists()
        && let Some(manifest) = read_pi_manifest(dir)
        && let Some(extensions) = manifest.extensions
        && !extensions.is_empty()
    {
        let mut entries = Vec::new();
        for ext in extensions {
            let resolved = dir.join(&ext);
            if resolved.exists() {
                entries.push(path_to_string(&resolved));
            }
        }
        if !entries.is_empty() {
            return Some(entries);
        }
    }
    let typescript_index = dir.join("index.ts");
    if typescript_index.exists() {
        return Some(vec![path_to_string(&typescript_index)]);
    }
    let javascript_index = dir.join("index.js");
    if javascript_index.exists() {
        return Some(vec![path_to_string(&javascript_index)]);
    }
    None
}

fn read_pi_manifest(package_root: &Path) -> Option<PiManifest> {
    let package_json = package_root.join("package.json");
    let content = fs::read_to_string(package_json).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    let pi = value.get("pi")?;
    Some(PiManifest {
        extensions: string_array(pi.get("extensions")),
        skills: string_array(pi.get("skills")),
        prompts: string_array(pi.get("prompts")),
        themes: string_array(pi.get("themes")),
    })
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn manifest_entries(manifest: &PiManifest, kind: ResourceKind) -> Option<Vec<String>> {
    match kind {
        ResourceKind::Extensions => manifest.extensions.clone(),
        ResourceKind::Skills => manifest.skills.clone(),
        ResourceKind::Prompts => manifest.prompts.clone(),
        ResourceKind::Themes => manifest.themes.clone(),
    }
}

fn add_manifest_entries(
    entries: Option<&[String]>,
    root: &Path,
    kind: ResourceKind,
    target: &mut HashMap<String, AccumEntry>,
    metadata: &PathMetadata,
) {
    let Some(entries) = entries else {
        return;
    };
    let all_files = collect_files_from_manifest_entries(entries, root, kind);
    let patterns: Vec<String> = entries
        .iter()
        .filter(|entry| is_override_pattern(entry))
        .cloned()
        .collect();
    let all_path_strings: Vec<String> = all_files.iter().map(|p| path_to_string(p)).collect();
    let enabled = apply_patterns(&all_path_strings, &patterns, root);
    for file in all_path_strings {
        if enabled.contains(&file) {
            add_resource(target, file, metadata.clone(), true);
        }
    }
}

fn collect_default_resources(
    package_root: &Path,
    kind: ResourceKind,
    target: &mut HashMap<String, AccumEntry>,
    metadata: &PathMetadata,
) {
    if let Some(manifest) = read_pi_manifest(package_root) {
        let entries = manifest_entries(&manifest, kind);
        if entries.is_some() {
            add_manifest_entries(entries.as_deref(), package_root, kind, target, metadata);
            return;
        }
    }
    let dir = package_root.join(kind.as_str());
    if dir.exists() {
        for file in collect_resource_files(&dir, kind) {
            add_resource(target, file, metadata.clone(), true);
        }
    }
}

fn apply_package_filter(
    package_root: &Path,
    user_patterns: &[String],
    kind: ResourceKind,
    target: &mut HashMap<String, AccumEntry>,
    metadata: &PathMetadata,
) {
    let all_files = collect_manifest_files(package_root, kind);
    if user_patterns.is_empty() {
        for file in all_files {
            add_resource(target, file, metadata.clone(), false);
        }
        return;
    }
    let enabled = apply_patterns(&all_files, user_patterns, package_root);
    for file in all_files {
        let is_enabled = enabled.contains(&file);
        add_resource(target, file, metadata.clone(), is_enabled);
    }
}

fn apply_package_delta_filter(
    package_root: &Path,
    user_patterns: &[String],
    kind: ResourceKind,
    target: &mut HashMap<String, AccumEntry>,
    metadata: &PathMetadata,
) {
    if user_patterns.is_empty() {
        return;
    }
    let all_files = collect_manifest_files(package_root, kind);
    let enabled_map = apply_autoload_disabled_patterns(&all_files, user_patterns, package_root);
    for (file, enabled) in enabled_map {
        add_resource(target, file, metadata.clone(), enabled);
    }
}

fn apply_autoload_disabled_patterns(
    all_paths: &[String],
    patterns: &[String],
    base_dir: &Path,
) -> HashMap<String, bool> {
    let mut result = HashMap::new();
    for pattern in patterns {
        let (target, enabled, exact) = if let Some(rest) = pattern.strip_prefix('+') {
            (rest, true, true)
        } else if let Some(rest) = pattern.strip_prefix('-') {
            (rest, false, true)
        } else if let Some(rest) = pattern.strip_prefix('!') {
            (rest, false, false)
        } else {
            (pattern.as_str(), true, false)
        };
        for file_path in all_paths {
            let matched = if exact {
                matches_any_exact_pattern(file_path, &[target.to_owned()], base_dir)
            } else {
                matches_any_pattern(file_path, &[target.to_owned()], base_dir)
            };
            if matched {
                result.insert(file_path.clone(), enabled);
            }
        }
    }
    result
}

fn collect_manifest_files(package_root: &Path, kind: ResourceKind) -> Vec<String> {
    if let Some(manifest) = read_pi_manifest(package_root)
        && let Some(entries) = manifest_entries(&manifest, kind)
        && !entries.is_empty()
    {
        let all_files = collect_files_from_manifest_entries(&entries, package_root, kind);
        let patterns: Vec<String> = entries
            .iter()
            .filter(|entry| is_override_pattern(entry))
            .cloned()
            .collect();
        let all_path_strings: Vec<String> =
            all_files.iter().map(|path| path_to_string(path)).collect();
        if patterns.is_empty() {
            return all_path_strings;
        }
        return apply_patterns(&all_path_strings, &patterns, package_root)
            .into_iter()
            .collect();
    }
    let convention_dir = package_root.join(kind.as_str());
    if !convention_dir.exists() {
        return Vec::new();
    }
    collect_resource_files(&convention_dir, kind)
}

fn collect_files_from_manifest_entries(
    entries: &[String],
    root: &Path,
    kind: ResourceKind,
) -> Vec<PathBuf> {
    let source_entries: Vec<&String> = entries
        .iter()
        .filter(|entry| !is_override_pattern(entry))
        .collect();
    let mut resolved = Vec::new();
    for entry in source_entries {
        if has_glob_pattern(entry) {
            let mut builder = GlobSetBuilder::new();
            if let Ok(glob) = Glob::new(entry) {
                let _ = builder.add(glob);
            }
            if let Ok(set) = builder.build()
                && let Ok(walker) = ignore::WalkBuilder::new(root)
                    .hidden(false)
                    .git_ignore(false)
                    .build()
                    .collect::<Result<Vec<_>, _>>()
            {
                for dent in walker {
                    let path = dent.into_path();
                    let rel = relative_path(root, &path);
                    let rel_posix = to_posix_path(&rel);
                    if set.is_match(&rel_posix) {
                        resolved.push(path);
                    }
                }
            }
        } else {
            resolved.push(root.join(entry));
        }
    }
    collect_files_from_paths(&resolved, kind)
}

fn collect_files_from_paths(paths: &[PathBuf], kind: ResourceKind) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        if meta.is_file() {
            files.push(path.clone());
        } else if meta.is_dir() {
            files.extend(
                collect_resource_files(path, kind)
                    .into_iter()
                    .map(PathBuf::from),
            );
        }
    }
    files
}

fn is_pattern(s: &str) -> bool {
    s.starts_with('!')
        || s.starts_with('+')
        || s.starts_with('-')
        || s.contains('*')
        || s.contains('?')
}

fn is_override_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

fn has_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn split_patterns(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut plain = Vec::new();
    let mut patterns = Vec::new();
    for entry in entries {
        if is_pattern(entry) {
            patterns.push(entry.clone());
        } else {
            plain.push(entry.clone());
        }
    }
    (plain, patterns)
}

fn matches_any_pattern(file_path: &str, patterns: &[String], base_dir: &Path) -> bool {
    let file = Path::new(file_path);
    let rel = to_posix_path(&relative_path(base_dir, file));
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_path_posix = to_posix_path(file);
    let is_skill_file = name == "SKILL.md";
    let parent_dir = if is_skill_file { file.parent() } else { None };
    let parent_rel = parent_dir.map(|p| to_posix_path(&relative_path(base_dir, p)));
    let parent_name =
        parent_dir.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let parent_dir_posix = parent_dir.map(to_posix_path);

    patterns.iter().any(|pattern| {
        let normalized = to_posix_path(Path::new(pattern));
        if glob_match(&rel, &normalized)
            || glob_match(&name, &normalized)
            || glob_match(&file_path_posix, &normalized)
        {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        parent_rel
            .as_deref()
            .is_some_and(|value| glob_match(value, &normalized))
            || parent_name
                .as_deref()
                .is_some_and(|value| glob_match(value, &normalized))
            || parent_dir_posix
                .as_deref()
                .is_some_and(|value| glob_match(value, &normalized))
    })
}

fn normalize_exact_pattern(pattern: &str) -> String {
    let normalized = pattern
        .strip_prefix("./")
        .or_else(|| pattern.strip_prefix(".\\"))
        .unwrap_or(pattern);
    to_posix_path(Path::new(normalized))
}

fn matches_any_exact_pattern(file_path: &str, patterns: &[String], base_dir: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let file = Path::new(file_path);
    let rel = to_posix_path(&relative_path(base_dir, file));
    let file_path_posix = to_posix_path(file);
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_skill_file = name == "SKILL.md";
    let parent_dir = if is_skill_file { file.parent() } else { None };
    let parent_rel = parent_dir.map(|p| to_posix_path(&relative_path(base_dir, p)));
    let parent_dir_posix = parent_dir.map(to_posix_path);

    patterns.iter().any(|pattern| {
        let normalized = normalize_exact_pattern(pattern);
        if normalized == rel || normalized == file_path_posix {
            return true;
        }
        if !is_skill_file {
            return false;
        }
        parent_rel.as_deref() == Some(normalized.as_str())
            || parent_dir_posix.as_deref() == Some(normalized.as_str())
    })
}

fn glob_match(value: &str, pattern: &str) -> bool {
    Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(value))
}

fn to_posix_path(path: &Path) -> String {
    path_to_string(path).replace('\\', "/")
}

fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf)
}

fn build_ignore_matcher(dir: &Path, root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    add_ignore_rules(&mut builder, dir, root);
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn owned_or_extended(
    ignore_matcher: Option<&Gitignore>,
    dir: &Path,
    root: &Path,
    builder: &mut GitignoreBuilder,
) -> Gitignore {
    let _ = ignore_matcher;
    // Rebuild from root by walking ancestors is expensive; instead re-add
    // ignore files for `dir` only. Parent patterns are already applied via
    // recursive call chain that passes the previous matcher — for correctness
    // with the ignore crate we rebuild cumulative rules from root→dir.
    let mut cumulative = GitignoreBuilder::new(root);
    let mut stack = Vec::new();
    let mut current = dir.to_path_buf();
    loop {
        stack.push(current.clone());
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    for path in stack.into_iter().rev() {
        add_ignore_rules(&mut cumulative, &path, root);
    }
    let _ = builder;
    cumulative.build().unwrap_or_else(|_| Gitignore::empty())
}

fn add_ignore_rules(builder: &mut GitignoreBuilder, dir: &Path, root: &Path) {
    let relative_dir = relative_path(root, dir);
    let prefix = {
        let rel = to_posix_path(&relative_dir);
        if rel.is_empty() || rel == "." {
            String::new()
        } else {
            format!("{}/", rel.trim_end_matches('/'))
        }
    };
    for filename in [".gitignore", ".ignore", ".fdignore"] {
        let ignore_path = dir.join(filename);
        if !ignore_path.exists() {
            continue;
        }
        let Ok(content) = fs::read_to_string(&ignore_path) else {
            continue;
        };
        for line in content.split('\n') {
            let line = line.trim_end_matches('\r');
            if let Some(pattern) = prefix_ignore_pattern(line, &prefix) {
                let _ = builder.add_line(None, &pattern);
            }
        }
    }
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }
    let mut pattern = line.to_owned();
    let mut negated = false;
    if pattern.starts_with('!') {
        negated = true;
        pattern.drain(..1);
    } else if pattern.starts_with("\\!") {
        pattern.drain(..1);
    }
    if pattern.starts_with('/') {
        pattern.drain(..1);
    }
    let prefixed = if prefix.is_empty() {
        pattern
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::{SettingsManager, SettingsManagerCreateOptions};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn temp_root(label: &str) -> std::io::Result<PathBuf> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!("pi-resources-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[test]
    fn precedence_rank_matches_ts() {
        assert_eq!(
            resource_precedence_rank(&PathMetadata {
                source: "local".into(),
                scope: SourceScope::Project,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            }),
            0
        );
        assert_eq!(
            resource_precedence_rank(&PathMetadata {
                source: "auto".into(),
                scope: SourceScope::Project,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            }),
            1
        );
        assert_eq!(
            resource_precedence_rank(&PathMetadata {
                source: "local".into(),
                scope: SourceScope::User,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            }),
            2
        );
        assert_eq!(
            resource_precedence_rank(&PathMetadata {
                source: "auto".into(),
                scope: SourceScope::User,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            }),
            3
        );
        assert_eq!(
            resource_precedence_rank(&PathMetadata {
                source: "npm:x".into(),
                scope: SourceScope::User,
                origin: SourceOrigin::Package,
                base_dir: None,
            }),
            4
        );
    }

    #[test]
    fn apply_patterns_include_exclude_force() {
        let base = Path::new("/base");
        let paths = vec![
            "/base/a.md".into(),
            "/base/b.md".into(),
            "/base/c.md".into(),
        ];
        let enabled = apply_patterns(
            &paths,
            &[
                "*.md".into(),
                "!b.md".into(),
                "+b.md".into(),
                "-c.md".into(),
            ],
            base,
        );
        assert!(enabled.contains("/base/a.md"));
        assert!(enabled.contains("/base/b.md"));
        assert!(!enabled.contains("/base/c.md"));
    }

    #[test]
    fn skill_discovery_stops_at_skill_md() -> TestResult {
        let root = temp_root("skill-stop")?;
        let skill = root.join("demo");
        fs::create_dir_all(skill.join("nested"))?;
        fs::write(skill.join("SKILL.md"), "x")?;
        fs::write(skill.join("nested").join("SKILL.md"), "y")?;
        let entries = collect_skill_entries(&skill, SkillDiscoveryMode::Agents, None, None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].ends_with("SKILL.md"));
        assert!(!entries[0].contains("nested"));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn trust_skips_project_auto_discovery() -> TestResult {
        let root = temp_root("trust-skip")?;
        let cwd = root.join("project");
        let agent = root.join("agent");
        fs::create_dir_all(cwd.join(".pi").join("skills"))?;
        fs::create_dir_all(agent.join("skills"))?;
        fs::write(
            cwd.join(".pi").join("skills").join("proj.md"),
            "---\ndescription: p\n---\n",
        )?;
        fs::write(
            agent.join("skills").join("user.md"),
            "---\ndescription: u\n---\n",
        )?;

        let manager = SettingsManager::create(
            &cwd,
            Some(&agent),
            SettingsManagerCreateOptions::new().project_trusted(false),
        );
        let path_resolver = PackagePathResolver::new(&cwd, &agent, &manager);
        let resolved_paths = path_resolver.resolve()?;
        let skill_paths: Vec<_> = resolved_paths
            .skills
            .iter()
            .map(|skill| skill.path.clone())
            .collect();
        assert!(
            skill_paths.iter().any(|path| path.contains("user.md")),
            "user auto skills should load: {skill_paths:?}"
        );
        assert!(
            !skill_paths.iter().any(|path| path.contains("proj.md")),
            "project auto skills must be skipped when untrusted: {skill_paths:?}"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn package_identity_normalizes_npm_and_git() -> TestResult {
        let cwd = Path::new("/tmp/proj");
        let agent = Path::new("/tmp/agent");
        assert_eq!(
            package_identity("npm:@scope/pkg@1.2.3", None, cwd, agent, true)?,
            "npm:@scope/pkg"
        );
        assert_eq!(
            package_identity("git:github.com/user/repo", None, cwd, agent, true)?,
            "git:github.com/user/repo"
        );
        assert_eq!(
            package_identity("https://github.com/user/repo.git", None, cwd, agent, true)?,
            "git:github.com/user/repo"
        );
        Ok(())
    }

    #[test]
    fn is_enabled_by_overrides_force_exclude_wins() {
        let base = Path::new("/base");
        assert!(!is_enabled_by_overrides(
            "/base/a.md",
            &["!a.md".into(), "+a.md".into(), "-a.md".into()],
            base
        ));
    }

    #[test]
    fn temporary_dir_hash_is_sha256_hex_prefix() {
        // Node: createHash('sha256').update('npm-').digest('hex').slice(0,8)
        // Precomputed with hashlib.sha256(b'npm-').hexdigest()[:8]
        assert_eq!(temporary_dir_hash("npm", ""), "f35b2129");
        // hashlib.sha256(b'git-github.com-user/repo').hexdigest()[:8]
        assert_eq!(
            temporary_dir_hash("git-github.com", "user/repo"),
            "338a1076"
        );
        assert_ne!(
            temporary_dir_hash("npm", ""),
            temporary_dir_hash("npm", "x")
        );
    }

    #[test]
    fn parse_git_url_github_shorthand() {
        assert_eq!(
            parse_git_url("github:owner/repo"),
            Some(ParsedSource::Git {
                repo: "https://github.com/owner/repo".into(),
                host: "github.com".into(),
                path: "owner/repo".into(),
                ref_name: None,
                pinned: false,
            })
        );
    }

    #[test]
    fn parse_git_url_gitlab_shorthand_with_ref() {
        assert_eq!(
            parse_git_url("gitlab:owner/repo#feature/x"),
            Some(ParsedSource::Git {
                repo: "https://gitlab.com/owner/repo".into(),
                host: "gitlab.com".into(),
                path: "owner/repo".into(),
                ref_name: Some("feature/x".into()),
                pinned: true,
            })
        );
    }

    #[test]
    fn parse_git_url_git_prefix_around_shorthand() {
        assert_eq!(
            parse_git_url("git:github:owner/repo"),
            Some(ParsedSource::Git {
                repo: "https://github.com/owner/repo".into(),
                host: "github.com".into(),
                path: "owner/repo".into(),
                ref_name: None,
                pinned: false,
            })
        );
    }

    #[test]
    fn parse_git_url_other_shorthand_falls_through() {
        // Only github: and gitlab: are supported by hostedGitInfo.
        assert!(parse_git_url("bitbucket:owner/repo").is_none());
    }

    #[test]
    fn temporary_npm_path_includes_hash_segment() -> TestResult {
        let root = temp_root("temp-npm")?;
        let agent = root.join("agent");
        let cwd = root.join("cwd");
        fs::create_dir_all(&agent)?;
        fs::create_dir_all(&cwd)?;
        let manager = SettingsManager::create(
            &cwd,
            Some(&agent),
            SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let resolver = PackagePathResolver::new(&cwd, &agent, &manager);
        let path = resolver.managed_npm_install_path("foo", SourceScope::Temporary)?;
        let hash = temporary_dir_hash("npm", "");
        let expected = agent
            .join("tmp")
            .join("extensions")
            .join("npm")
            .join(&hash)
            .join("node_modules")
            .join("foo");
        assert_eq!(path, expected);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn temporary_git_path_includes_hash_and_suffix() -> TestResult {
        let root = temp_root("temp-git")?;
        let agent = root.join("agent");
        let cwd = root.join("cwd");
        fs::create_dir_all(&agent)?;
        fs::create_dir_all(&cwd)?;
        let manager = SettingsManager::create(
            &cwd,
            Some(&agent),
            SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let resolver = PackagePathResolver::new(&cwd, &agent, &manager);
        let path = resolver.git_install_path("github.com", "user/repo", SourceScope::Temporary)?;
        let hash = temporary_dir_hash("git-github.com", "user/repo");
        let expected = agent
            .join("tmp")
            .join("extensions")
            .join("git-github.com")
            .join(&hash)
            .join("user")
            .join("repo");
        assert_eq!(path, expected);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn legacy_npm_fallback_uses_existing_global_path() -> TestResult {
        let root = temp_root("legacy-npm")?;
        let agent = root.join("agent");
        let cwd = root.join("cwd");
        let global_root = root.join("global_nm");
        fs::create_dir_all(agent.join("npm").join("node_modules"))?;
        fs::create_dir_all(global_root.join("legacy-pkg"))?;
        fs::create_dir_all(&cwd)?;
        let bin = root.join("bin");
        fs::create_dir_all(&bin)?;
        let npm = bin.join("npm");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = root ] && [ \"$2\" = -g ]; then echo '{}'; exit 0; fi\nexit 1\n",
            global_root.display()
        );
        fs::write(&npm, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&npm)?.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&npm, permissions)?;
        }
        let mut manager = SettingsManager::create(
            &cwd,
            Some(&agent),
            SettingsManagerCreateOptions::new().project_trusted(true),
        );
        manager.set_npm_command(Some(vec![path_to_string(&npm)]));
        let resolver = PackagePathResolver::new(&cwd, &agent, &manager);
        let path = resolver.npm_install_path("legacy-pkg", SourceScope::User)?;
        assert_eq!(path, global_root.join("legacy-pkg"));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collect_auto_extension_recognizes_manifest_directory() -> TestResult {
        let root = temp_root("ext-manifest")?;
        let ext = root.join("manifest-ext");
        fs::create_dir_all(&ext)?;
        fs::write(
            ext.join("pi-extension.json"),
            r#"{"runtime":"native","entry":"entry"}"#,
        )?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(entries, vec![path_to_string(&ext)]);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collect_auto_extension_keeps_invalid_manifest_for_diagnostics() -> TestResult {
        let root = temp_root("ext-invalid-manifest")?;
        let ext = root.join("manifest-ext");
        fs::create_dir_all(ext.join("pi-extension.json"))?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(entries, vec![path_to_string(&ext)]);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn collect_auto_extension_prefers_manifest_to_legacy_entries() -> TestResult {
        let root = temp_root("ext-manifest-precedence")?;
        let ext = root.join("manifest-ext");
        fs::create_dir_all(&ext)?;
        fs::write(
            ext.join("pi-extension.json"),
            r#"{"runtime":"native","entry":"native"}"#,
        )?;
        fs::write(ext.join("index.ts"), "export default {}")?;
        fs::write(ext.join("legacy.ts"), "export default {}")?;
        fs::write(
            ext.join("package.json"),
            r#"{"pi":{"extensions":["legacy.ts"]}}"#,
        )?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(entries, vec![path_to_string(&ext)]);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    // ──────────────────────────────────────────────────────────────────
    // XC-9 / M20: discovery precedence witness —
    // pi-extension.json > package.json > auto-scan
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn m20_pi_extension_json_wins_over_package_json() -> TestResult {
        let root = temp_root("m20-manifest-vs-pkg")?;
        let ext = root.join("precedence-ext");
        fs::create_dir_all(&ext)?;
        fs::write(
            ext.join("pi-extension.json"),
            r#"{"runtime":"native","entry":"native-entry"}"#,
        )?;
        fs::write(ext.join("legacy.ts"), "export default {}")?;
        fs::write(
            ext.join("package.json"),
            r#"{"pi":{"extensions":["legacy.ts"]}}"#,
        )?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(
            entries,
            vec![path_to_string(&ext)],
            "pi-extension.json must take precedence over package.json"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn m20_package_json_wins_over_auto_scan_index() -> TestResult {
        let root = temp_root("m20-pkg-vs-index")?;
        let ext = root.join("pkg-ext");
        fs::create_dir_all(&ext)?;
        fs::write(ext.join("index.ts"), "export default {}")?;
        fs::write(ext.join("custom.ts"), "export default {}")?;
        fs::write(
            ext.join("package.json"),
            r#"{"pi":{"extensions":["custom.ts"]}}"#,
        )?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(
            entries,
            vec![path_to_string(&ext.join("custom.ts"))],
            "package.json extensions must take precedence over index.ts"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn m20_index_ts_wins_over_index_js() -> TestResult {
        let root = temp_root("m20-index-ts-vs-js")?;
        let ext = root.join("index-ext");
        fs::create_dir_all(&ext)?;
        fs::write(ext.join("index.ts"), "export default {}")?;
        fs::write(ext.join("index.js"), "export default {}")?;
        let entries = collect_auto_extension_entries(&root);
        assert_eq!(
            entries,
            vec![path_to_string(&ext.join("index.ts"))],
            "index.ts must take precedence over index.js"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
