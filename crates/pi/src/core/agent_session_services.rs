//! Cwd-bound runtime services and session factory helpers.
//!
//! Ports the creation order and diagnostics surface from
//! `coding-agent/src/core/agent-session-services.ts`, plus the model-priority /
//! restore-fallback helpers from `model-resolver.ts` and auth-guidance strings.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_ai::providers::KnownProvider;
use pi_ai::types::{Model, ModelThinkingLevel};
use pi_ext::protocol::FlagValueWire;
use thiserror::Error;

use super::config::{get_agent_dir, get_docs_path, resolve_path};
use super::extension_runtime_set::{
    ExtensionRuntimeSet, ExtensionSetDiagnostic, ExtensionSetStart,
};
use super::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelRuntimeError, ProviderConfigInput,
};
use super::resources::{
    DefaultResourceLoader, DefaultResourceLoaderOptions, ExtensionPathInfo, ResourceLoader,
    SourceInfo,
};
use super::settings::{SettingsManager, SettingsManagerCreateOptions};
use super::trust::{ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted};

/// Default thinking level when none is configured (`medium`).
pub const DEFAULT_THINKING_LEVEL: ModelThinkingLevel = ModelThinkingLevel::Medium;

/// Default model ids for each known provider (catalog priority order).
#[must_use]
pub fn default_model_per_provider() -> &'static [(&'static str, &'static str)] {
    &[
        ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
        ("ant-ling", "Ring-2.6-1T"),
        ("anthropic", "claude-opus-4-8"),
        ("openai", "gpt-5.5"),
        ("azure-openai-responses", "gpt-5.4"),
        ("openai-codex", "gpt-5.5"),
        ("radius", "auto"),
        ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
        ("deepseek", "deepseek-v4-pro"),
        ("google", "gemini-3.1-pro-preview"),
        ("google-vertex", "gemini-3.1-pro-preview"),
        ("github-copilot", "gpt-5.4"),
        ("openrouter", "moonshotai/kimi-k2.6"),
        ("vercel-ai-gateway", "zai/glm-5.1"),
        ("xai", "grok-4.5"),
        ("groq", "openai/gpt-oss-120b"),
        ("cerebras", "zai-glm-4.7"),
        ("zai", "glm-5.1"),
        ("zai-coding-cn", "glm-5.1"),
        ("mistral", "devstral-medium-latest"),
        ("minimax", "MiniMax-M2.7"),
        ("minimax-cn", "MiniMax-M2.7"),
        ("moonshotai", "kimi-k2.6"),
        ("moonshotai-cn", "kimi-k2.6"),
        ("huggingface", "moonshotai/Kimi-K2.6"),
        ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
        ("together", "moonshotai/Kimi-K2.6"),
        ("opencode", "kimi-k2.6"),
        ("opencode-go", "kimi-k2.6"),
        ("kimi-coding", "kimi-for-coding"),
        ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
        (
            "cloudflare-ai-gateway",
            "workers-ai/@cf/moonshotai/kimi-k2.6",
        ),
        ("xiaomi", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
        ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
    ]
}

/// Severity of a runtime diagnostic collected while creating services/sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSessionRuntimeDiagnosticKind {
    /// Informational note.
    Info,
    /// Non-fatal issue the app may surface.
    Warning,
    /// Hard failure the app should treat as startup-blocking.
    Error,
}

/// Non-fatal issue collected while creating services or sessions.
///
/// Runtime creation returns diagnostics to the caller instead of printing or
/// exiting. The app layer decides whether warnings should be shown and whether
/// errors should abort startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionRuntimeDiagnostic {
    /// Severity.
    pub kind: AgentSessionRuntimeDiagnosticKind,
    /// Human-readable message (exact TypeScript wording where applicable).
    pub message: String,
}

impl AgentSessionRuntimeDiagnostic {
    /// Info diagnostic.
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            kind: AgentSessionRuntimeDiagnosticKind::Info,
            message: message.into(),
        }
    }

    /// Warning diagnostic.
    #[must_use]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            kind: AgentSessionRuntimeDiagnosticKind::Warning,
            message: message.into(),
        }
    }

    /// Error diagnostic.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: AgentSessionRuntimeDiagnosticKind::Error,
            message: message.into(),
        }
    }
}

/// Inputs for creating cwd-bound runtime services.
#[derive(Default)]
pub struct CreateAgentSessionServicesOptions {
    /// Working directory for project-local discovery.
    pub cwd: PathBuf,
    /// Global agent config directory. Defaults to [`get_agent_dir`].
    pub agent_dir: Option<PathBuf>,
    /// Optional pre-built settings manager.
    pub settings_manager: Option<SettingsManager>,
    /// Optional pre-built model runtime.
    pub model_runtime: Option<ModelRuntime>,
    /// Extension CLI flag values (`--flag` / `--flag value`) awaiting validation.
    pub extension_flag_values: Option<BTreeMap<String, ExtensionFlagValue>>,
    /// Resource-loader discovery options. Services supply paths and trust.
    pub resource_loader_options: Option<ResourceLoaderServiceOptions>,
    /// Pending provider registrations discovered by extensions (tests / host).
    ///
    /// Each entry is `(provider_id, config, extension_path)`. Production wiring
    /// feeds this from the extension host; Phase 3 resource loader only exposes
    /// paths, so the seam is injectable.
    pub pending_provider_registrations: Vec<PendingProviderRegistration>,
    /// Registered extension flags for the unknown-flag validation seam.
    ///
    /// When empty, every supplied extension flag is treated as unknown.
    pub registered_extension_flags: BTreeMap<String, ExtensionFlagType>,
}

/// Source-compatible boolean used by resource-discovery input fields.
///
/// Service creation immediately normalizes these flags into one private policy
/// bitset so internal phases cannot observe an incoherent collection of booleans.
pub type ResourceDiscoveryDisabled = bool;

/// Resource-loader construction knobs owned by services creation.
#[derive(Clone, Debug, Default)]
pub struct ResourceLoaderServiceOptions {
    /// Additional extension paths.
    pub additional_extension_paths: Vec<String>,
    /// Additional skill paths.
    pub additional_skill_paths: Vec<String>,
    /// Additional prompt template paths.
    pub additional_prompt_template_paths: Vec<String>,
    /// Additional theme paths.
    pub additional_theme_paths: Vec<String>,
    /// Disable extension discovery.
    pub no_extensions: ResourceDiscoveryDisabled,
    /// Disable skill discovery.
    pub no_skills: ResourceDiscoveryDisabled,
    /// Disable prompt templates.
    pub no_prompt_templates: ResourceDiscoveryDisabled,
    /// Disable themes.
    pub no_themes: ResourceDiscoveryDisabled,
    /// Disable context files.
    pub no_context_files: ResourceDiscoveryDisabled,
    /// Explicit system prompt override.
    pub system_prompt: Option<String>,
    /// Explicit append-system-prompt overrides.
    pub append_system_prompt: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResourceDiscoveryPolicy(u8);

impl ResourceDiscoveryPolicy {
    const EXTENSIONS: u8 = 1 << 0;
    const SKILLS: u8 = 1 << 1;
    const PROMPT_TEMPLATES: u8 = 1 << 2;
    const THEMES: u8 = 1 << 3;
    const CONTEXT_FILES: u8 = 1 << 4;

    fn from_options(options: &ResourceLoaderServiceOptions) -> Self {
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

/// Pending extension provider registration applied during services creation.
#[derive(Clone, Debug)]
pub struct PendingProviderRegistration {
    /// Provider id to register.
    pub name: String,
    /// Provider configuration.
    pub config: ProviderConfigInput,
    /// Extension path used in diagnostic messages.
    pub extension_path: String,
}

/// Extension CLI flag type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionFlagType {
    /// Boolean flag (`--flag` sets true).
    Boolean,
    /// String flag requiring a value.
    String,
}

/// Parsed extension flag value from CLI unknown-flags capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionFlagValue {
    /// Boolean presence.
    Bool(bool),
    /// String value.
    Str(String),
}

/// Coherent cwd-bound runtime services for one effective session cwd.
///
/// This is infrastructure only. The [`AgentSession`](crate::core::agent_session::AgentSession)
/// itself is created separately so session options can be resolved against these services first.
pub struct AgentSessionServices {
    /// Resolved working directory.
    pub cwd: PathBuf,
    /// Resolved agent directory.
    pub agent_dir: PathBuf,
    /// Shared model/auth runtime.
    pub model_runtime: ModelRuntime,
    /// Settings manager bound to this services `cwd`/`agent_dir`.
    ///
    /// The single authoritative owner until it moves into the session via
    /// [`CreateAgentSessionResult`]. The resource loader no longer holds a
    /// manager; it stores only the resolved `project_trusted` bit.
    pub settings_manager: SettingsManager,
    /// Resource loader (paths + snapshots; stores the resolved trust bit only).
    pub resource_loader: DefaultResourceLoader,
    /// Diagnostics collected during creation.
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    /// Validated extension flag values applied during creation.
    pub extension_flag_values: BTreeMap<String, ExtensionFlagValue>,
    /// Concrete host runner when extensions were discovered and loaded.
    ///
    /// `None` when no extension paths were discovered, discovery was disabled,
    /// or host start failed (degraded to diagnostics only).
    pub extension_runner: Option<Arc<ExtensionRuntimeSet>>,
}

/// Inputs for creating a session from already-created services.
pub struct CreateAgentSessionFromServicesOptions {
    /// Previously created services.
    pub services: AgentSessionServices,
    /// Optional explicit model.
    pub model: Option<Model>,
    /// Optional thinking level.
    pub thinking_level: Option<ModelThinkingLevel>,
    /// Scoped models for cycling.
    pub scoped_models: Vec<ScopedModel>,
    /// Optional tool allowlist.
    pub tools: Option<Vec<String>>,
    /// Optional tool denylist.
    pub exclude_tools: Option<Vec<String>>,
    /// Default tool suppression mode when no allowlist is provided.
    pub no_tools: Option<NoToolsMode>,
    /// Session-start metadata carried into the session (reason + previous
    /// session file for replacements).
    pub session_start_event: Option<crate::core::agent_session::SessionStartEvent>,
    /// Saved session model to restore (provider, model id).
    pub saved_session_model: Option<(String, String)>,
    /// Whether the session already has messages (affects restore vs initial).
    pub has_existing_session: bool,
}

/// Tool-suppression mode when no explicit allowlist is provided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoToolsMode {
    /// Start with no tools enabled.
    All,
    /// Disable default built-ins but keep extension/custom tools.
    Builtin,
}

/// Model + optional thinking level from a scope pattern.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedModel {
    /// Resolved model.
    pub model: Model,
    /// Thinking level when the pattern specified one.
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Result of initial model selection.
#[derive(Clone, Debug, PartialEq)]
pub struct InitialModelResult {
    /// Selected model, when any is available/configured.
    pub model: Option<Model>,
    /// Thinking level to use with the model.
    pub thinking_level: ModelThinkingLevel,
    /// Optional fallback warning (unused by pure initial selection).
    pub fallback_message: Option<String>,
}

/// Complete inputs for constructing an agent session.
///
/// The result owns each cwd-bound runtime component. A caller can borrow the
/// settings manager while it builds dependent inputs, then move the manager
/// into the session.
pub struct CreateAgentSessionResult {
    /// Resolved model after restore / initial selection.
    pub model: Option<Model>,
    /// Resolved thinking level.
    pub thinking_level: ModelThinkingLevel,
    /// Initial active tool names after allow/exclude/`no_tools` resolution.
    pub initial_active_tool_names: Vec<String>,
    /// Optional allowlist (None = default set).
    pub allowed_tool_names: Option<Vec<String>>,
    /// Optional denylist.
    pub excluded_tool_names: Option<Vec<String>>,
    /// Scoped models for cycling.
    pub scoped_models: Vec<ScopedModel>,
    /// Warning when a saved model could not be restored or no model is available.
    pub model_fallback_message: Option<String>,
    /// Diagnostics accumulated during session creation (in addition to services).
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    /// Working directory bound into the session.
    pub cwd: PathBuf,
    /// Agent directory bound into the session.
    pub agent_dir: PathBuf,
    /// Shared model runtime.
    pub model_runtime: ModelRuntime,
    /// Settings manager moved from services into the session (single owner).
    pub settings_manager: SettingsManager,
    /// Resource loader retained for extension-driven reloads.
    pub resource_loader: DefaultResourceLoader,
    /// Session-start metadata forwarded from the creation options.
    pub session_start_event: Option<crate::core::agent_session::SessionStartEvent>,
    /// Concrete host runner moved out of services (if any).
    pub extension_runner: Option<Arc<ExtensionRuntimeSet>>,
}

/// Failures from services / session factory operations.
#[derive(Clone, Debug, Error)]
pub enum AgentSessionServicesError {
    /// Model runtime construction failed.
    #[error(transparent)]
    ModelRuntime(#[from] ModelRuntimeError),
    /// Project trust resolution failed before resource loading.
    #[error("{0}")]
    Trust(String),
    /// Resource loader reload failed.
    #[error("{0}")]
    ResourceLoader(String),
}

/// Provider login help block used by no-model guidance strings.
#[must_use]
pub fn get_provider_login_help() -> String {
    let docs = get_docs_path();
    format!(
        "Use /login to log in to a provider via OAuth or API key. See:\n  {}\n  {}",
        docs.join("providers.md").display(),
        docs.join("models.md").display()
    )
}

/// Message when no models are available at all.
#[must_use]
pub fn format_no_models_available_message() -> String {
    format!("No models available. {}", get_provider_login_help())
}

/// Message when a prompt is attempted with no selected model.
#[must_use]
pub fn format_no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        get_provider_login_help()
    )
}

/// Message when auth is missing for a provider.
#[must_use]
pub fn format_no_api_key_found_message(provider: &str) -> String {
    let provider_display = if provider == "unknown" {
        "the selected model"
    } else {
        provider
    };
    format!(
        "No API key found for {provider_display}.\n\n{}",
        get_provider_login_help()
    )
}

/// OAuth-specific auth failure guidance.
#[must_use]
pub fn format_oauth_auth_failed_message(provider: &str) -> String {
    format!(
        "Authentication failed for \"{provider}\". Credentials may have expired or network is unavailable. Run '/login {provider}' to re-authenticate."
    )
}

/// Create all cwd-bound session services with trust resolved from saved/global state.
///
/// # Errors
///
/// Returns service construction, trust resolution, or resource loading errors.
pub async fn create_agent_session_services(
    options: CreateAgentSessionServicesOptions,
) -> Result<AgentSessionServices, AgentSessionServicesError> {
    create_agent_session_services_with_trust(options, None).await
}

/// Create all cwd-bound session services with an invocation-scoped trust override.
///
/// The override is resolved before project settings, packages, or extensions load.
///
/// # Errors
///
/// Returns service construction, trust resolution, or resource loading errors.
pub async fn create_agent_session_services_with_trust(
    options: CreateAgentSessionServicesOptions,
    project_trust_override: Option<bool>,
) -> Result<AgentSessionServices, AgentSessionServicesError> {
    let CreateAgentSessionServicesOptions {
        cwd,
        agent_dir,
        settings_manager,
        model_runtime,
        extension_flag_values,
        resource_loader_options,
        pending_provider_registrations,
        registered_extension_flags,
    } = options;
    let settings_manager_was_supplied = settings_manager.is_some();
    let mut foundation =
        create_service_foundation(cwd, agent_dir, settings_manager, model_runtime).await?;
    let project_trusted = if settings_manager_was_supplied && project_trust_override.is_none() {
        foundation.settings_manager.is_project_trusted()
    } else {
        resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: foundation.cwd.clone(),
            trust_store: &ProjectTrustStore::new(&foundation.agent_dir),
            trust_override: project_trust_override,
            default_project_trust: foundation.settings_manager.get_default_project_trust(),
            extension_hook: None,
            ui: None,
            on_extension_error: None,
        })
        .map_err(|error| AgentSessionServicesError::Trust(error.to_string()))?
    };
    foundation
        .settings_manager
        .set_project_trusted(project_trusted);
    let (resource_loader, discovery) = create_service_resource_loader(
        &foundation.cwd,
        &foundation.agent_dir,
        project_trusted,
        resource_loader_options.unwrap_or_default(),
    )
    .await?;

    let mut diagnostics = extension_discovery_diagnostics(&resource_loader);
    let (mut extension_runner, host_registered_flags) = start_extension_phase(
        &resource_loader,
        discovery,
        &foundation.cwd,
        &foundation.model_runtime,
        project_trusted,
        &mut diagnostics,
    )
    .await;

    let mut registered_flags = registered_extension_flags;
    registered_flags.extend(host_registered_flags);
    let (flag_diagnostics, applied_flags) =
        apply_extension_flag_values(extension_flag_values.unwrap_or_default(), &registered_flags);
    diagnostics.extend(flag_diagnostics);
    if let Some(runner) = extension_runner.as_deref() {
        match apply_flags_to_runner(runner, &applied_flags).await {
            Ok(flag_diagnostics) => {
                diagnostics.extend(flag_diagnostics.into_iter().map(|diagnostic| {
                    AgentSessionRuntimeDiagnostic::warning(diagnostic.to_string())
                }));
            }
            Err(error) => {
                diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
                    "Extension flags failed to apply: {error}"
                )));
                runner.unregister_providers_from(&foundation.model_runtime);
                runner.shutdown_once().await;
                extension_runner = None;
            }
        }
    }

    for registration in pending_provider_registrations {
        if let Err(error) = foundation
            .model_runtime
            .register_provider(&registration.name, registration.config)
        {
            diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
                "Extension \"{}\" error: {error}",
                registration.extension_path
            )));
        }
    }

    let _ = foundation
        .model_runtime
        .refresh(super::model_runtime::ModelsRefreshOptions {
            allow_network: Some(false),
        })
        .await;

    Ok(AgentSessionServices {
        cwd: foundation.cwd,
        agent_dir: foundation.agent_dir,
        model_runtime: foundation.model_runtime,
        settings_manager: foundation.settings_manager,
        resource_loader,
        diagnostics,
        extension_flag_values: applied_flags,
        extension_runner,
    })
}

struct ServiceFoundation {
    cwd: PathBuf,
    agent_dir: PathBuf,
    model_runtime: ModelRuntime,
    settings_manager: SettingsManager,
}

async fn create_service_foundation(
    cwd: PathBuf,
    agent_dir: Option<PathBuf>,
    settings_manager: Option<SettingsManager>,
    model_runtime: Option<ModelRuntime>,
) -> Result<ServiceFoundation, AgentSessionServicesError> {
    let cwd = resolve_path(cwd.to_string_lossy().as_ref());
    let agent_dir = agent_dir.map_or_else(get_agent_dir, |path| {
        resolve_path(path.to_string_lossy().as_ref())
    });
    let model_runtime = match model_runtime {
        Some(runtime) => runtime,
        None => {
            ModelRuntime::create(CreateModelRuntimeOptions {
                auth_path: Some(agent_dir.join("auth.json")),
                models_path: Some(agent_dir.join("models.json")),
                models_store_path: Some(agent_dir.join("models-store.json")),
                allow_model_network: Some(false),
                ..CreateModelRuntimeOptions::default()
            })
            .await?
        }
    };
    let settings_manager = settings_manager.unwrap_or_else(|| {
        SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            SettingsManagerCreateOptions::default().project_trusted(false),
        )
    });
    Ok(ServiceFoundation {
        cwd,
        agent_dir,
        model_runtime,
        settings_manager,
    })
}

async fn create_service_resource_loader(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    options: ResourceLoaderServiceOptions,
) -> Result<(DefaultResourceLoader, ResourceDiscoveryPolicy), AgentSessionServicesError> {
    let discovery = ResourceDiscoveryPolicy::from_options(&options);
    let mut loader = DefaultResourceLoader::new(DefaultResourceLoaderOptions {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        project_trusted,
        additional_extension_paths: options.additional_extension_paths,
        additional_skill_paths: options.additional_skill_paths,
        additional_prompt_template_paths: options.additional_prompt_template_paths,
        additional_theme_paths: options.additional_theme_paths,
        no_extensions: discovery.disables(ResourceDiscoveryPolicy::EXTENSIONS),
        no_skills: discovery.disables(ResourceDiscoveryPolicy::SKILLS),
        no_prompt_templates: discovery.disables(ResourceDiscoveryPolicy::PROMPT_TEMPLATES),
        no_themes: discovery.disables(ResourceDiscoveryPolicy::THEMES),
        no_context_files: discovery.disables(ResourceDiscoveryPolicy::CONTEXT_FILES),
        system_prompt: options.system_prompt,
        append_system_prompt: options.append_system_prompt,
    });
    loader
        .reload()
        .await
        .map_err(|error| AgentSessionServicesError::ResourceLoader(error.to_string()))?;
    Ok((loader, discovery))
}

fn extension_discovery_diagnostics(
    loader: &DefaultResourceLoader,
) -> Vec<AgentSessionRuntimeDiagnostic> {
    loader
        .get_extensions()
        .errors
        .iter()
        .map(|error| {
            AgentSessionRuntimeDiagnostic::error(format!(
                "Extension \"{}\" error: {}",
                error.path, error.error
            ))
        })
        .collect()
}

fn record_extension_start(
    started: ExtensionSetStart,
    diagnostics: &mut Vec<AgentSessionRuntimeDiagnostic>,
) -> Option<Arc<ExtensionRuntimeSet>> {
    let generation_survived = started.set.is_some();
    for diagnostic in started.diagnostics {
        let message = diagnostic.to_string();
        diagnostics.push(if generation_survived {
            AgentSessionRuntimeDiagnostic::warning(message)
        } else {
            AgentSessionRuntimeDiagnostic::error(message)
        });
    }
    started.set
}

async fn start_extension_phase(
    loader: &DefaultResourceLoader,
    discovery: ResourceDiscoveryPolicy,
    cwd: &Path,
    model_runtime: &ModelRuntime,
    project_trusted: bool,
    diagnostics: &mut Vec<AgentSessionRuntimeDiagnostic>,
) -> (
    Option<Arc<ExtensionRuntimeSet>>,
    BTreeMap<String, ExtensionFlagType>,
) {
    if discovery.disables(ResourceDiscoveryPolicy::EXTENSIONS) {
        return (None, BTreeMap::new());
    }
    let extension_infos = loader.get_extensions().paths.clone();
    let paths = extension_infos
        .iter()
        .map(|info| {
            if info.resolved_path.is_empty() {
                info.path.clone()
            } else {
                info.resolved_path.clone()
            }
        })
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return (None, BTreeMap::new());
    }
    let started =
        ExtensionRuntimeSet::start(paths, cwd.to_string_lossy().into_owned(), project_trusted)
            .await;
    let Some(runner) = record_extension_start(started, diagnostics) else {
        return (None, BTreeMap::new());
    };
    runner.set_command_source_infos(command_source_info_entries(extension_infos));
    for (path, outcome) in runner.register_providers_on(model_runtime) {
        if let Err(error) = outcome {
            diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
                "Extension \"{path}\" error: {error}"
            )));
        }
    }
    let flags = runner.registered_flag_types();
    (Some(runner), flags)
}

async fn apply_flags_to_runner(
    runner: &ExtensionRuntimeSet,
    applied_flags: &BTreeMap<String, ExtensionFlagValue>,
) -> Result<Vec<ExtensionSetDiagnostic>, pi_ext::client::HostClientError> {
    let values = applied_flags
        .iter()
        .map(|(name, value)| {
            let value = match value {
                ExtensionFlagValue::Bool(value) => FlagValueWire::Boolean(*value),
                ExtensionFlagValue::Str(value) => FlagValueWire::String(value.clone()),
            };
            (name.clone(), value)
        })
        .collect();
    runner.apply_flag_values(&values).await
}

/// Build the `(path, source_info)` entries for
/// [`ExtensionRuntimeSet::set_command_source_infos`].
///
/// An empty `resolved_path` (the loader falls back to `info.path` for
/// execution) would collide every unresolved extension onto one `""` key —
/// last silently wins. Skip it, and skip the duplicate entry when
/// `path == resolved_path`.
fn command_source_info_entries(
    infos: Vec<ExtensionPathInfo>,
) -> impl Iterator<Item = (String, SourceInfo)> {
    infos.into_iter().flat_map(|info| {
        let source = info.source_info;
        let resolved = (!info.resolved_path.is_empty() && info.resolved_path != info.path)
            .then(|| (info.resolved_path, source.clone()));
        [(info.path, source)].into_iter().chain(resolved)
    })
}

/// Validate and apply extension CLI flag values.
///
/// Unknown flags produce a single error diagnostic with the exact
/// `Unknown option[s]: --a, --b` wording. Boolean flags preserve the
/// provided boolean value (including `false`); a string value for a
/// declared boolean flag is rejected with a diagnostic and not applied.
/// String flags require a string value.
#[must_use]
pub fn apply_extension_flag_values(
    extension_flag_values: BTreeMap<String, ExtensionFlagValue>,
    registered_flags: &BTreeMap<String, ExtensionFlagType>,
) -> (
    Vec<AgentSessionRuntimeDiagnostic>,
    BTreeMap<String, ExtensionFlagValue>,
) {
    if extension_flag_values.is_empty() {
        return (Vec::new(), BTreeMap::new());
    }

    let mut diagnostics = Vec::new();
    let mut applied = BTreeMap::new();
    let mut unknown_flags = Vec::new();

    for (name, value) in extension_flag_values {
        let Some(flag_type) = registered_flags.get(&name) else {
            unknown_flags.push(name);
            continue;
        };
        match flag_type {
            ExtensionFlagType::Boolean => match value {
                ExtensionFlagValue::Bool(b) => {
                    applied.insert(name, ExtensionFlagValue::Bool(b));
                }
                ExtensionFlagValue::Str(_) => {
                    diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
                        "Extension flag \"--{name}\" does not take a value"
                    )));
                }
            },
            ExtensionFlagType::String => match value {
                ExtensionFlagValue::Str(text) => {
                    applied.insert(name, ExtensionFlagValue::Str(text));
                }
                ExtensionFlagValue::Bool(_) => {
                    diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
                        "Extension flag \"--{name}\" requires a value"
                    )));
                }
            },
        }
    }

    if !unknown_flags.is_empty() {
        let plural = if unknown_flags.len() == 1 { "" } else { "s" };
        let list = unknown_flags
            .iter()
            .map(|name| format!("--{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        diagnostics.push(AgentSessionRuntimeDiagnostic::error(format!(
            "Unknown option{plural}: {list}"
        )));
    }

    (diagnostics, applied)
}

/// Find the initial model based on priority:
/// 1. CLI provider+model (caller resolves and passes via `cli_model`)
/// 2. First scoped model when not continuing
/// 3. Saved default from settings when auth is configured
/// 4. First available default-per-provider match, else first available
/// 5. None
pub async fn find_initial_model(options: FindInitialModelOptions<'_>) -> InitialModelResult {
    if let Some(model) = options.cli_model {
        return InitialModelResult {
            model: Some(model.clone()),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        };
    }

    if !options.scoped_models.is_empty() && !options.is_continuing {
        let first = &options.scoped_models[0];
        return InitialModelResult {
            model: Some(first.model.clone()),
            thinking_level: first.thinking_level.unwrap_or(
                options
                    .default_thinking_level
                    .unwrap_or(DEFAULT_THINKING_LEVEL),
            ),
            fallback_message: None,
        };
    }

    if let (Some(provider), Some(model_id)) = (options.default_provider, options.default_model_id)
        && let Some(found) = options.model_runtime.get_model(provider, model_id)
        && options.model_runtime.has_configured_auth(&found.provider)
    {
        return InitialModelResult {
            model: Some(found),
            thinking_level: options
                .default_thinking_level
                .unwrap_or(DEFAULT_THINKING_LEVEL),
            fallback_message: None,
        };
    }

    let available = options
        .model_runtime
        .get_available(None)
        .await
        .unwrap_or_default();
    if let Some(model) = pick_default_available(&available) {
        return InitialModelResult {
            model: Some(model),
            thinking_level: DEFAULT_THINKING_LEVEL,
            fallback_message: None,
        };
    }

    InitialModelResult {
        model: None,
        thinking_level: DEFAULT_THINKING_LEVEL,
        fallback_message: None,
    }
}

/// Options for [`find_initial_model`].
pub struct FindInitialModelOptions<'a> {
    /// Pre-resolved CLI model (highest priority).
    pub cli_model: Option<&'a Model>,
    /// Scoped models for cycling.
    pub scoped_models: &'a [ScopedModel],
    /// Whether a session is being continued/resumed.
    pub is_continuing: bool,
    /// Settings default provider.
    pub default_provider: Option<&'a str>,
    /// Settings default model id.
    pub default_model_id: Option<&'a str>,
    /// Settings default thinking level.
    pub default_thinking_level: Option<ModelThinkingLevel>,
    /// Model runtime used for lookup/availability.
    pub model_runtime: &'a ModelRuntime,
}

/// Restore a saved session model, falling back to `current_model` or available models.
pub async fn restore_model_from_session(
    saved_provider: &str,
    saved_model_id: &str,
    current_model: Option<&Model>,
    model_runtime: &ModelRuntime,
) -> (Option<Model>, Option<String>) {
    let restored = model_runtime.get_model(saved_provider, saved_model_id);
    let has_configured_auth = restored
        .as_ref()
        .is_some_and(|model| model_runtime.has_configured_auth(&model.provider));

    if has_configured_auth && let Some(model) = restored {
        return (Some(model), None);
    }

    let reason = if restored.is_none() {
        "model no longer exists"
    } else {
        "no auth configured"
    };

    if let Some(current) = current_model {
        return (
            Some(current.clone()),
            Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                current.provider, current.id
            )),
        );
    }

    let available = model_runtime.get_available(None).await.unwrap_or_default();
    if let Some(fallback) = pick_default_available(&available) {
        return (
            Some(fallback.clone()),
            Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                fallback.provider, fallback.id
            )),
        );
    }

    (None, None)
}

/// Create an [`AgentSession`](crate::core::agent_session::AgentSession) package from previously created services.
///
/// Resolves model restore / initial selection, thinking level, and tool names.
/// The live `AgentSession` constructor is owned by a sibling module; this
/// returns the fully resolved inputs plus fallback message.
///
/// # Errors
///
/// Currently infallible for the resolution path; the `Result` form matches the
/// TypeScript promise API and leaves room for constructor failures once wired.
pub async fn create_agent_session_from_services(
    options: CreateAgentSessionFromServicesOptions,
) -> Result<CreateAgentSessionResult, AgentSessionServicesError> {
    let CreateAgentSessionFromServicesOptions {
        services,
        model: explicit_model,
        thinking_level: explicit_thinking,
        scoped_models,
        tools,
        exclude_tools,
        no_tools,
        session_start_event,
        saved_session_model,
        has_existing_session,
    } = options;

    let (model, model_fallback_message) = resolve_session_model(
        &services,
        explicit_model,
        &scoped_models,
        saved_session_model.as_ref(),
        has_existing_session,
    )
    .await;
    let mut thinking_level = explicit_thinking.unwrap_or_else(|| {
        services
            .settings_manager
            .get_default_thinking_level()
            .unwrap_or(DEFAULT_THINKING_LEVEL)
    });
    if model.is_none() {
        thinking_level = ModelThinkingLevel::Off;
    }
    let (initial_active_tool_names, allowed_tool_names) =
        resolve_session_tools(tools, exclude_tools.as_deref(), no_tools);
    let diagnostics = services.diagnostics.clone();

    Ok(CreateAgentSessionResult {
        model,
        thinking_level,
        initial_active_tool_names,
        allowed_tool_names,
        excluded_tool_names: exclude_tools,
        scoped_models,
        model_fallback_message,
        diagnostics,
        cwd: services.cwd,
        agent_dir: services.agent_dir,
        model_runtime: services.model_runtime,
        settings_manager: services.settings_manager,
        resource_loader: services.resource_loader,
        extension_runner: services.extension_runner,
        session_start_event,
    })
}

async fn resolve_session_model(
    services: &AgentSessionServices,
    explicit_model: Option<Model>,
    scoped_models: &[ScopedModel],
    saved_session_model: Option<&(String, String)>,
    has_existing_session: bool,
) -> (Option<Model>, Option<String>) {
    let mut model = explicit_model;
    let mut fallback = None;
    if model.is_none()
        && has_existing_session
        && let Some((provider, model_id)) = saved_session_model
    {
        let restored = services.model_runtime.get_model(provider, model_id);
        if let Some(found) = restored
            && services.model_runtime.has_configured_auth(&found.provider)
        {
            model = Some(found);
        } else {
            fallback = Some(format!("Could not restore model {provider}/{model_id}"));
        }
    }
    if model.is_none() {
        let selected = find_initial_model(FindInitialModelOptions {
            cli_model: None,
            scoped_models,
            is_continuing: has_existing_session,
            default_provider: services.settings_manager.get_default_provider().as_deref(),
            default_model_id: services.settings_manager.get_default_model().as_deref(),
            default_thinking_level: services.settings_manager.get_default_thinking_level(),
            model_runtime: &services.model_runtime,
        })
        .await
        .model;
        match (selected.as_ref(), fallback.as_mut()) {
            (None, _) => fallback = Some(format_no_models_available_message()),
            (Some(selected), Some(existing)) => {
                let _ = write!(existing, ". Using {}/{}", selected.provider, selected.id);
            }
            (Some(_), None) => {}
        }
        model = selected;
    }
    (model, fallback)
}

fn resolve_session_tools(
    tools: Option<Vec<String>>,
    exclude_tools: Option<&[String]>,
    no_tools: Option<NoToolsMode>,
) -> (Vec<String>, Option<Vec<String>>) {
    let allowed = match (&tools, no_tools) {
        (Some(tools), _) => Some(tools.clone()),
        (None, Some(NoToolsMode::All)) => Some(Vec::new()),
        (None, Some(NoToolsMode::Builtin) | None) => None,
    };
    let excluded = exclude_tools.map(|names| names.iter().cloned().collect::<BTreeSet<_>>());
    let active = if let Some(tools) = tools {
        tools
            .into_iter()
            .filter(|name| excluded.as_ref().is_none_or(|set| !set.contains(name)))
            .collect()
    } else if no_tools.is_some() {
        Vec::new()
    } else {
        ["read", "bash", "edit", "write"]
            .into_iter()
            .filter(|name| excluded.as_ref().is_none_or(|set| !set.contains(*name)))
            .map(str::to_owned)
            .collect()
    };
    (active, allowed)
}

fn pick_default_available(available: &[Model]) -> Option<Model> {
    for (provider, default_id) in default_model_per_provider() {
        if let Some(match_model) = available
            .iter()
            .find(|model| model.provider == *provider && model.id == *default_id)
        {
            return Some(match_model.clone());
        }
        // Keep KnownProvider parse reachable so renames fail tests.
        let _ = KnownProvider::from_id(provider);
    }
    available.first().cloned()
}

/// Resolve path helper re-export for tests.
#[must_use]
pub fn resolve_service_path(path: impl AsRef<Path>) -> PathBuf {
    resolve_path(path.as_ref().to_string_lossy().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::auth::InMemoryCredentialStore;
    use pi_ai::models_store::InMemoryModelsStore;
    use pi_ai::types::{ModelCost, ModelInput};
    use std::io;

    use crate::core::model_runtime::{
        CreateModelRuntimeOptions, ModelsJsonConfig, ProviderModelDefinition,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn required<T>(value: Option<T>, context: &'static str) -> io::Result<T> {
        value.ok_or_else(|| io::Error::other(context))
    }

    async fn runtime_with_env_openai() -> TestResult<ModelRuntime> {
        let mut env = pi_ai::auth::ProviderEnv::new();
        env.insert("OPENAI_API_KEY".to_owned(), "sk-test".to_owned());
        Ok(ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(Arc::new(InMemoryCredentialStore::new())),
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
            models_config: Some(ModelsJsonConfig::empty()),
            allow_model_network: Some(false),
            auth_env: Some(env),
            ..CreateModelRuntimeOptions::default()
        })
        .await?)
    }

    #[test]
    fn auth_guidance_strings_match_typescript() {
        let help = get_provider_login_help();
        assert!(help.contains("Use /login to log in to a provider via OAuth or API key."));
        assert!(help.contains("providers.md"));
        assert!(help.contains("models.md"));

        let no_models = format_no_models_available_message();
        assert!(no_models.starts_with("No models available. "));
        assert!(no_models.contains(&help));

        let no_selected = format_no_model_selected_message();
        assert!(no_selected.starts_with("No model selected."));
        assert!(no_selected.contains("Then use /model to select a model."));

        let no_key = format_no_api_key_found_message("anthropic");
        assert_eq!(no_key, format!("No API key found for anthropic.\n\n{help}"));
        let unknown = format_no_api_key_found_message("unknown");
        assert!(unknown.contains("the selected model"));

        let oauth = format_oauth_auth_failed_message("openai-codex");
        assert_eq!(
            oauth,
            "Authentication failed for \"openai-codex\". Credentials may have expired or network is unavailable. Run '/login openai-codex' to re-authenticate."
        );
    }

    #[test]
    fn extension_flag_validation_unknown_and_string_required() {
        let mut flags = BTreeMap::new();
        flags.insert("verbose".to_owned(), ExtensionFlagValue::Bool(true));
        flags.insert("mode".to_owned(), ExtensionFlagValue::Bool(true));
        flags.insert("unknown".to_owned(), ExtensionFlagValue::Str("x".into()));

        let mut registered = BTreeMap::new();
        registered.insert("verbose".to_owned(), ExtensionFlagType::Boolean);
        registered.insert("mode".to_owned(), ExtensionFlagType::String);

        let (diagnostics, applied) = apply_extension_flag_values(flags, &registered);
        assert!(applied.contains_key("verbose"));
        assert!(!applied.contains_key("mode"));
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "Extension flag \"--mode\" requires a value")
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "Unknown option: --unknown")
        );
    }

    #[test]
    fn extension_flag_validation_plural_unknown_options() {
        let mut flags = BTreeMap::new();
        flags.insert("a".to_owned(), ExtensionFlagValue::Bool(true));
        flags.insert("b".to_owned(), ExtensionFlagValue::Str("1".into()));
        let (diagnostics, _) = apply_extension_flag_values(flags, &BTreeMap::new());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "Unknown options: --a, --b");
    }

    #[tokio::test]
    async fn services_creation_order_registers_pending_providers() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(Arc::new(InMemoryCredentialStore::new())),
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
            models_config: Some(ModelsJsonConfig::empty()),
            allow_model_network: Some(false),
            ..CreateModelRuntimeOptions::default()
        })
        .await?;

        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent.clone()),
            model_runtime: Some(runtime),
            pending_provider_registrations: vec![PendingProviderRegistration {
                name: "acme".to_owned(),
                config: ProviderConfigInput {
                    base_url: Some("https://acme.test/v1".into()),
                    api: Some("openai-completions".into()),
                    api_key: Some("sk-acme".into()),
                    models: Some(vec![ProviderModelDefinition {
                        id: "acme-1".into(),
                        name: Some("Acme 1".into()),
                        api: Some("openai-completions".into()),
                        base_url: Some("https://acme.test/v1".into()),
                        reasoning: false,
                        thinking_level_map: None,
                        input: Some(vec![ModelInput::Text]),
                        cost: Some(ModelCost::default()),
                        context_window: Some(8_000),
                        max_tokens: Some(1_024),
                        headers: None,
                        compat: None,
                    }]),
                    ..ProviderConfigInput::default()
                },
                extension_path: "/ext/acme.ts".into(),
            }],
            registered_extension_flags: BTreeMap::new(),
            extension_flag_values: None,
            settings_manager: None,
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
        })
        .await?;

        assert!(services.model_runtime.get_model("acme", "acme-1").is_some());
        assert!(services.model_runtime.has_configured_auth("acme"));
        assert!(services.diagnostics.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn pending_provider_failure_becomes_diagnostic() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create_in_memory().await?;
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd,
            agent_dir: Some(agent),
            model_runtime: Some(runtime),
            pending_provider_registrations: vec![PendingProviderRegistration {
                name: "broken".into(),
                config: ProviderConfigInput {
                    models: Some(vec![ProviderModelDefinition {
                        id: "m".into(),
                        name: None,
                        api: None,
                        base_url: None,
                        reasoning: false,
                        thinking_level_map: None,
                        input: None,
                        cost: None,
                        context_window: None,
                        max_tokens: None,
                        headers: None,
                        compat: None,
                    }]),
                    ..ProviderConfigInput::default()
                },
                extension_path: "/ext/broken.ts".into(),
            }],
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        assert_eq!(services.diagnostics.len(), 1);
        assert!(
            services.diagnostics[0]
                .message
                .starts_with("Extension \"/ext/broken.ts\" error:")
        );
        Ok(())
    }

    #[tokio::test]
    async fn find_initial_model_priority_settings_then_available() -> TestResult {
        let runtime = runtime_with_env_openai().await?;
        let openai_default = required(
            runtime.get_model("openai", "gpt-5.5"),
            "OpenAI default must exist in the built-in catalog",
        )?;

        // Settings default with configured auth wins over first-available scan.
        let result = find_initial_model(FindInitialModelOptions {
            cli_model: None,
            scoped_models: &[],
            is_continuing: false,
            default_provider: Some("openai"),
            default_model_id: Some("gpt-5.5"),
            default_thinking_level: Some(ModelThinkingLevel::High),
            model_runtime: &runtime,
        })
        .await;
        assert_eq!(
            result.model.as_ref().map(|m| m.id.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(result.thinking_level, ModelThinkingLevel::High);

        // CLI model wins over settings.
        let cli = openai_default.clone();
        let result = find_initial_model(FindInitialModelOptions {
            cli_model: Some(&cli),
            scoped_models: &[],
            is_continuing: false,
            default_provider: Some("openai"),
            default_model_id: Some("other"),
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await;
        assert_eq!(
            result.model.as_ref().map(|m| m.id.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(result.thinking_level, DEFAULT_THINKING_LEVEL);
        Ok(())
    }

    #[tokio::test]
    async fn find_initial_model_scoped_skipped_when_continuing() -> TestResult {
        let runtime = runtime_with_env_openai().await?;
        let model = required(
            runtime.get_model("openai", "gpt-5.5"),
            "OpenAI test model must exist in the built-in catalog",
        )?;
        let scoped = vec![ScopedModel {
            model: model.clone(),
            thinking_level: Some(ModelThinkingLevel::Low),
        }];

        let continuing = find_initial_model(FindInitialModelOptions {
            cli_model: None,
            scoped_models: &scoped,
            is_continuing: true,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await;
        // Continues into available-default path (openai default with env auth).
        assert_eq!(
            continuing.model.as_ref().map(|m| m.id.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(continuing.thinking_level, DEFAULT_THINKING_LEVEL);

        let fresh = find_initial_model(FindInitialModelOptions {
            cli_model: None,
            scoped_models: &scoped,
            is_continuing: false,
            default_provider: None,
            default_model_id: None,
            default_thinking_level: None,
            model_runtime: &runtime,
        })
        .await;
        assert_eq!(fresh.thinking_level, ModelThinkingLevel::Low);
        Ok(())
    }

    #[tokio::test]
    async fn restore_model_fallback_message() -> TestResult {
        let runtime = runtime_with_env_openai().await?;
        let current = required(
            runtime.get_model("openai", "gpt-5.5"),
            "OpenAI fallback model must exist in the built-in catalog",
        )?;
        let (model, message) =
            restore_model_from_session("missing", "gone", Some(&current), &runtime).await;
        assert_eq!(model.as_ref().map(|m| m.id.as_str()), Some("gpt-5.5"));
        assert_eq!(
            message.as_deref(),
            Some(
                "Could not restore model missing/gone (model no longer exists). Using openai/gpt-5.5."
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn create_session_from_services_tool_resolution_and_fallback() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create_in_memory().await?;
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd,
            agent_dir: Some(agent),
            model_runtime: Some(runtime),
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        let result = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
            services,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: Some(vec!["bash".into()]),
            no_tools: None,
            session_start_event: None,
            saved_session_model: None,
            has_existing_session: false,
        })
        .await?;

        assert!(result.model.is_none());
        assert_eq!(
            result.model_fallback_message.as_deref(),
            Some(format_no_models_available_message().as_str())
        );
        assert_eq!(
            result.initial_active_tool_names,
            vec!["read".to_owned(), "edit".to_owned(), "write".to_owned()]
        );
        assert_eq!(result.thinking_level, ModelThinkingLevel::Off);
        assert!(result.allowed_tool_names.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn from_services_forwards_session_start_event() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create_in_memory().await?;
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd,
            agent_dir: Some(agent),
            model_runtime: Some(runtime),
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        let event = crate::core::agent_session::SessionStartEvent {
            reason: crate::core::agent_session::SessionStartReason::New,
            previous_session_file: Some("prev.jsonl".to_owned()),
        };
        let result = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
            services,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: None,
            no_tools: None,
            session_start_event: Some(event.clone()),
            saved_session_model: None,
            has_existing_session: false,
        })
        .await?;

        assert_eq!(
            result.session_start_event,
            Some(event),
            "replacement session-start metadata must survive services resolution"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_tools_all_sets_empty_allowlist_builtin_leaves_none() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create_in_memory().await?;
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent.clone()),
            model_runtime: Some(runtime.clone()),
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        let all = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
            services,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: None,
            no_tools: Some(NoToolsMode::All),
            session_start_event: None,
            saved_session_model: None,
            has_existing_session: false,
        })
        .await?;
        assert_eq!(all.allowed_tool_names, Some(Vec::new()));
        assert!(all.initial_active_tool_names.is_empty());

        let runtime = ModelRuntime::create_in_memory().await?;
        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd,
            agent_dir: Some(agent),
            model_runtime: Some(runtime),
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        let builtin = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
            services,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: None,
            no_tools: Some(NoToolsMode::Builtin),
            session_start_event: None,
            saved_session_model: None,
            has_existing_session: false,
        })
        .await?;
        assert!(builtin.allowed_tool_names.is_none());
        assert!(builtin.initial_active_tool_names.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn trust_override_precedes_project_settings_load() -> TestResult {
        for (trust_override, trusted) in [(None, false), (Some(false), false), (Some(true), true)] {
            let dir = tempfile::tempdir()?;
            let cwd = dir.path().join("project");
            let agent = dir.path().join("agent");
            std::fs::create_dir_all(cwd.join(".pi"))?;
            std::fs::create_dir_all(&agent)?;
            std::fs::write(agent.join("settings.json"), r#"{"theme":"global"}"#)?;
            std::fs::write(
                cwd.join(".pi").join("settings.json"),
                r#"{"theme":"project"}"#,
            )?;

            let services = create_agent_session_services_with_trust(
                CreateAgentSessionServicesOptions {
                    cwd,
                    agent_dir: Some(agent),
                    model_runtime: Some(ModelRuntime::create_in_memory().await?),
                    resource_loader_options: Some(ResourceLoaderServiceOptions {
                        no_extensions: true,
                        no_skills: true,
                        no_prompt_templates: true,
                        no_themes: true,
                        no_context_files: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                trust_override,
            )
            .await?;

            assert_eq!(services.settings_manager.is_project_trusted(), trusted);
            assert_eq!(
                services.settings_manager.get_theme().as_deref(),
                Some(if trusted { "project" } else { "global" })
            );
        }
        Ok(())
    }

    #[test]
    fn runtime_feedback_extension_start_diagnostics_follow_generation_survival() {
        let diagnostic = super::super::extension_runtime_set::ExtensionSetDiagnostic {
            path: "broken.ts".to_owned(),
            message: "load failed".to_owned(),
        };
        let surviving = ExtensionRuntimeSet::bind(Vec::new());
        let mut diagnostics = Vec::new();
        let returned = record_extension_start(
            ExtensionSetStart {
                set: Some(Arc::clone(&surviving)),
                diagnostics: vec![diagnostic.clone()],
            },
            &mut diagnostics,
        );
        assert!(returned.is_some());
        assert_eq!(
            diagnostics,
            vec![AgentSessionRuntimeDiagnostic::warning(
                "Extension \"broken.ts\" error: load failed"
            )]
        );

        diagnostics.clear();
        let returned = record_extension_start(
            ExtensionSetStart {
                set: None,
                diagnostics: vec![diagnostic],
            },
            &mut diagnostics,
        );
        assert!(returned.is_none());
        assert_eq!(
            diagnostics,
            vec![AgentSessionRuntimeDiagnostic::error(
                "Extension \"broken.ts\" error: load failed"
            )]
        );
    }

    #[test]
    fn command_source_info_entries_skips_empty_and_duplicate_resolved_path() {
        use crate::core::resources::{ExtensionPathInfo, SourceInfo, SourceOrigin, SourceScope};

        fn src(path: &str) -> SourceInfo {
            SourceInfo {
                path: path.to_owned(),
                source: "local".to_owned(),
                scope: SourceScope::Temporary,
                origin: SourceOrigin::TopLevel,
                base_dir: None,
            }
        }

        // Two extensions with empty resolved_path: without the fix both
        // write ("", source) and collide — last silently wins. With the fix
        // each keeps its own path key.
        let infos = vec![
            ExtensionPathInfo {
                path: "/ext/a".to_owned(),
                resolved_path: String::new(),
                source_info: src("/ext/a"),
            },
            ExtensionPathInfo {
                path: "/ext/b".to_owned(),
                resolved_path: String::new(),
                source_info: src("/ext/b"),
            },
        ];
        let map: std::collections::HashMap<String, SourceInfo> =
            command_source_info_entries(infos).collect();
        assert_eq!(
            map.get("/ext/a").map(|info| info.path.as_str()),
            Some("/ext/a")
        );
        assert_eq!(
            map.get("/ext/b").map(|info| info.path.as_str()),
            Some("/ext/b")
        );
        assert!(!map.contains_key(""), "no empty-string collision key");

        // path == resolved_path: only one entry, not a duplicate.
        let infos = vec![ExtensionPathInfo {
            path: "/ext/c".to_owned(),
            resolved_path: "/ext/c".to_owned(),
            source_info: src("/ext/c"),
        }];
        let map: std::collections::HashMap<String, SourceInfo> =
            command_source_info_entries(infos).collect();
        assert_eq!(
            map.len(),
            1,
            "duplicate path==resolved must produce one entry"
        );

        // Distinct path and resolved_path: two entries.
        let infos = vec![ExtensionPathInfo {
            path: "/ext/d".to_owned(),
            resolved_path: "/resolved/d".to_owned(),
            source_info: src("/ext/d"),
        }];
        let map: std::collections::HashMap<String, SourceInfo> =
            command_source_info_entries(infos).collect();
        assert_eq!(
            map.len(),
            2,
            "distinct path+resolved must produce two entries"
        );
        assert!(map.contains_key("/ext/d"));
        assert!(map.contains_key("/resolved/d"));
    }
    #[tokio::test]
    async fn result_settings_manager_is_the_services_manager_not_a_reconstruction() -> TestResult {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().join("project");
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent)?;

        let runtime = ModelRuntime::create_in_memory().await?;
        let mut services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd,
            agent_dir: Some(agent),
            model_runtime: Some(runtime),
            resource_loader_options: Some(ResourceLoaderServiceOptions {
                no_extensions: true,
                no_skills: true,
                no_prompt_templates: true,
                no_themes: true,
                no_context_files: true,
                ..ResourceLoaderServiceOptions::default()
            }),
            ..CreateAgentSessionServicesOptions::default()
        })
        .await?;

        // Apply a NON-PERSISTED in-memory override. A freshly reconstructed
        // manager (the old duplicate-owner bug) reads only disk and cannot
        // observe this value; the single-owner manager carried from services
        // can.
        let mut overrides = serde_json::Map::new();
        overrides.insert(
            "theme".to_owned(),
            serde_json::Value::String("in-memory-sentinel".to_owned()),
        );
        services.settings_manager.apply_overrides(&overrides);

        let result = create_agent_session_from_services(CreateAgentSessionFromServicesOptions {
            services,
            model: None,
            thinking_level: None,
            scoped_models: Vec::new(),
            tools: None,
            exclude_tools: None,
            no_tools: None,
            session_start_event: None,
            saved_session_model: None,
            has_existing_session: false,
        })
        .await?;

        assert_eq!(
            result.settings_manager.get_theme().as_deref(),
            Some("in-memory-sentinel"),
            "result must carry the services manager, not a disk-only reconstruction"
        );
        Ok(())
    }
}
