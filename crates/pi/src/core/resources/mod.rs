//! Product resource discovery, loading, and snapshot management.
//!
//! Ports:
//! - `package-manager.ts` resolve-side → [`discovery`]
//! - `skills.ts` → [`skills`]
//! - `prompt-templates.ts` → [`prompts`]
//! - theme path collection from `resource-loader.ts` → [`themes`]
//! - `resource-loader.ts` → [`ResourceLoader`] / [`DefaultResourceLoader`]

pub mod diagnostics;
pub mod discovery;
pub mod frontmatter;
pub mod prompts;
pub mod skills;
pub mod slash;
pub mod source_info;
pub mod themes;

pub use diagnostics::{DiagnosticType, ResourceCollision, ResourceDiagnostic, ResourceType};
pub use discovery::{
    PackagePathResolver, PackageResolveError, ParsedSource, PathMetadata, ResolvedPaths,
    ResolvedResource, ResourceKind, SkillDiscoveryMode, apply_patterns,
    collect_ancestor_agents_skill_dirs, collect_auto_extension_entries,
    collect_auto_prompt_entries, collect_auto_theme_entries, collect_skill_entries,
    find_git_repo_root, is_enabled_by_overrides, package_identity, parse_package_source,
    path_to_string, resource_precedence_rank, temporary_dir_hash,
};
pub use frontmatter::{
    FrontmatterError, ParsedFrontmatter, extract_frontmatter, frontmatter_bool, frontmatter_string,
    normalize_newlines, parse_frontmatter, parse_yaml_value, strip_frontmatter,
};
pub use prompts::{
    LoadPromptTemplatesOptions, PromptTemplate, expand_prompt_template, load_prompt_templates,
    parse_command_args, substitute_args,
};
pub use skills::{
    LoadSkillsFromDirOptions, LoadSkillsOptions, LoadSkillsResult, Skill, expand_skill_invocation,
    format_skills_for_prompt, load_skills, load_skills_from_dir,
};
pub use slash::{
    BUILTIN_SLASH_COMMAND_COUNT, BuiltinSlashCommand, SlashCommandInfo, SlashCommandSource,
    builtin_quit_description, builtin_slash_commands,
};
pub use source_info::{
    SourceInfo, SourceOrigin, SourceScope, SyntheticSourceInfoOptions, create_source_info,
    create_synthetic_source_info,
};
pub use themes::{
    LoadThemesOptions, LoadThemesResult, LoadedTheme, load_theme_from_file, load_themes,
    load_themes_from_dir,
};

use std::collections::HashMap;
use std::future::Future;

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::core::config::{
    CONFIG_DIR_NAME, PathInputOptions, canonicalize_path, is_local_path, resolve_path_with,
};
use crate::core::settings::{SettingsManager, SettingsManagerCreateOptions};

/// Extension path info for Phase 3 (paths only; no execution).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionPathInfo {
    /// Configured / discovered path.
    pub path: String,
    /// Resolved absolute path when available.
    pub resolved_path: String,
    /// Provenance.
    pub source_info: SourceInfo,
}

/// Extension load result for Phase 3: paths + missing-path errors only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadExtensionsResult {
    /// Extension paths (not executed in Phase 3).
    pub paths: Vec<ExtensionPathInfo>,
    /// Path-level errors (`{path, error}`).
    pub errors: Vec<ExtensionLoadError>,
}

/// Single extension path error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionLoadError {
    /// Path that failed.
    pub path: String,
    /// Error message.
    pub error: String,
}

/// Context file (`AGENTS.md` / `CLAUDE.md`) content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentsFile {
    /// Absolute path.
    pub path: String,
    /// File contents.
    pub content: String,
}

/// Paths registered via [`ResourceLoader::extend_resources`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResourceExtensionPaths {
    /// Extra skill paths with metadata.
    pub skill_paths: Vec<ExtensionResourcePath>,
    /// Extra prompt paths with metadata.
    pub prompt_paths: Vec<ExtensionResourcePath>,
    /// Extra theme paths with metadata.
    pub theme_paths: Vec<ExtensionResourcePath>,
}

impl ResourceExtensionPaths {
    /// Whether discovery produced no skill, prompt, or theme paths.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skill_paths.is_empty() && self.prompt_paths.is_empty() && self.theme_paths.is_empty()
    }
}

/// One extension-provided resource path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionResourcePath {
    /// Resource path.
    pub path: String,
    /// Discovery metadata.
    pub metadata: PathMetadata,
}

impl ExtensionResourcePath {
    /// Build temporary provenance for a path returned by one extension.
    #[must_use]
    pub fn discovered(path: String, extension_path: &str) -> Self {
        let synthetic = extension_path.starts_with('<');
        let label = if synthetic {
            extension_path.trim_matches(['<', '>']).to_owned()
        } else {
            Path::new(extension_path)
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or(extension_path)
                .to_owned()
        };
        Self {
            path,
            metadata: PathMetadata {
                source: format!("extension:{label}"),
                scope: source_info::SourceScope::Temporary,
                origin: source_info::SourceOrigin::TopLevel,
                base_dir: (!synthetic).then(|| {
                    Path::new(extension_path)
                        .parent()
                        .map_or_else(String::new, path_to_string)
                }),
            },
        }
    }
}

/// Source-compatible disabled flag used by resource-loader construction options.
///
/// Loader construction immediately normalizes these flags into one discovery policy.
pub type ResourceLoadingDisabled = bool;

/// Options for [`DefaultResourceLoader::new`].
pub struct DefaultResourceLoaderOptions {
    /// Working directory.
    pub cwd: PathBuf,
    /// Agent config directory.
    pub agent_dir: PathBuf,
    /// Optional settings manager (created when absent).
    pub settings_manager: Option<SettingsManager>,
    /// Additional CLI extension sources.
    pub additional_extension_paths: Vec<String>,
    /// Additional CLI skill paths.
    pub additional_skill_paths: Vec<String>,
    /// Additional CLI prompt template paths.
    pub additional_prompt_template_paths: Vec<String>,
    /// Additional CLI theme paths.
    pub additional_theme_paths: Vec<String>,
    /// Skip package/settings extensions (CLI still applied).
    pub no_extensions: ResourceLoadingDisabled,
    /// Skip package/settings skills (CLI still applied).
    pub no_skills: ResourceLoadingDisabled,
    /// Skip package/settings prompts (CLI still applied).
    pub no_prompt_templates: ResourceLoadingDisabled,
    /// Skip package/settings themes (CLI still applied).
    pub no_themes: ResourceLoadingDisabled,
    /// Skip AGENTS/CLAUDE context files.
    pub no_context_files: ResourceLoadingDisabled,
    /// Explicit system prompt (file path or literal).
    pub system_prompt: Option<String>,
    /// Explicit append system prompts (file paths or literals).
    pub append_system_prompt: Option<Vec<String>>,
}

impl Default for DefaultResourceLoaderOptions {
    fn default() -> Self {
        Self {
            cwd: PathBuf::from("."),
            agent_dir: crate::core::config::get_agent_dir(),
            settings_manager: None,
            additional_extension_paths: Vec::new(),
            additional_skill_paths: Vec::new(),
            additional_prompt_template_paths: Vec::new(),
            additional_theme_paths: Vec::new(),
            no_extensions: false,
            no_skills: false,
            no_prompt_templates: false,
            no_themes: false,
            no_context_files: false,
            system_prompt: None,
            append_system_prompt: None,
        }
    }
}

/// Resource loader errors.
#[derive(Debug, Error)]
pub enum ResourceLoaderError {
    /// Package path resolution failed.
    #[error(transparent)]
    Package(#[from] PackageResolveError),
    /// Blocking task join failed.
    #[error("resource resolve task failed: {0}")]
    Join(String),
}

/// Resource loading surface matching TypeScript `ResourceLoader`.
pub trait ResourceLoader {
    /// Extension paths (Phase 3: no execution).
    fn get_extensions(&self) -> &LoadExtensionsResult;
    /// Skills + diagnostics.
    fn get_skills(&self) -> (&[Skill], &[ResourceDiagnostic]);
    /// Prompts + diagnostics.
    fn get_prompts(&self) -> (&[PromptTemplate], &[ResourceDiagnostic]);
    /// Themes + diagnostics.
    fn get_themes(&self) -> (&[LoadedTheme], &[ResourceDiagnostic]);
    /// Context files.
    fn get_agents_files(&self) -> &[AgentsFile];
    /// Resolved system prompt text, if any.
    fn get_system_prompt(&self) -> Option<&str>;
    /// Resolved append-system-prompt texts.
    fn get_append_system_prompt(&self) -> &[String];
    /// Extend skill/prompt/theme paths after load.
    fn extend_resources(&mut self, paths: ResourceExtensionPaths);
    /// Reload settings, packages, and all resource snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`ResourceLoaderError`] when package resolve fails or the
    /// blocking resolve task panics.
    fn reload(&mut self) -> impl Future<Output = Result<(), ResourceLoaderError>>;
}
/// Immutable resource snapshot replaced on reload.
#[derive(Clone, Debug, Default)]
struct ResourceSnapshot {
    extensions: LoadExtensionsResult,
    skills: Vec<Skill>,
    skill_diagnostics: Vec<ResourceDiagnostic>,
    prompts: Vec<PromptTemplate>,
    prompt_diagnostics: Vec<ResourceDiagnostic>,
    themes: Vec<LoadedTheme>,
    theme_diagnostics: Vec<ResourceDiagnostic>,
    agents_files: Vec<AgentsFile>,
    system_prompt: Option<String>,
    append_system_prompt: Vec<String>,
    last_skill_paths: Vec<String>,
    last_prompt_paths: Vec<String>,
    last_theme_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResourceDiscoveryPolicy(u8);

impl ResourceDiscoveryPolicy {
    const EXTENSIONS: u8 = 1 << 0;
    const SKILLS: u8 = 1 << 1;
    const PROMPT_TEMPLATES: u8 = 1 << 2;
    const THEMES: u8 = 1 << 3;
    const CONTEXT_FILES: u8 = 1 << 4;

    fn from_options(options: &DefaultResourceLoaderOptions) -> Self {
        let mut bits = 0;
        for (disabled, flag) in [
            (options.no_extensions, Self::EXTENSIONS),
            (options.no_skills, Self::SKILLS),
            (options.no_prompt_templates, Self::PROMPT_TEMPLATES),
            (options.no_themes, Self::THEMES),
            (options.no_context_files, Self::CONTEXT_FILES),
        ] {
            if disabled {
                bits |= flag;
            }
        }
        Self(bits)
    }

    const fn disables(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Default)]
struct EnabledResourcePaths {
    extensions: Vec<String>,
    skills: Vec<String>,
    prompts: Vec<String>,
    themes: Vec<String>,
}

struct ReloadDiscovery {
    metadata_by_path: HashMap<String, PathMetadata>,
    package: EnabledResourcePaths,
    cli: EnabledResourcePaths,
}

/// Default resource loader with immutable snapshot replaced on reload.
pub struct DefaultResourceLoader {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings_manager: SettingsManager,
    additional_extension_paths: Vec<String>,
    additional_skill_paths: Vec<String>,
    additional_prompt_template_paths: Vec<String>,
    additional_theme_paths: Vec<String>,
    base_skill_paths: Vec<String>,
    base_prompt_paths: Vec<String>,
    base_theme_paths: Vec<String>,
    extension_skill_paths: Vec<String>,
    extension_prompt_paths: Vec<String>,
    extension_theme_paths: Vec<String>,
    discovery: ResourceDiscoveryPolicy,
    system_prompt_source: Option<String>,
    append_system_prompt_source: Option<Vec<String>>,
    extension_skill_source_infos: HashMap<String, SourceInfo>,
    extension_prompt_source_infos: HashMap<String, SourceInfo>,
    extension_theme_source_infos: HashMap<String, SourceInfo>,
    snapshot: ResourceSnapshot,
    loaded: bool,
}

impl DefaultResourceLoader {
    /// Create a loader (does not load until [`ResourceLoader::reload`]).
    #[must_use]
    pub fn new(options: DefaultResourceLoaderOptions) -> Self {
        let cwd = resolve_path_with(
            &path_to_string(&options.cwd),
            Path::new("."),
            PathInputOptions::new(),
        );
        let agent_dir = resolve_path_with(
            &path_to_string(&options.agent_dir),
            Path::new("."),
            PathInputOptions::new(),
        );
        let discovery = ResourceDiscoveryPolicy::from_options(&options);
        let settings_manager = options.settings_manager.unwrap_or_else(|| {
            SettingsManager::create(
                &cwd,
                Some(&agent_dir),
                SettingsManagerCreateOptions::default(),
            )
        });
        Self {
            cwd,
            agent_dir,
            settings_manager,
            additional_extension_paths: options.additional_extension_paths,
            additional_skill_paths: options.additional_skill_paths,
            additional_prompt_template_paths: options.additional_prompt_template_paths,
            additional_theme_paths: options.additional_theme_paths,
            base_skill_paths: Vec::new(),
            base_prompt_paths: Vec::new(),
            base_theme_paths: Vec::new(),
            extension_skill_paths: Vec::new(),
            extension_prompt_paths: Vec::new(),
            extension_theme_paths: Vec::new(),
            discovery,
            system_prompt_source: options.system_prompt,
            append_system_prompt_source: options.append_system_prompt,
            extension_skill_source_infos: HashMap::new(),
            extension_prompt_source_infos: HashMap::new(),
            extension_theme_source_infos: HashMap::new(),
            snapshot: ResourceSnapshot::default(),
            loaded: false,
        }
    }

    /// Mutable access to the settings manager.
    pub fn settings_manager_mut(&mut self) -> &mut SettingsManager {
        &mut self.settings_manager
    }

    /// Shared settings manager reference.
    #[must_use]
    pub fn settings_manager(&self) -> &SettingsManager {
        &self.settings_manager
    }

    fn resolve_resource_path(&self, path: &str) -> PathBuf {
        resolve_path_with(path, &self.cwd, PathInputOptions::new().trim(true))
    }

    fn merge_paths(&self, primary: &[String], additional: &[String]) -> Vec<String> {
        let mut merged = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in primary.iter().chain(additional.iter()) {
            let resolved = self.resolve_resource_path(path);
            let canonical = path_to_string(&canonicalize_path(&resolved));
            if !seen.insert(canonical) {
                continue;
            }
            merged.push(path_to_string(&resolved));
        }
        merged
    }

    fn map_skill_path(
        resource: &ResolvedResource,
        metadata_by_path: &mut HashMap<String, PathMetadata>,
    ) -> String {
        if resource.metadata.source != "auto" && resource.metadata.origin != SourceOrigin::Package {
            return resource.path.clone();
        }
        let path = Path::new(&resource.path);
        let Ok(meta) = fs::metadata(path) else {
            return resource.path.clone();
        };
        if !meta.is_dir() {
            return resource.path.clone();
        }
        let skill_file = path.join("SKILL.md");
        if skill_file.exists() {
            let skill_path = path_to_string(&skill_file);
            metadata_by_path
                .entry(skill_path.clone())
                .or_insert_with(|| resource.metadata.clone());
            return skill_path;
        }
        resource.path.clone()
    }

    fn update_skills_from_paths(
        &mut self,
        skill_paths: &[String],
        metadata_by_path: Option<&HashMap<String, PathMetadata>>,
    ) {
        let result =
            if self.discovery.disables(ResourceDiscoveryPolicy::SKILLS) && skill_paths.is_empty() {
                LoadSkillsResult::default()
            } else {
                load_skills(&LoadSkillsOptions {
                    cwd: self.cwd.clone(),
                    agent_dir: self.agent_dir.clone(),
                    skill_paths: skill_paths.to_vec(),
                    include_defaults: false,
                })
            };
        self.snapshot.skills = result
            .skills
            .into_iter()
            .map(|mut skill| {
                let existing = skill.source_info.clone();
                skill.source_info = self
                    .find_source_info_for_path(
                        &skill.file_path,
                        Some(&self.extension_skill_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or(existing);
                skill
            })
            .collect();
        self.snapshot.skill_diagnostics = result.diagnostics;
    }

    fn update_prompts_from_paths(
        &mut self,
        prompt_paths: &[String],
        metadata_by_path: Option<&HashMap<String, PathMetadata>>,
    ) {
        let (prompts, mut diagnostics) = if self
            .discovery
            .disables(ResourceDiscoveryPolicy::PROMPT_TEMPLATES)
            && prompt_paths.is_empty()
        {
            (Vec::new(), Vec::new())
        } else {
            let all = load_prompt_templates(&LoadPromptTemplatesOptions {
                cwd: self.cwd.clone(),
                agent_dir: self.agent_dir.clone(),
                prompt_paths: prompt_paths.to_vec(),
                include_defaults: false,
            });
            dedupe_prompts(all)
        };
        self.snapshot.prompts = prompts
            .into_iter()
            .map(|mut prompt| {
                let existing = prompt.source_info.clone();
                prompt.source_info = self
                    .find_source_info_for_path(
                        &prompt.file_path,
                        Some(&self.extension_prompt_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or(existing);
                prompt
            })
            .collect();
        self.snapshot.prompt_diagnostics = {
            let _ = &mut diagnostics;
            diagnostics
        };
    }

    fn update_themes_from_paths(
        &mut self,
        theme_paths: &[String],
        metadata_by_path: Option<&HashMap<String, PathMetadata>>,
    ) {
        let result =
            if self.discovery.disables(ResourceDiscoveryPolicy::THEMES) && theme_paths.is_empty() {
                LoadThemesResult::default()
            } else {
                load_themes(&LoadThemesOptions {
                    cwd: self.cwd.clone(),
                    theme_paths: theme_paths.to_vec(),
                })
            };
        self.snapshot.themes = result
            .themes
            .into_iter()
            .map(|mut theme| {
                let existing = theme.source_info.clone();
                theme.source_info = self
                    .find_source_info_for_path(
                        &theme.source_path,
                        Some(&self.extension_theme_source_infos),
                        metadata_by_path,
                    )
                    .unwrap_or(existing);
                theme
            })
            .collect();
        self.snapshot.theme_diagnostics = result.diagnostics;
    }

    fn find_source_info_for_path(
        &self,
        resource_path: &str,
        extra: Option<&HashMap<String, SourceInfo>>,
        metadata_by_path: Option<&HashMap<String, PathMetadata>>,
    ) -> Option<SourceInfo> {
        if resource_path.is_empty() {
            return None;
        }
        if resource_path.starts_with('<') {
            return Some(self.default_source_info_for_path(resource_path));
        }
        let normalized = resolve_path_with(resource_path, Path::new("."), PathInputOptions::new());
        let normalized_str = path_to_string(&normalized);
        if let Some(extra) = extra {
            for (source_path, source_info) in extra {
                let source_resolved =
                    resolve_path_with(source_path, Path::new("."), PathInputOptions::new());
                if normalized == source_resolved || normalized.starts_with(&source_resolved) {
                    let mut info = source_info.clone();
                    resource_path.clone_into(&mut info.path);
                    return Some(info);
                }
            }
        }
        if let Some(metadata_by_path) = metadata_by_path {
            if let Some(meta) = metadata_by_path
                .get(&normalized_str)
                .or_else(|| metadata_by_path.get(resource_path))
            {
                return Some(create_source_info(resource_path, meta));
            }
            for (source_path, metadata) in metadata_by_path {
                let source_resolved =
                    resolve_path_with(source_path, Path::new("."), PathInputOptions::new());
                if normalized == source_resolved || normalized.starts_with(&source_resolved) {
                    return Some(create_source_info(resource_path, metadata));
                }
            }
        }
        None
    }

    fn default_source_info_for_path(&self, file_path: &str) -> SourceInfo {
        if file_path.starts_with('<') && file_path.ends_with('>') {
            let inner = &file_path[1..file_path.len() - 1];
            let source = inner.split(':').next().unwrap_or("temporary");
            return SourceInfo {
                path: file_path.to_owned(),
                source: source.to_owned(),
                scope: SourceScope::Temporary,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            };
        }
        let normalized = resolve_path_with(file_path, Path::new("."), PathInputOptions::new());
        let agent_roots = [
            self.agent_dir.join("skills"),
            self.agent_dir.join("prompts"),
            self.agent_dir.join("themes"),
            self.agent_dir.join("extensions"),
        ];
        let project_roots = [
            self.cwd.join(CONFIG_DIR_NAME).join("skills"),
            self.cwd.join(CONFIG_DIR_NAME).join("prompts"),
            self.cwd.join(CONFIG_DIR_NAME).join("themes"),
            self.cwd.join(CONFIG_DIR_NAME).join("extensions"),
        ];
        for root in &agent_roots {
            if is_under_path(&normalized, root) {
                return SourceInfo {
                    path: file_path.to_owned(),
                    source: "local".into(),
                    scope: SourceScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(path_to_string(root)),
                };
            }
        }
        for root in &project_roots {
            if is_under_path(&normalized, root) {
                return SourceInfo {
                    path: file_path.to_owned(),
                    source: "local".into(),
                    scope: SourceScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: Some(path_to_string(root)),
                };
            }
        }
        let base_dir = if normalized.is_dir() {
            path_to_string(&normalized)
        } else {
            normalized
                .parent()
                .map_or_else(|| path_to_string(&normalized), path_to_string)
        };
        SourceInfo {
            path: file_path.to_owned(),
            source: "local".into(),
            scope: SourceScope::Temporary,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(base_dir),
        }
    }

    fn discover_system_prompt_file(&self) -> Option<String> {
        let project_path = self.cwd.join(CONFIG_DIR_NAME).join("SYSTEM.md");
        if self.settings_manager.is_project_trusted() && project_path.exists() {
            return Some(path_to_string(&project_path));
        }
        let global_path = self.agent_dir.join("SYSTEM.md");
        if global_path.exists() {
            return Some(path_to_string(&global_path));
        }
        None
    }

    fn discover_append_system_prompt_file(&self) -> Option<String> {
        let project_path = self.cwd.join(CONFIG_DIR_NAME).join("APPEND_SYSTEM.md");
        if self.settings_manager.is_project_trusted() && project_path.exists() {
            return Some(path_to_string(&project_path));
        }
        let global_path = self.agent_dir.join("APPEND_SYSTEM.md");
        if global_path.exists() {
            return Some(path_to_string(&global_path));
        }
        None
    }

    fn normalize_extension_paths(
        &self,
        entries: &[ExtensionResourcePath],
    ) -> Vec<ExtensionResourcePath> {
        entries
            .iter()
            .map(|entry| {
                let mut metadata = entry.metadata.clone();
                let base = metadata
                    .base_dir
                    .as_deref()
                    .map(|base| self.resolve_resource_path(base));
                metadata.base_dir = base.as_deref().map(path_to_string);
                let path = resolve_path_with(
                    &entry.path,
                    base.as_deref().unwrap_or(&self.cwd),
                    PathInputOptions::new(),
                );
                ExtensionResourcePath {
                    path: path_to_string(&path),
                    metadata,
                }
            })
            .collect()
    }

    async fn resolve_reload_discovery(&self) -> Result<ReloadDiscovery, ResourceLoaderError> {
        let cwd = self.cwd.clone();
        let agent_dir = self.agent_dir.clone();
        let project_trusted = self.settings_manager.is_project_trusted();
        let additional_extension_paths = self.additional_extension_paths.clone();
        let (resolved, cli) = tokio::task::spawn_blocking(move || {
            let settings = SettingsManager::create(
                &cwd,
                Some(&agent_dir),
                SettingsManagerCreateOptions::new().project_trusted(project_trusted),
            );
            let resolver = PackagePathResolver::new(&cwd, &agent_dir, &settings);
            let packages = resolver.resolve()?;
            let cli =
                resolver.resolve_extension_sources(&additional_extension_paths, true, false)?;
            Ok::<_, PackageResolveError>((packages, cli))
        })
        .await
        .map_err(|error| ResourceLoaderError::Join(error.to_string()))??;
        Ok(Self::collect_reload_discovery(&resolved, &cli))
    }

    fn collect_reload_discovery(resolved: &ResolvedPaths, cli: &ResolvedPaths) -> ReloadDiscovery {
        let mut metadata_by_path = HashMap::new();
        for resources in [
            &resolved.extensions,
            &resolved.skills,
            &resolved.prompts,
            &resolved.themes,
        ] {
            for resource in resources {
                metadata_by_path
                    .entry(resource.path.clone())
                    .or_insert_with(|| resource.metadata.clone());
            }
        }
        for resource in cli.extensions.iter().chain(&cli.skills) {
            metadata_by_path
                .entry(resource.path.clone())
                .or_insert(PathMetadata {
                    source: "cli".into(),
                    scope: SourceScope::Temporary,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                });
        }
        let mut package = EnabledResourcePaths {
            extensions: enabled_paths(&resolved.extensions),
            prompts: enabled_paths(&resolved.prompts),
            themes: enabled_paths(&resolved.themes),
            ..EnabledResourcePaths::default()
        };
        package.skills = resolved
            .skills
            .iter()
            .filter(|resource| resource.enabled)
            .map(|resource| Self::map_skill_path(resource, &mut metadata_by_path))
            .collect();
        let cli = EnabledResourcePaths {
            extensions: enabled_paths(&cli.extensions),
            skills: enabled_paths(&cli.skills),
            prompts: enabled_paths(&cli.prompts),
            themes: enabled_paths(&cli.themes),
        };
        ReloadDiscovery {
            metadata_by_path,
            package,
            cli,
        }
    }

    fn load_extension_phase(&mut self, discovery: &ReloadDiscovery) {
        let paths = if self.discovery.disables(ResourceDiscoveryPolicy::EXTENSIONS) {
            discovery.cli.extensions.clone()
        } else {
            self.merge_paths(&discovery.cli.extensions, &discovery.package.extensions)
        };
        let mut result = LoadExtensionsResult {
            paths: paths
                .iter()
                .map(|path| ExtensionPathInfo {
                    path: path.clone(),
                    resolved_path: path_to_string(&self.resolve_resource_path(path)),
                    source_info: discovery.metadata_by_path.get(path).map_or_else(
                        || self.default_source_info_for_path(path),
                        |metadata| create_source_info(path, metadata),
                    ),
                })
                .collect(),
            errors: Vec::new(),
        };
        for path in &self.additional_extension_paths {
            if is_local_path(path) {
                let resolved = self.resolve_resource_path(path);
                if !resolved.exists() {
                    let resolved = path_to_string(&resolved);
                    result.errors.push(ExtensionLoadError {
                        path: resolved.clone(),
                        error: format!("Extension path does not exist: {resolved}"),
                    });
                }
            }
        }
        self.snapshot.extensions = result;
    }

    fn load_skill_phase(&mut self, discovery: &ReloadDiscovery) {
        let base_paths = if self.discovery.disables(ResourceDiscoveryPolicy::SKILLS) {
            self.merge_paths(&discovery.cli.skills, &self.additional_skill_paths)
        } else {
            let mut primary = discovery.cli.skills.clone();
            primary.extend(discovery.package.skills.iter().cloned());
            self.merge_paths(&primary, &self.additional_skill_paths)
        };
        self.base_skill_paths.clone_from(&base_paths);
        let paths = self.merge_paths(&base_paths, &self.extension_skill_paths);
        self.snapshot.last_skill_paths.clone_from(&paths);
        self.update_skills_from_paths(&paths, Some(&discovery.metadata_by_path));
        for path in &self.additional_skill_paths {
            if is_local_path(path) {
                let resolved = path_to_string(&self.resolve_resource_path(path));
                if !Path::new(&resolved).exists()
                    && !self
                        .snapshot
                        .skill_diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.path.as_deref() == Some(resolved.as_str()))
                {
                    self.snapshot
                        .skill_diagnostics
                        .push(ResourceDiagnostic::error(
                            "Skill path does not exist",
                            Some(resolved),
                        ));
                }
            }
        }
    }

    fn load_prompt_phase(&mut self, discovery: &ReloadDiscovery) {
        let base_paths = if self
            .discovery
            .disables(ResourceDiscoveryPolicy::PROMPT_TEMPLATES)
        {
            self.merge_paths(
                &discovery.cli.prompts,
                &self.additional_prompt_template_paths,
            )
        } else {
            let mut primary = discovery.cli.prompts.clone();
            primary.extend(discovery.package.prompts.iter().cloned());
            self.merge_paths(&primary, &self.additional_prompt_template_paths)
        };
        self.base_prompt_paths.clone_from(&base_paths);
        let paths = self.merge_paths(&base_paths, &self.extension_prompt_paths);
        self.snapshot.last_prompt_paths.clone_from(&paths);
        self.update_prompts_from_paths(&paths, Some(&discovery.metadata_by_path));
        for path in &self.additional_prompt_template_paths {
            if is_local_path(path) {
                let resolved = path_to_string(&self.resolve_resource_path(path));
                if !Path::new(&resolved).exists()
                    && !self
                        .snapshot
                        .prompt_diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.path.as_deref() == Some(resolved.as_str()))
                {
                    self.snapshot
                        .prompt_diagnostics
                        .push(ResourceDiagnostic::error(
                            "Prompt template path does not exist",
                            Some(resolved),
                        ));
                }
            }
        }
    }

    fn load_theme_phase(&mut self, discovery: &ReloadDiscovery) {
        let base_paths = if self.discovery.disables(ResourceDiscoveryPolicy::THEMES) {
            self.merge_paths(&discovery.cli.themes, &self.additional_theme_paths)
        } else {
            let mut primary = discovery.cli.themes.clone();
            primary.extend(discovery.package.themes.iter().cloned());
            self.merge_paths(&primary, &self.additional_theme_paths)
        };
        self.base_theme_paths.clone_from(&base_paths);
        let paths = self.merge_paths(&base_paths, &self.extension_theme_paths);
        self.snapshot.last_theme_paths.clone_from(&paths);
        self.update_themes_from_paths(&paths, Some(&discovery.metadata_by_path));
        for path in &self.additional_theme_paths {
            let resolved = path_to_string(&self.resolve_resource_path(path));
            if !Path::new(&resolved).exists()
                && !self
                    .snapshot
                    .theme_diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.path.as_deref() == Some(resolved.as_str()))
            {
                self.snapshot
                    .theme_diagnostics
                    .push(ResourceDiagnostic::error(
                        "Theme path does not exist",
                        Some(resolved),
                    ));
            }
        }
    }

    fn load_context_and_prompts_phase(&mut self) {
        self.snapshot.agents_files = if self
            .discovery
            .disables(ResourceDiscoveryPolicy::CONTEXT_FILES)
        {
            Vec::new()
        } else {
            load_project_context_files(&self.cwd, &self.agent_dir)
        };
        self.snapshot.system_prompt = resolve_prompt_input(
            self.system_prompt_source
                .clone()
                .or_else(|| self.discover_system_prompt_file()),
        );
        let append_sources = self.append_system_prompt_source.clone().unwrap_or_else(|| {
            self.discover_append_system_prompt_file()
                .into_iter()
                .collect()
        });
        self.snapshot.append_system_prompt = append_sources
            .into_iter()
            .filter_map(|source| resolve_prompt_input(Some(source)))
            .collect();
    }
}

impl ResourceLoader for DefaultResourceLoader {
    fn get_extensions(&self) -> &LoadExtensionsResult {
        &self.snapshot.extensions
    }

    fn get_skills(&self) -> (&[Skill], &[ResourceDiagnostic]) {
        (&self.snapshot.skills, &self.snapshot.skill_diagnostics)
    }

    fn get_prompts(&self) -> (&[PromptTemplate], &[ResourceDiagnostic]) {
        (&self.snapshot.prompts, &self.snapshot.prompt_diagnostics)
    }

    fn get_themes(&self) -> (&[LoadedTheme], &[ResourceDiagnostic]) {
        (&self.snapshot.themes, &self.snapshot.theme_diagnostics)
    }

    fn get_agents_files(&self) -> &[AgentsFile] {
        &self.snapshot.agents_files
    }

    fn get_system_prompt(&self) -> Option<&str> {
        self.snapshot.system_prompt.as_deref()
    }

    fn get_append_system_prompt(&self) -> &[String] {
        &self.snapshot.append_system_prompt
    }

    fn extend_resources(&mut self, paths: ResourceExtensionPaths) {
        let skill_paths = self.normalize_extension_paths(&paths.skill_paths);
        let prompt_paths = self.normalize_extension_paths(&paths.prompt_paths);
        let theme_paths = self.normalize_extension_paths(&paths.theme_paths);

        self.extension_skill_paths = path_strings(&skill_paths);
        self.extension_prompt_paths = path_strings(&prompt_paths);
        self.extension_theme_paths = path_strings(&theme_paths);

        self.extension_skill_source_infos = source_infos(&skill_paths);
        self.extension_prompt_source_infos = source_infos(&prompt_paths);
        self.extension_theme_source_infos = source_infos(&theme_paths);

        self.snapshot.last_skill_paths =
            self.merge_paths(&self.base_skill_paths, &self.extension_skill_paths);
        let skill_snapshot = self.snapshot.last_skill_paths.clone();
        self.update_skills_from_paths(&skill_snapshot, None);

        self.snapshot.last_prompt_paths =
            self.merge_paths(&self.base_prompt_paths, &self.extension_prompt_paths);
        let prompt_snapshot = self.snapshot.last_prompt_paths.clone();
        self.update_prompts_from_paths(&prompt_snapshot, None);

        self.snapshot.last_theme_paths =
            self.merge_paths(&self.base_theme_paths, &self.extension_theme_paths);
        let theme_snapshot = self.snapshot.last_theme_paths.clone();
        self.update_themes_from_paths(&theme_snapshot, None);
    }

    async fn reload(&mut self) -> Result<(), ResourceLoaderError> {
        let _ = self.loaded;
        self.settings_manager.reload();
        let discovery = self.resolve_reload_discovery().await?;
        self.extension_skill_source_infos.clear();
        self.extension_prompt_source_infos.clear();
        self.extension_theme_source_infos.clear();
        self.load_extension_phase(&discovery);
        self.load_skill_phase(&discovery);
        self.load_prompt_phase(&discovery);
        self.load_theme_phase(&discovery);
        self.load_context_and_prompts_phase();
        self.loaded = true;
        Ok(())
    }
}

/// Load AGENTS/CLAUDE context files: global agentDir first, then root→cwd.
///
/// Not trust-gated (matches TypeScript `loadProjectContextFiles`).
#[must_use]
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<AgentsFile> {
    let resolved_cwd = resolve_path_with(
        &path_to_string(cwd),
        Path::new("."),
        PathInputOptions::new(),
    );
    let resolved_agent_dir = resolve_path_with(
        &path_to_string(agent_dir),
        Path::new("."),
        PathInputOptions::new(),
    );
    let mut context_files = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    if let Some(global) = load_context_file_from_dir(&resolved_agent_dir) {
        seen_paths.insert(global.path.clone());
        context_files.push(global);
    }

    let mut ancestor = Vec::new();
    let mut current = resolved_cwd;
    loop {
        if let Some(context) = load_context_file_from_dir(&current)
            && seen_paths.insert(context.path.clone())
        {
            ancestor.insert(0, context);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    context_files.extend(ancestor);
    context_files
}

fn load_context_file_from_dir(dir: &Path) -> Option<AgentsFile> {
    for filename in ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"] {
        let file_path = dir.join(filename);
        if file_path.exists()
            && let Ok(content) = fs::read_to_string(&file_path)
        {
            return Some(AgentsFile {
                path: path_to_string(&file_path),
                content,
            });
        }
    }
    None
}

fn resolve_prompt_input(input: Option<String>) -> Option<String> {
    let input = input?;
    let path = Path::new(&input);
    if path.exists() {
        match fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(_) => Some(input),
        }
    } else {
        Some(input)
    }
}

fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut seen: HashMap<String, PromptTemplate> = HashMap::new();
    let mut order = Vec::new();
    let mut diagnostics = Vec::new();
    for prompt in prompts {
        if let Some(existing) = seen.get(&prompt.name) {
            diagnostics.push(ResourceDiagnostic::collision(
                format!("name \"/{}\" collision", prompt.name),
                Some(prompt.file_path.clone()),
                ResourceCollision {
                    resource_type: ResourceType::Prompt,
                    name: prompt.name.clone(),
                    winner_path: existing.file_path.clone(),
                    loser_path: prompt.file_path.clone(),
                    winner_source: None,
                    loser_source: None,
                },
            ));
        } else {
            order.push(prompt.name.clone());
            seen.insert(prompt.name.clone(), prompt);
        }
    }
    let prompts = order
        .into_iter()
        .filter_map(|name| seen.remove(&name))
        .collect();
    (prompts, diagnostics)
}

fn path_strings(paths: &[ExtensionResourcePath]) -> Vec<String> {
    paths.iter().map(|entry| entry.path.clone()).collect()
}

fn source_infos(paths: &[ExtensionResourcePath]) -> HashMap<String, SourceInfo> {
    paths
        .iter()
        .map(|entry| {
            (
                entry.path.clone(),
                create_source_info(&entry.path, &entry.metadata),
            )
        })
        .collect()
}

fn is_under_path(target: &Path, root: &Path) -> bool {
    let normalized_root = resolve_path_with(
        &path_to_string(root),
        Path::new("."),
        PathInputOptions::new(),
    );
    let normalized_target = resolve_path_with(
        &path_to_string(target),
        Path::new("."),
        PathInputOptions::new(),
    );
    if normalized_target == normalized_root {
        return true;
    }
    // Match TS: prefix is root + separator — never `/skills-extra` for `/skills`.
    let root_str = path_to_string(&normalized_root);
    let target_str = path_to_string(&normalized_target);
    let sep = std::path::MAIN_SEPARATOR;
    let prefix = if root_str.ends_with(sep) {
        root_str
    } else {
        format!("{root_str}{sep}")
    };
    target_str.starts_with(&prefix)
}

fn enabled_paths(resources: &[ResolvedResource]) -> Vec<String> {
    resources
        .iter()
        .filter(|resource| resource.enabled)
        .map(|resource| resource.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::SettingsManagerCreateOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(label: &str) -> std::io::Result<PathBuf> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let root = std::env::temp_dir().join(format!("pi-resloader-{label}-{nanos}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)?;
        Ok(root)
    }

    #[tokio::test]
    async fn reload_loads_skills_prompts_themes_and_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("reload")?;
        let cwd = root.join("project");
        let agent = root.join("agent");
        fs::create_dir_all(agent.join("skills").join("demo"))?;
        fs::create_dir_all(agent.join("prompts"))?;
        fs::create_dir_all(agent.join("themes"))?;
        fs::create_dir_all(&cwd)?;
        fs::write(
            agent.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody\n",
        )?;
        fs::write(
            agent.join("prompts").join("hello.md"),
            "---\ndescription: hi\n---\nHello $1\n",
        )?;
        fs::write(agent.join("themes").join("t.json"), r#"{"name":"t"}"#)?;
        fs::write(agent.join("AGENTS.md"), "global agents\n")?;
        fs::write(cwd.join("AGENTS.md"), "project agents\n")?;
        fs::write(agent.join("SYSTEM.md"), "system\n")?;
        fs::write(agent.join("APPEND_SYSTEM.md"), "append\n")?;

        let settings = SettingsManager::create(
            &cwd,
            Some(&agent),
            SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let mut loader = DefaultResourceLoader::new(DefaultResourceLoaderOptions {
            cwd: cwd.clone(),
            agent_dir: agent.clone(),
            settings_manager: Some(settings),
            ..Default::default()
        });
        loader.reload().await?;

        let (skills, _) = loader.get_skills();
        assert!(skills.iter().any(|s| s.name == "demo"));
        let (prompts, _) = loader.get_prompts();
        assert!(prompts.iter().any(|p| p.name == "hello"));
        let (themes, _) = loader.get_themes();
        assert!(themes.iter().any(|t| t.name == "t"));
        let agents = loader.get_agents_files();
        assert!(agents.len() >= 2);
        assert_eq!(loader.get_system_prompt(), Some("system\n"));
        assert_eq!(loader.get_append_system_prompt(), &["append\n".to_owned()]);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    struct ExtensionFixture {
        root: PathBuf,
        cwd: PathBuf,
        agent: PathBuf,
        extension_dir: PathBuf,
        base_skill: PathBuf,
        extension_path: PathBuf,
    }

    fn setup_extension_fixture() -> Result<ExtensionFixture, Box<dyn std::error::Error>> {
        let root = temp_root("extension-resources")?;
        let cwd = root.join("project");
        let agent = root.join("agent");
        let extension_dir = root.join("outside").join("extension");
        let base_skill = extension_dir.join("base-skill");
        let extra_skill = extension_dir.join("extra-skill");
        fs::create_dir_all(&cwd)?;
        fs::create_dir_all(&agent)?;
        fs::create_dir_all(&base_skill)?;
        fs::create_dir_all(&extra_skill)?;
        fs::write(
            base_skill.join("SKILL.md"),
            "---\nname: base\ndescription: base\n---\nbase\n",
        )?;
        fs::write(
            extra_skill.join("SKILL.md"),
            "---\nname: extra\ndescription: extra\n---\nextra\n",
        )?;
        fs::write(
            extension_dir.join("prompt.md"),
            "---\ndescription: prompt\n---\nPrompt\n",
        )?;
        fs::write(extension_dir.join("theme.json"), r#"{"name":"extension"}"#)?;
        let extension_path = extension_dir.join("plugin.ts");
        fs::write(&extension_path, "")?;

        Ok(ExtensionFixture {
            root,
            cwd,
            agent,
            extension_dir,
            base_skill,
            extension_path,
        })
    }

    fn assert_extension_resources_loaded(
        loader: &DefaultResourceLoader,
        extension_dir: &std::path::Path,
    ) {
        let ext_dir_str = path_to_string(extension_dir);
        assert!(loader.get_skills().0.iter().any(|skill| {
            skill.name == "extra"
                && skill.source_info.source == "extension:plugin"
                && skill.source_info.scope == SourceScope::Temporary
                && skill.source_info.base_dir.as_deref() == Some(&ext_dir_str)
        }));
        assert!(
            loader
                .get_prompts()
                .0
                .iter()
                .any(|prompt| prompt.name == "prompt")
        );
        assert!(
            loader
                .get_themes()
                .0
                .iter()
                .any(|theme| theme.name == "extension")
        );
    }

    fn assert_empty_replacement_and_base_preservation(loader: &DefaultResourceLoader) {
        assert!(
            loader
                .get_skills()
                .0
                .iter()
                .any(|skill| skill.name == "base")
        );
        assert!(
            !loader
                .get_skills()
                .0
                .iter()
                .any(|skill| skill.name == "extra")
        );
        assert!(
            !loader
                .get_prompts()
                .0
                .iter()
                .any(|prompt| prompt.name == "prompt")
        );
        assert!(
            !loader
                .get_themes()
                .0
                .iter()
                .any(|theme| theme.name == "extension")
        );
    }

    #[tokio::test]
    async fn extension_paths_resolve_from_extension_and_replace_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = setup_extension_fixture()?;
        let extension_path = path_to_string(&fixture.extension_path);

        let settings = SettingsManager::create(
            &fixture.cwd,
            Some(&fixture.agent),
            SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let mut loader = DefaultResourceLoader::new(DefaultResourceLoaderOptions {
            cwd: fixture.cwd.clone(),
            agent_dir: fixture.agent,
            settings_manager: Some(settings),
            additional_skill_paths: vec![path_to_string(&fixture.base_skill)],
            ..Default::default()
        });
        loader.reload().await?;
        loader.extend_resources(ResourceExtensionPaths {
            skill_paths: vec![
                ExtensionResourcePath::discovered("base-skill".to_owned(), &extension_path),
                ExtensionResourcePath::discovered("extra-skill".to_owned(), &extension_path),
            ],
            prompt_paths: vec![ExtensionResourcePath::discovered(
                "prompt.md".to_owned(),
                &extension_path,
            )],
            theme_paths: vec![ExtensionResourcePath::discovered(
                "theme.json".to_owned(),
                &extension_path,
            )],
        });

        assert_extension_resources_loaded(&loader, &fixture.extension_dir);

        loader.reload().await?;
        assert!(
            loader
                .get_skills()
                .0
                .iter()
                .any(|skill| skill.name == "extra")
        );
        loader.extend_resources(ResourceExtensionPaths::default());
        assert_empty_replacement_and_base_preservation(&loader);

        let _ = fs::remove_dir_all(fixture.root);
        Ok(())
    }

    #[test]
    fn context_files_not_trust_gated() -> Result<(), Box<dyn std::error::Error>> {
        let root = temp_root("ctx")?;
        let cwd = root.join("p");
        let agent = root.join("a");
        fs::create_dir_all(&cwd)?;
        fs::create_dir_all(&agent)?;
        fs::write(cwd.join("AGENTS.md"), "p\n")?;
        let files = load_project_context_files(&cwd, &agent);
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains('p'));
        let _ = fs::remove_dir_all(root);
        Ok(())
    }
}
