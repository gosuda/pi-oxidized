//! Process entry: `entry::run(args, io) -> ExitCode`.
//!
//! This is the substantive entry point that `main.rs` calls. It owns the tokio
//! runtime construction, drives the [`bootstrap::run_bootstrap`] pipeline, and
//! dispatches the resolved mode through [`modes::run::run_mode_default`].
//!
//! [`Io::real`] constructs the real production surface: a [`RuntimeFactory`]
//! backed by [`create_agent_session_services`] and [`create_agent_session_from_services`]
//! plus [`AgentSessionRuntime`], a [`PackageHandler`] backed by [`PackageManager`],
//! and a [`DefaultDispatcher`] with concrete print/json binding and
//! integrator-injected RPC/interactive closures.
//!
//! For tests, [`Io::custom`] accepts any combination of injected fakes so the
//! full pipeline runs without touching the real terminal, network, or
//! filesystem.
//!
//! The `main.rs` is one line:
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     pi::cli::entry::run(std::env::args().skip(1).collect(), pi::cli::entry::Io::real())
//! }
//! ```

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use pi_ai::{
    AssistantMessageEvent, Context, Model, ModelThinkingLevel, Provider, ProviderError,
    StreamOptions,
};

use crate::cli::bootstrap::{
    BootstrapInputs, BootstrapIo, BootstrapOutcome, RuntimeFactory, RuntimeFactoryOptions,
    RuntimeHandle, run_bootstrap,
};
use crate::cli::package_manager_cli::{ListedPackage, ListedScope, PackageHandler, PackageOutput};
use crate::core::agent_session_runtime::{
    AgentSessionRuntime, AgentSessionRuntimeServices, CreateAgentSessionRuntimeFactory,
    CreateAgentSessionRuntimeOptions, CreateAgentSessionRuntimeResult,
};
use crate::core::agent_session_services::{
    AgentSessionRuntimeDiagnostic, AgentSessionServices, AgentSessionServicesError,
    CreateAgentSessionResult, CreateAgentSessionServicesOptions, ExtensionFlagValue,
    create_agent_session_from_services, create_agent_session_services_with_trust,
};
use crate::core::config::get_agent_dir;
use crate::core::model_resolver::{
    ResolveCliModelOptions, ResolveCliModelResult, ResolveModelScopeResult, resolve_cli_model,
    resolve_model_scope_with_diagnostics,
};
use crate::core::model_runtime::ModelRuntime;
use crate::core::package_manager::{PackageManager, PackageManagerOptions, Scope};
use crate::core::resources::ResourceLoader;
use crate::core::settings::{SettingsManager, SettingsManagerCreateOptions};
use crate::core::system_prompt::{BuildSystemPromptOptions, build_system_prompt};
use crate::core::trust::{
    ProjectTrustStore, ResolveProjectTrustedOptions, resolve_project_trusted,
};
use crate::modes::interactive::runtime::{InteractiveRuntimeOptions, run_interactive_mode};
use crate::modes::rpc::server::run_rpc_mode;
use crate::modes::run::{DefaultDispatcher, run_mode_default};

/// Injectable process I/O and product runtime dependencies.
pub struct Io {
    /// Bootstrap environment and terminal operations.
    pub bootstrap_io: Arc<dyn BootstrapIo>,
    /// Runtime factory.
    pub factory: Arc<dyn RuntimeFactory>,
    /// Package-command handler.
    pub package_handler: Arc<dyn PackageHandler>,
    /// Package-command output sink.
    pub package_output: Arc<dyn PackageOutput>,
    /// Mode dispatcher (concrete so runners can be replaced).
    pub dispatcher: Arc<DefaultDispatcher>,
}

impl Io {
    /// Production wiring backed by real product services.
    ///
    /// Constructs a [`RuntimeFactory`] that chains
    /// `create_agent_session_services` → `create_agent_session_from_services` →
    /// `AgentSessionRuntime`, a [`PackageHandler`] backed by [`PackageManager`],
    /// and a [`DefaultDispatcher`] with concrete print/json binding.
    ///
    /// RPC and interactive closures are injected by the caller when those mode
    /// runners are available (the RPC server and interactive TUI are built by
    /// sibling slices). Use [`Io::with_rpc`] / [`Io::with_interactive`] to
    /// inject them.
    #[must_use]
    pub fn real() -> Self {
        let dispatcher = DefaultDispatcher::new()
            .with_interactive(|dispatched, runtime| {
                Box::pin(async move {
                    let _ = dispatched;
                    run_interactive_mode(runtime, InteractiveRuntimeOptions::detect())
                        .await
                        .map_err(|e| format!("interactive: {e}"))
                })
            })
            .with_rpc(|_dispatched, runtime| {
                Box::pin(async move {
                    let code = run_rpc_mode(runtime).await;
                    Ok(u8::try_from(code).unwrap_or(1))
                })
            });
        Self {
            bootstrap_io: Arc::new(RealBootstrapIo),
            factory: Arc::new(RealRuntimeFactory),
            package_handler: Arc::new(RealPackageHandler::new(false)),
            package_output: Arc::new(ProductOutputSink),
            dispatcher: Arc::new(dispatcher),
        }
    }

    /// Inject the RPC mode runner.
    #[must_use]
    pub fn with_rpc<F>(mut self, f: F) -> Self
    where
        F: Fn(
                crate::cli::bootstrap::Dispatched,
                Arc<AgentSessionRuntime>,
            ) -> BoxFuture<'static, Result<u8, String>>
            + Send
            + Sync
            + 'static,
    {
        let dispatcher = Arc::try_unwrap(self.dispatcher).unwrap_or_else(|arc| DefaultDispatcher {
            rpc: arc.rpc.clone(),
            interactive: arc.interactive.clone(),
        });
        self.dispatcher = Arc::new(dispatcher.with_rpc(f));
        self
    }

    /// Inject the interactive mode runner.
    #[must_use]
    pub fn with_interactive<F>(mut self, f: F) -> Self
    where
        F: Fn(
                crate::cli::bootstrap::Dispatched,
                Arc<AgentSessionRuntime>,
            ) -> BoxFuture<'static, Result<u8, String>>
            + Send
            + Sync
            + 'static,
    {
        let dispatcher = Arc::try_unwrap(self.dispatcher).unwrap_or_else(|arc| DefaultDispatcher {
            rpc: arc.rpc.clone(),
            interactive: arc.interactive.clone(),
        });
        self.dispatcher = Arc::new(dispatcher.with_interactive(f));
        self
    }

    /// Test/custom wiring with full injection.
    #[must_use]
    pub fn custom(
        bootstrap_io: Arc<dyn BootstrapIo>,
        factory: Arc<dyn RuntimeFactory>,
        package_handler: Arc<dyn PackageHandler>,
        package_output: Arc<dyn PackageOutput>,
        dispatcher: Arc<DefaultDispatcher>,
    ) -> Self {
        Self {
            bootstrap_io,
            factory,
            package_handler,
            package_output,
            dispatcher,
        }
    }
}

/// Entry point. Synchronous; constructs a tokio multi-thread runtime and
/// blocks on the bootstrap + dispatch pipeline.
#[must_use]
pub fn run(args: Vec<String>, io: Io) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            crate::core::output_guard::ProductOutput::writeln(&format!(
                "Error: failed to start runtime: {err}"
            ));
            return ExitCode::from(1);
        }
    };
    let result = runtime.block_on(async move { run_pipeline(args, &io).await });
    drop(runtime);
    result
}

/// Async pipeline: bootstrap → dispatch. Public so tests with their own tokio
/// runtime can drive it without the sync wrapper.
pub async fn run_pipeline(args: Vec<String>, io: &Io) -> ExitCode {
    let outcome = run_bootstrap(BootstrapInputs {
        args,
        io: io.bootstrap_io.as_ref(),
        factory: io.factory.as_ref(),
        package_handler: io.package_handler.as_ref(),
        package_output: io.package_output.as_ref(),
    })
    .await;

    match outcome {
        BootstrapOutcome::Exit { code, drain_quirk } => {
            let _ = drain_quirk;
            ExitCode::from(code)
        }
        BootstrapOutcome::Dispatch(dispatched) => {
            run_mode_default(dispatched, io.dispatcher.as_ref()).await
        }
    }
}

struct RealBootstrapIo;

impl BootstrapIo for RealBootstrapIo {
    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn set_env(&self, _key: &str, _value: &str) {
        // `unsafe_code = "forbid"` prevents `std::env::set_var`. Offline is
        // threaded via `PackageHandler::set_offline` and explicit offline
        // parameters; env-only readers are not used for --offline.
    }
    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
    fn stdin_is_tty(&self) -> bool {
        io::IsTerminal::is_terminal(&io::stdin())
    }
    fn stdout_is_tty(&self) -> bool {
        io::IsTerminal::is_terminal(&io::stdout())
    }
    fn read_piped_stdin<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<String>>> + Send + 'a>> {
        Box::pin(async move {
            if self.stdin_is_tty() {
                return Ok(None);
            }
            let mut buf = String::new();
            let stdin = io::stdin();
            let mut handle = stdin.lock();
            handle.read_to_string(&mut buf)?;
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        })
    }
    fn write_stdout(&self, line: &str) {
        crate::core::output_guard::ProductOutput::writeln(line);
    }
    fn write_stderr(&self, line: &str) {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(line.as_bytes());
        let _ = stderr.write_all(b"\n");
    }
}

#[derive(Clone)]
struct RuntimeProvider(ModelRuntime);

impl Provider for RuntimeProvider {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        self.0.stream_simple(model.clone(), context, options)
    }
}
fn build_builtin_tools(
    cwd: &Path,
    settings: &SettingsManager,
    model: Option<Model>,
) -> Vec<Arc<dyn pi_agent::AgentTool>> {
    use crate::core::tools::{
        bash::create_bash_tool,
        edit::EditTool,
        find::FindTool,
        grep::GrepTool,
        ls::LsTool,
        read::{ReadTool, ReadToolOptions},
        write::WriteTool,
    };

    vec![
        Arc::new(ReadTool::with_options(ReadToolOptions {
            cwd: cwd.to_path_buf(),
            auto_resize_images: settings.get_image_auto_resize(),
            model,
        })),
        create_bash_tool(cwd.to_path_buf()),
        Arc::new(EditTool::new(cwd)),
        Arc::new(WriteTool::new(cwd)),
        Arc::new(GrepTool::new(cwd)),
        Arc::new(FindTool::new(cwd)),
        Arc::new(LsTool::new(cwd)),
    ]
}

fn thinking_level_from_str(level: &str) -> Option<pi_ai::ModelThinkingLevel> {
    match level {
        "off" => Some(pi_ai::ModelThinkingLevel::Off),
        "minimal" => Some(pi_ai::ModelThinkingLevel::Minimal),
        "low" => Some(pi_ai::ModelThinkingLevel::Low),
        "medium" => Some(pi_ai::ModelThinkingLevel::Medium),
        "high" => Some(pi_ai::ModelThinkingLevel::High),
        "xhigh" => Some(pi_ai::ModelThinkingLevel::Xhigh),
        _ => None,
    }
}

fn thinking_level_token(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

fn append_session_bootstrap_entries(
    session_manager: &mut crate::core::sessions::SessionManager,
    messages: &[pi_agent::AgentMessage],
    model: Option<&Model>,
    thinking_level: ModelThinkingLevel,
) -> Result<(), String> {
    if messages.is_empty() {
        if let Some(model) = model {
            session_manager
                .append_model_change(&model.provider, &model.id)
                .map_err(|error| error.to_string())?;
        }
        session_manager
            .append_thinking_level_change(thinking_level_token(thinking_level))
            .map_err(|error| error.to_string())?;
        return Ok(());
    }

    if !session_manager.get_entries().iter().any(|entry| {
        matches!(
            entry,
            crate::core::sessions::SessionEntry::ThinkingLevelChange(_)
        )
    }) {
        session_manager
            .append_thinking_level_change(thinking_level_token(thinking_level))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct SessionBuildOptions {
    cwd: String,
    session_manager: crate::core::sessions::SessionManager,
    settings_manager: SettingsManager,
    session_result: CreateAgentSessionResult,
    tools: Vec<Arc<dyn pi_agent::AgentTool>>,
    messages: Vec<pi_agent::AgentMessage>,
    system_prompt: String,
    skills: Vec<crate::core::resources::skills::Skill>,
    prompt_templates: Vec<crate::core::resources::prompts::PromptTemplate>,
}

struct BuiltSession {
    session: Arc<crate::core::agent_session::AgentSession>,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    model_fallback_message: Option<String>,
}

fn build_session(options: SessionBuildOptions) -> Result<BuiltSession, String> {
    let session_result = options.session_result;
    let host_runner = session_result.extension_runner.clone();
    let trait_runner = host_runner
        .clone()
        .map(|runner| runner as Arc<dyn crate::core::agent_session::ExtensionRunner>);
    let mut session_manager = options.session_manager;
    append_session_bootstrap_entries(
        &mut session_manager,
        &options.messages,
        session_result.model.as_ref(),
        session_result.thinking_level,
    )?;
    let config = crate::core::agent_session::AgentSessionConfig {
        agent: None,
        provider: Some(Arc::new(RuntimeProvider(
            session_result.model_runtime.clone(),
        ))),
        session_manager,
        settings_manager: options.settings_manager,
        cwd: options.cwd,
        scoped_models: session_result
            .scoped_models
            .iter()
            .map(|scoped| crate::core::agent_session::ScopedModel {
                model: scoped.model.clone(),
                thinking_level: scoped.thinking_level,
            })
            .collect(),
        initial_active_tool_names: Some(session_result.initial_active_tool_names),
        allowed_tool_names: session_result.allowed_tool_names,
        excluded_tool_names: session_result.excluded_tool_names,
        model: session_result.model,
        thinking_level: session_result.thinking_level,
        system_prompt: options.system_prompt,
        tools: options.tools,
        messages: options.messages,
        extension_runner: trait_runner,
        host_extension_runner: host_runner,
        model_runtime: Some(Arc::new(session_result.model_runtime)),
        compaction_stream_override: None,
        skills: options.skills,
        prompt_templates: options.prompt_templates,
        resource_loader: Some(session_result.resource_loader),
        session_start_event: session_result.session_start_event,
        base_config: None,
    };
    let session =
        crate::core::agent_session::AgentSession::new(config).map_err(|error| error.to_string())?;
    Ok(BuiltSession {
        session,
        diagnostics: session_result.diagnostics,
        model_fallback_message: session_result.model_fallback_message,
    })
}

struct SessionResources {
    skills: Vec<crate::core::resources::skills::Skill>,
    prompt_templates: Vec<crate::core::resources::prompts::PromptTemplate>,
    context_files: Vec<crate::core::resources::AgentsFile>,
    custom_prompt: Option<String>,
    append_prompt: Option<String>,
}

fn session_resources(loader: &crate::core::resources::DefaultResourceLoader) -> SessionResources {
    SessionResources {
        skills: loader.get_skills().0.to_vec(),
        prompt_templates: loader.get_prompts().0.to_vec(),
        context_files: loader.get_agents_files().to_vec(),
        custom_prompt: loader.get_system_prompt().map(str::to_owned),
        append_prompt: (!loader.get_append_system_prompt().is_empty())
            .then(|| loader.get_append_system_prompt().join("\n\n")),
    }
}

struct RestoredSession {
    has_existing_session: bool,
    saved_session_model: Option<(String, String)>,
    saved_thinking_level: Option<pi_ai::ModelThinkingLevel>,
    messages: Vec<pi_agent::AgentMessage>,
}

fn restore_session(context: crate::core::sessions::SessionContext) -> RestoredSession {
    RestoredSession {
        has_existing_session: !context.messages.is_empty(),
        saved_session_model: context.model.map(|model| (model.provider, model.model_id)),
        saved_thinking_level: thinking_level_from_str(&context.thinking_level),
        messages: context.messages,
    }
}

fn extension_flag_values(args: &crate::cli::args::Args) -> BTreeMap<String, ExtensionFlagValue> {
    args.unknown_flags
        .iter()
        .map(|(key, value)| {
            let value = match value {
                crate::cli::args::FlagValue::Bool => ExtensionFlagValue::Bool(true),
                crate::cli::args::FlagValue::Str(value) => ExtensionFlagValue::Str(value.clone()),
            };
            (key.clone(), value)
        })
        .collect()
}

fn no_tools_mode(
    args: &crate::cli::args::Args,
) -> Option<crate::core::agent_session_services::NoToolsMode> {
    if args.no_tools {
        Some(crate::core::agent_session_services::NoToolsMode::All)
    } else if args.no_builtin_tools {
        Some(crate::core::agent_session_services::NoToolsMode::Builtin)
    } else {
        None
    }
}

#[derive(Clone)]
struct RuntimeServiceConfiguration {
    extension_flag_values: BTreeMap<String, ExtensionFlagValue>,
    resource_loader_options: crate::core::agent_session_services::ResourceLoaderServiceOptions,
    project_trust_override: Option<bool>,
}

impl RuntimeServiceConfiguration {
    fn from_args(args: &crate::cli::args::Args) -> Self {
        Self {
            extension_flag_values: extension_flag_values(args),
            project_trust_override: args.project_trust_override,
            resource_loader_options:
                crate::core::agent_session_services::ResourceLoaderServiceOptions {
                    no_extensions: args.no_extensions,
                    no_skills: args.no_skills,
                    no_prompt_templates: args.no_prompt_templates,
                    no_themes: args.no_themes,
                    no_context_files: args.no_context_files,
                    system_prompt: args.system_prompt.clone(),
                    append_system_prompt: (!args.append_system_prompt.is_empty())
                        .then(|| args.append_system_prompt.clone()),
                    additional_extension_paths: args.extensions.clone(),
                    ..Default::default()
                },
        }
    }
}

#[derive(Clone)]
struct ReplacementRuntimeConfiguration {
    service: RuntimeServiceConfiguration,
    api_key: Option<String>,
}

impl ReplacementRuntimeConfiguration {
    fn from_args(args: &crate::cli::args::Args) -> Self {
        Self {
            service: RuntimeServiceConfiguration::from_args(args),
            api_key: args.api_key.clone(),
        }
    }
}

async fn create_runtime_services(
    cwd: &str,
    agent_dir: &str,
    configuration: &RuntimeServiceConfiguration,
) -> Result<AgentSessionServices, String> {
    create_agent_session_services_with_trust(
        CreateAgentSessionServicesOptions {
            cwd: PathBuf::from(cwd),
            agent_dir: Some(PathBuf::from(agent_dir)),
            extension_flag_values: Some(configuration.extension_flag_values.clone()),
            resource_loader_options: Some(configuration.resource_loader_options.clone()),
            ..Default::default()
        },
        configuration.project_trust_override,
    )
    .await
    .map_err(|error| error.to_string())
}

struct ResolvedModels {
    cli: ResolveCliModelResult,
    scope: ResolveModelScopeResult,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

async fn resolve_models(args: &crate::cli::args::Args, runtime: &ModelRuntime) -> ResolvedModels {
    let cli = resolve_cli_model(ResolveCliModelOptions {
        cli_provider: args.provider.as_deref(),
        cli_model: args.model.as_deref(),
        cli_thinking: args.thinking,
        model_runtime: runtime,
    });
    let mut diagnostics = Vec::new();
    if let Some(error) = &cli.error {
        diagnostics.push(AgentSessionRuntimeDiagnostic::error(error.clone()));
    }
    if let Some(warning) = &cli.warning {
        diagnostics.push(AgentSessionRuntimeDiagnostic::warning(warning.clone()));
    }
    let scope = resolve_model_scope_with_diagnostics(&args.models, runtime).await;
    diagnostics.extend(
        scope
            .diagnostics
            .iter()
            .map(|diagnostic| AgentSessionRuntimeDiagnostic::warning(diagnostic.message.clone())),
    );
    ResolvedModels {
        cli,
        scope,
        diagnostics,
    }
}
async fn install_cli_api_key(
    api_key: &str,
    model: &Model,
    runtime: &ModelRuntime,
) -> Result<(), String> {
    runtime
        .set_runtime_api_key(&model.provider, api_key)
        .await
        .map_err(|error| error.to_string())?;
    runtime
        .get_available(None)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn apply_cli_api_key(
    api_key: Option<&str>,
    selected_model: Option<&Model>,
    runtime: &ModelRuntime,
    diagnostics: &mut Vec<AgentSessionRuntimeDiagnostic>,
) -> Result<(), String> {
    let Some(api_key) = api_key else {
        return Ok(());
    };
    let Some(model) = selected_model else {
        diagnostics.push(AgentSessionRuntimeDiagnostic::error(
            "--api-key requires a model to be specified via --model, --provider/--model, or --models",
        ));
        return Ok(());
    };

    install_cli_api_key(api_key, model, runtime).await
}

/// Build the settings manager, built-in tools, and base system prompt that
/// `build_session` consumes together. Pulled out of `RealRuntimeFactory::create`
/// to keep the entry pipeline under the strict `too_many_lines` ceiling.
fn build_session_inputs(
    cwd: &str,
    agent_dir: &str,
    project_trusted: bool,
    model: Option<Model>,
    initial_active_tool_names: Vec<String>,
    resources: &SessionResources,
) -> (SettingsManager, Vec<Arc<dyn pi_agent::AgentTool>>, String) {
    let settings_manager = SettingsManager::create(
        cwd,
        Some(agent_dir),
        SettingsManagerCreateOptions::default().project_trusted(project_trusted),
    );
    let tools = build_builtin_tools(Path::new(cwd), &settings_manager, model);
    let system_prompt = build_system_prompt(&BuildSystemPromptOptions {
        custom_prompt: resources.custom_prompt.clone(),
        selected_tools: Some(initial_active_tool_names),
        tool_snippets: None,
        prompt_guidelines: None,
        append: resources.append_prompt.clone(),
        cwd: cwd.to_owned(),
        context_files: Some(resources.context_files.clone()),
        skills: Some(resources.skills.clone()),
    });
    (settings_manager, tools, system_prompt)
}
/// Runtime factory backed by real product services.
struct RealRuntimeFactory;

impl RuntimeFactory for RealRuntimeFactory {
    fn create(
        &self,
        options: RuntimeFactoryOptions,
    ) -> BoxFuture<'_, Result<RuntimeHandle, String>> {
        Box::pin(async move {
            let cwd = options.cwd.clone();
            let agent_dir = options.agent_dir.clone();
            let parsed = options.parsed;
            let session_context = options
                .session_manager
                .build_session_context()
                .map_err(|error| error.to_string())?;
            let replacement_configuration = ReplacementRuntimeConfiguration::from_args(&parsed);
            let RestoredSession {
                has_existing_session,
                saved_session_model,
                saved_thinking_level,
                messages: existing_messages,
            } = restore_session(session_context);

            let services = create_runtime_services(&cwd, &agent_dir, &replacement_configuration.service).await?;
            let project_trusted = services.settings_manager().is_project_trusted();

            // Services refresh registers extension providers before model resolution.
            let ResolvedModels {
                cli: cli_resolved,
                scope,
                diagnostics: mut pre_session_diagnostics,
            } = resolve_models(&parsed, &services.model_runtime).await;

            let resources = session_resources(&services.resource_loader);
            let no_tools = no_tools_mode(&parsed);

            let thinking_level = parsed
                .thinking
                .or(cli_resolved.thinking_level)
                .or(saved_thinking_level);

            // Resolve model + tool config into a session result.
            let mut session_result = create_agent_session_from_services(
                crate::core::agent_session_services::CreateAgentSessionFromServicesOptions {
                    services,
                    model: cli_resolved.model.clone(),
                    thinking_level,
                    scoped_models: scope.scoped_models,
                    tools: if parsed.tools.is_empty() {
                        None
                    } else {
                        Some(parsed.tools.clone())
                    },
                    exclude_tools: if parsed.exclude_tools.is_empty() {
                        None
                    } else {
                        Some(parsed.exclude_tools.clone())
                    },
                    no_tools,
                    session_start_event: None,
                    saved_session_model,
                    has_existing_session,
                },
            )
            .await
            .map_err(|e: AgentSessionServicesError| format!("{e}"))?;
            apply_cli_api_key(
                parsed.api_key.as_deref(),
                session_result.model.as_ref(),
                &session_result.model_runtime,
                &mut pre_session_diagnostics,
            )
            .await?;
            session_result
                .diagnostics
                .splice(0..0, pre_session_diagnostics);

            let (settings_manager, tools, system_prompt) = build_session_inputs(
                &cwd,
                &agent_dir,
                project_trusted,
                session_result.model.clone(),
                session_result.initial_active_tool_names.clone(),
                &resources,
            );

            let built = build_session(SessionBuildOptions {
                cwd: cwd.clone(),
                session_manager: options.session_manager,
                settings_manager,
                session_result,
                tools,
                messages: existing_messages,
                system_prompt,
                skills: resources.skills,
                prompt_templates: resources.prompt_templates,
            })?;
            let runtime = AgentSessionRuntime::new(
                built.session,
                AgentSessionRuntimeServices {
                    cwd: PathBuf::from(&cwd),
                    agent_dir: PathBuf::from(&agent_dir),
                },
                Arc::new(RealReplacementFactory {
                    configuration: replacement_configuration,
                }),
                built.diagnostics,
                built.model_fallback_message,
            );

            Ok(RuntimeHandle {
                runtime: Arc::new(runtime),
            })
        })
    }

    fn supports_interactive(&self) -> bool {
        true
    }
}

/// Replacement factory for runtime swap operations (new/switch/fork).
#[derive(Clone)]
struct RealReplacementFactory {
    configuration: ReplacementRuntimeConfiguration,
}

impl CreateAgentSessionRuntimeFactory for RealReplacementFactory {
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<
        '_,
        Result<
            CreateAgentSessionRuntimeResult,
            crate::core::agent_session_runtime::AgentSessionRuntimeError,
        >,
    > {
        let cwd = options.cwd.clone();
        let agent_dir = options.agent_dir.clone();
        let session_context = options
            .session_manager
            .build_session_context()
            .map_err(|error| {
                crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory(
                    error.to_string(),
                )
            });
        Box::pin(async move {
            let session_context = session_context?;
            let RestoredSession {
                has_existing_session,
                saved_session_model,
                saved_thinking_level: thinking_level,
                messages: existing_messages,
            } = restore_session(session_context);
            let services = create_runtime_services(&cwd, &agent_dir, &self.configuration.service)
                .await
                .map_err(crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory)?;
            let project_trusted = services.settings_manager().is_project_trusted();
            let resources = session_resources(&services.resource_loader);
            let saved_model = saved_session_model
                .as_ref()
                .and_then(|(provider, model_id)| services.model_runtime.get_model(provider, model_id));
            let mut replacement_diagnostics = Vec::new();
            if let (Some(api_key), Some(saved_model)) =
                (self.configuration.api_key.as_deref(), saved_model.as_ref())
            {
                install_cli_api_key(api_key, saved_model, &services.model_runtime)
                    .await
                    .map_err(crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory)?;
            }

            let mut session_result = create_agent_session_from_services(
                crate::core::agent_session_services::CreateAgentSessionFromServicesOptions {
                    services,
                    model: None,
                    thinking_level,
                    scoped_models: Vec::new(),
                    tools: None,
                    exclude_tools: None,
                    no_tools: None,
                    session_start_event: Some(crate::core::agent_session::SessionStartEvent {
                        reason: options.start_reason,
                        previous_session_file: options.previous_session_file.clone(),
                    }),
                    saved_session_model,
                    has_existing_session,
                },
            )
            .await
            .map_err(|e| {
                crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory(format!(
                    "{e}"
                ))
            })?;

            apply_cli_api_key(
                self.configuration.api_key.as_deref(),
                session_result.model.as_ref(),
                &session_result.model_runtime,
                &mut replacement_diagnostics,
            )
            .await
            .map_err(crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory)?;
            session_result.diagnostics.extend(replacement_diagnostics);

            let built = assemble_replacement_session(
                &cwd,
                &agent_dir,
                project_trusted,
                options.session_manager,
                session_result,
                resources,
                existing_messages,
            )
            .map_err(crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory)?;

            Ok(CreateAgentSessionRuntimeResult {
                session: built.session,
                services: AgentSessionRuntimeServices {
                    cwd: PathBuf::from(&cwd),
                    agent_dir: PathBuf::from(&agent_dir),
                },
                diagnostics: built.diagnostics,
                model_fallback_message: built.model_fallback_message,
            })
        })
    }
}

/// Assemble the replacement session inputs (settings, tools, system prompt)
/// and build the session. Pulled out of [`RealReplacementFactory::create`] to
/// keep it under the strict `too_many_lines` ceiling.
fn assemble_replacement_session(
    cwd: &str,
    agent_dir: &str,
    project_trusted: bool,
    session_manager: crate::core::sessions::SessionManager,
    session_result: CreateAgentSessionResult,
    resources: SessionResources,
    existing_messages: Vec<pi_agent::AgentMessage>,
) -> Result<BuiltSession, String> {
    let SessionResources {
        skills,
        prompt_templates,
        context_files,
        custom_prompt,
        append_prompt,
    } = resources;
    let settings_manager = SettingsManager::create(
        cwd,
        Some(agent_dir),
        SettingsManagerCreateOptions::default().project_trusted(project_trusted),
    );
    let tools = build_builtin_tools(
        Path::new(cwd),
        &settings_manager,
        session_result.model.clone(),
    );
    let system_prompt = build_system_prompt(&BuildSystemPromptOptions {
        custom_prompt,
        selected_tools: Some(session_result.initial_active_tool_names.clone()),
        tool_snippets: None,
        prompt_guidelines: None,
        append: append_prompt,
        cwd: cwd.to_owned(),
        context_files: Some(context_files),
        skills: Some(skills.clone()),
    });
    build_session(SessionBuildOptions {
        cwd: cwd.to_owned(),
        session_manager,
        settings_manager,
        session_result,
        tools,
        messages: existing_messages,
        system_prompt,
        skills,
        prompt_templates,
    })
}

/// Package handler backed by real [`PackageManager`] + [`SettingsManager`].
struct RealPackageHandler {
    cwd: PathBuf,
    agent_dir: PathBuf,
    offline: std::sync::Mutex<bool>,
    project_trust_override: std::sync::Mutex<Option<bool>>,
}

impl RealPackageHandler {
    fn new(offline: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let agent_dir = get_agent_dir();
        Self {
            cwd,
            agent_dir,
            offline: std::sync::Mutex::new(offline),
            project_trust_override: std::sync::Mutex::new(None),
        }
    }

    fn offline(&self) -> bool {
        *self
            .offline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn build_settings(&self, trusted: bool) -> SettingsManager {
        SettingsManager::create(
            &self.cwd,
            Some(&self.agent_dir),
            SettingsManagerCreateOptions::default().project_trusted(trusted),
        )
    }

    fn resolved_project_trusted(&self) -> bool {
        let global_settings = self.build_settings(false);
        let trust_override = *self
            .project_trust_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: self.cwd.clone(),
            trust_store: &ProjectTrustStore::new(&self.agent_dir),
            trust_override,
            default_project_trust: global_settings.get_default_project_trust(),
            extension_hook: None,
            ui: None,
            on_extension_error: None,
        })
        .unwrap_or(false)
    }

    fn build_package_manager(&self) -> PackageManager {
        PackageManager::with_offline(
            PackageManager::new(PackageManagerOptions::new(&self.cwd, &self.agent_dir)),
            self.offline(),
        )
    }

    fn scope(local: bool) -> Scope {
        if local { Scope::Project } else { Scope::User }
    }
}

impl PackageHandler for RealPackageHandler {
    fn set_project_trust_override(&self, trust_override: Option<bool>) {
        *self
            .project_trust_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = trust_override;
    }

    fn set_offline(&self, offline: bool) {
        *self
            .offline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = offline;
    }

    fn install(&self, source: &str, local: bool) -> Result<(), String> {
        let mut settings = self.build_settings(local && self.resolved_project_trusted());
        let pm = self.build_package_manager();
        pm.install_and_persist(&mut settings, source, Self::scope(local))
            .map_err(|e| format!("{e}"))
    }

    fn remove(&self, source: &str, local: bool) -> Result<bool, String> {
        let mut settings = self.build_settings(local && self.resolved_project_trusted());
        let pm = self.build_package_manager();
        pm.remove_and_persist(&mut settings, source, Self::scope(local))
            .map_err(|e| format!("{e}"))
    }

    fn list(&self) -> Result<Vec<ListedPackage>, String> {
        let settings = self.build_settings(self.resolved_project_trusted());
        let pm = self.build_package_manager();
        let configured = pm
            .list_configured_packages(&settings)
            .map_err(|e| format!("{e}"))?;
        Ok(configured
            .into_iter()
            .map(|pkg| ListedPackage {
                display: if pkg.filtered {
                    format!("{} (filtered)", pkg.source)
                } else {
                    pkg.source.clone()
                },
                installed_path: pkg.installed_path.map(|p| p.to_string_lossy().into_owned()),
                scope: match pkg.scope {
                    Scope::User => ListedScope::User,
                    Scope::Project => ListedScope::Project,
                },
            })
            .collect())
    }

    fn is_project_trusted(&self) -> bool {
        self.resolved_project_trusted()
    }

    fn refresh_models(&self) -> Result<(), String> {
        if self.offline() {
            return Err("Cannot refresh model catalogs while offline".to_owned());
        }
        let agent_dir = self.agent_dir.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("Failed to start model refresh runtime: {error}"))?
                .block_on(async move {
                    let runtime = ModelRuntime::create(
                        crate::core::model_runtime::CreateModelRuntimeOptions {
                            auth_path: Some(agent_dir.join("auth.json")),
                            models_path: Some(agent_dir.join("models.json")),
                            models_store_path: Some(agent_dir.join("models-store.json")),
                            allow_model_network: Some(true),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    runtime
                        .refresh(crate::core::model_runtime::ModelsRefreshOptions {
                            allow_network: Some(true),
                        })
                        .await
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
        })
        .join()
        .map_err(|_| "Model refresh worker panicked".to_owned())?
    }

    fn update_extensions(&self, source: Option<&str>) -> Result<(), String> {
        let settings = self.build_settings(self.resolved_project_trusted());
        let pm = self.build_package_manager();
        pm.update_extensions(&settings, source)
            .map_err(|e| format!("{e}"))
    }

    fn update_self(&self, _force: bool) -> Result<bool, String> {
        Err(
            "Self-update is not supported by this build; install the new release with your package manager"
                .to_owned(),
        )
    }

    fn open_config_selector(
        &self,
        local: bool,
        project_trust_override: Option<bool>,
    ) -> Result<(), String> {
        let options = crate::cli::config_selector::ConfigSelectorOptions {
            cwd: self.cwd.clone(),
            agent_dir: self.agent_dir.clone(),
            write_project: local,
            project_trust_override,
            offline: self.offline(),
        };
        // Bootstrap already owns a multi-thread runtime; park the worker while
        // the standalone TUI drives TerminalInput / paint futures.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(crate::cli::config_selector::select_config(options))
        })
    }
}

struct ProductOutputSink;

impl PackageOutput for ProductOutputSink {
    fn status(&self, line: &str) {
        crate::core::output_guard::ProductOutput::writeln(line);
    }
    fn status_dim(&self, line: &str) {
        crate::core::output_guard::ProductOutput::writeln(line);
    }
    fn success(&self, line: &str) {
        crate::core::output_guard::ProductOutput::writeln(line);
    }
    fn error(&self, line: &str) {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(line.as_bytes());
        let _ = stderr.write_all(b"\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeIo {
        env: Mutex<std::collections::HashMap<String, String>>,
        stdout: Mutex<Vec<String>>,
        stderr: Mutex<Vec<String>>,
    }

    impl BootstrapIo for FakeIo {
        fn env(&self, key: &str) -> Option<String> {
            self.env
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .cloned()
        }
        fn set_env(&self, key: &str, value: &str) {
            self.env
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key.to_owned(), value.to_owned());
        }
        fn cwd(&self) -> PathBuf {
            std::env::temp_dir()
        }
        fn stdin_is_tty(&self) -> bool {
            true
        }
        fn stdout_is_tty(&self) -> bool {
            true
        }
        fn read_piped_stdin<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = io::Result<Option<String>>> + Send + 'a>> {
            Box::pin(async move { Ok(None) })
        }
        fn write_stdout(&self, line: &str) {
            self.stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line.to_owned());
        }
        fn write_stderr(&self, line: &str) {
            self.stderr
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line.to_owned());
        }
    }

    #[tokio::test]
    async fn bootstrap_entries_match_new_and_restored_session_semantics() -> Result<(), String> {
        let runtime = ModelRuntime::create_in_memory()
            .await
            .map_err(|error| format!("failed to create in-memory model runtime: {error}"))?;
        let model = runtime
            .get_models(None)
            .into_iter()
            .next()
            .ok_or_else(|| "built-in model catalog is empty".to_owned())?;
        let mut fresh = crate::core::sessions::SessionManager::in_memory(Some("/tmp"), None)
            .map_err(|error| error.to_string())?;

        append_session_bootstrap_entries(&mut fresh, &[], Some(&model), ModelThinkingLevel::High)?;
        assert_eq!(
            fresh
                .get_entries()
                .iter()
                .map(|entry| entry.discriminant())
                .collect::<Vec<_>>(),
            ["model_change", "thinking_level_change"]
        );

        let messages = vec![pi_agent::AgentMessage::Llm(Box::new(pi_ai::Message::User(
            pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("seed".to_owned()), 0),
        )))];
        let mut restored = crate::core::sessions::SessionManager::in_memory(Some("/tmp"), None)
            .map_err(|error| error.to_string())?;
        restored
            .append_message(&messages[0])
            .map_err(|error| error.to_string())?;
        append_session_bootstrap_entries(
            &mut restored,
            &messages,
            Some(&model),
            ModelThinkingLevel::High,
        )?;
        append_session_bootstrap_entries(
            &mut restored,
            &messages,
            Some(&model),
            ModelThinkingLevel::High,
        )?;
        assert_eq!(
            restored
                .get_entries()
                .iter()
                .map(|entry| entry.discriminant())
                .collect::<Vec<_>>(),
            ["message", "thinking_level_change"]
        );
        Ok(())
    }

    #[test]
    fn replacement_runtime_configuration_retains_service_policy_and_api_key() {
        let args = crate::cli::args::parse_args(&[
            "--api-key".into(),
            "sk-replacement".into(),
            "--extension".into(),
            "/extensions/provider.ts".into(),
            "--provider-profile".into(),
            "strict".into(),
            "--no-skills".into(),
            "--no-prompt-templates".into(),
            "--no-themes".into(),
            "--no-context-files".into(),
            "--approve".into(),
        ]);

        let config = ReplacementRuntimeConfiguration::from_args(&args);

        assert_eq!(config.api_key.as_deref(), Some("sk-replacement"));
        assert_eq!(config.service.project_trust_override, Some(true));
        assert_eq!(
            config.service.resource_loader_options.additional_extension_paths,
            ["/extensions/provider.ts"]
        );
        assert!(config.service.resource_loader_options.no_skills);
        assert!(config.service.resource_loader_options.no_prompt_templates);
        assert!(config.service.resource_loader_options.no_themes);
        assert!(config.service.resource_loader_options.no_context_files);
        assert_eq!(
            config.service.extension_flag_values.get("provider-profile"),
            Some(&ExtensionFlagValue::Str("strict".into()))
        );
    }

    #[tokio::test]
    async fn api_key_without_selected_model_emits_reference_diagnostic() -> Result<(), String> {
        let runtime = ModelRuntime::create_in_memory()
            .await
            .map_err(|error| format!("failed to create in-memory model runtime: {error}"))?;
        let mut diagnostics = Vec::new();

        apply_cli_api_key(Some("sk-test"), None, &runtime, &mut diagnostics)
            .await
            .map_err(|error| {
                format!("missing model should be diagnostic, not factory failure: {error}")
            })?;

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].message,
            "--api-key requires a model to be specified via --model, --provider/--model, or --models"
        );
        Ok(())
    }

    #[tokio::test]
    async fn api_key_configures_provider_of_explicit_and_embedded_model_selection()
    -> Result<(), String> {
        for embedded_provider in [false, true] {
            let runtime = ModelRuntime::create_in_memory()
                .await
                .map_err(|error| format!("failed to create in-memory model runtime: {error}"))?;
            let catalog = runtime.get_models(None);
            let Some(catalog_model) = catalog.first() else {
                return Err("built-in model catalog is empty".to_owned());
            };
            let model_reference = if embedded_provider {
                format!("{}/{}", catalog_model.provider, catalog_model.id)
            } else {
                catalog_model.id.clone()
            };
            let resolved = resolve_cli_model(ResolveCliModelOptions {
                cli_provider: (!embedded_provider).then_some(catalog_model.provider.as_str()),
                cli_model: Some(&model_reference),
                cli_thinking: None,
                model_runtime: &runtime,
            });
            let Some(selected) = resolved.model else {
                return Err("CLI model did not resolve".to_owned());
            };
            let unselected_provider = catalog
                .iter()
                .find(|model| model.provider != selected.provider)
                .map(|model| model.provider.clone());
            let mut diagnostics = Vec::new();

            apply_cli_api_key(
                Some("sk-selected-provider"),
                Some(&selected),
                &runtime,
                &mut diagnostics,
            )
            .await
            .map_err(|error| format!("runtime API key installation failed: {error}"))?;

            assert!(diagnostics.is_empty());
            assert!(runtime.has_configured_auth(&selected.provider));
            if let Some(unselected_provider) = unselected_provider {
                assert!(
                    !runtime.has_configured_auth(&unselected_provider),
                    "key must not be stored under an unselected CLI provider"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn replacement_fresh_session_applies_api_key_after_model_resolution() -> Result<(), String> {
        let runtime = ModelRuntime::create_in_memory()
            .await
            .map_err(|error| format!("failed to create in-memory model runtime: {error}"))?;
        let model = runtime
            .get_models(None)
            .into_iter()
            .next()
            .ok_or_else(|| "built-in model catalog is empty".to_owned())?;
        let mut diagnostics = Vec::new();

        // `new_session` has no saved model; model selection completes first.
        apply_cli_api_key(Some("sk-fresh"), Some(&model), &runtime, &mut diagnostics).await?;

        assert!(diagnostics.is_empty());
        assert!(runtime.has_configured_auth(&model.provider));
        Ok(())
    }

    #[tokio::test]
    async fn replacement_saved_session_provisions_api_key_before_restore() -> Result<(), String> {
        let runtime = ModelRuntime::create_in_memory()
            .await
            .map_err(|error| format!("failed to create in-memory model runtime: {error}"))?;
        let saved_model = runtime
            .get_models(None)
            .into_iter()
            .next()
            .ok_or_else(|| "built-in model catalog is empty".to_owned())?;

        // The replacement factory installs this before restore_model_from_session
        // checks configured auth for the saved provider.
        install_cli_api_key("sk-restored", &saved_model, &runtime).await?;
        assert!(runtime.has_configured_auth(&saved_model.provider));

        let mut diagnostics = Vec::new();
        apply_cli_api_key(
            Some("sk-restored"),
            Some(&saved_model),
            &runtime,
            &mut diagnostics,
        )
        .await?;
        assert!(diagnostics.is_empty());
        Ok(())
    }

    /// Version short-circuit works with a fake factory (never reaches runtime).
    #[tokio::test]
    async fn run_pipeline_version_exits_zero() {
        let io_state = Arc::new(FakeIo::default());
        let factory = Arc::new(FakeRuntimeFactory);
        let handler = Arc::new(FakePackageHandler);
        let output = Arc::new(FakePackageOutput::default());
        let dispatcher = Arc::new(DefaultDispatcher::new());
        let io = Io::custom(io_state, factory, handler, output, dispatcher);
        let result = run_pipeline(vec!["--version".to_owned()], &io).await;
        assert_eq!(result, ExitCode::from(0));
    }

    #[tokio::test]
    async fn run_pipeline_unknown_flag_exits_one() {
        let io_state = Arc::new(FakeIo::default());
        let factory = Arc::new(FakeRuntimeFactory);
        let handler = Arc::new(FakePackageHandler);
        let output = Arc::new(FakePackageOutput::default());
        let dispatcher = Arc::new(DefaultDispatcher::new());
        let io = Io::custom(io_state, factory, handler, output, dispatcher);
        let result = run_pipeline(vec!["-Z".to_owned()], &io).await;
        assert_eq!(result, ExitCode::from(1));
    }

    #[test]
    fn unavailable_real_updates_fail_honestly() {
        let handler = RealPackageHandler::new(true);
        assert_eq!(
            handler.refresh_models(),
            Err("Cannot refresh model catalogs while offline".to_owned())
        );
        let error = handler.update_self(false).err();
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("not supported by this build")),
            "self-update must fail honestly without an engine: {error:?}"
        );
    }

    // Fake factory that errors with a stable string.
    struct FakeRuntimeFactory;
    impl RuntimeFactory for FakeRuntimeFactory {
        fn create(
            &self,
            _options: RuntimeFactoryOptions,
        ) -> BoxFuture<'_, Result<RuntimeHandle, String>> {
            async { Err("__fake_factory__".to_owned()) }.boxed()
        }
    }

    struct FakePackageHandler;
    impl PackageHandler for FakePackageHandler {
        fn install(&self, _s: &str, _l: bool) -> Result<(), String> {
            Ok(())
        }
        fn remove(&self, _s: &str, _l: bool) -> Result<bool, String> {
            Ok(true)
        }
        fn list(&self) -> Result<Vec<ListedPackage>, String> {
            Ok(Vec::new())
        }
        fn is_project_trusted(&self) -> bool {
            true
        }
        fn refresh_models(&self) -> Result<(), String> {
            Ok(())
        }
        fn update_extensions(&self, _s: Option<&str>) -> Result<(), String> {
            Ok(())
        }
        fn update_self(&self, _f: bool) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[derive(Default)]
    struct FakePackageOutput {
        lines: Mutex<Vec<String>>,
    }

    impl PackageOutput for FakePackageOutput {
        fn status(&self, line: &str) {
            self.lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line.to_owned());
        }
        fn status_dim(&self, line: &str) {
            self.status(line);
        }
        fn success(&self, line: &str) {
            self.status(line);
        }
        fn error(&self, line: &str) {
            self.status(line);
        }
    }
}
