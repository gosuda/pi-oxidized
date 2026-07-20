//! CLI bootstrap: argv → resolved mode + runtime handle, or short-circuit exit.
//!
//! Ports the ordered pipeline of `.references/pi/packages/coding-agent/src/
//! main.ts` into a testable orchestrator. Everything that touches the process
//! (env vars, TTY status, stdin, stdout/stderr) flows through an injected
//! [`BootstrapIo`] trait; everything that builds the agent-session runtime
//! flows through an injected [`RuntimeFactory`] trait. Pure helpers
//! ([`resolve_app_mode`], validators) are public so the integrator and tests
//! can drive them directly.
//!
//! # Pipeline order (matches `main.ts:472-858`)
//!
//! 1. Reset timings (no-op here; the timings module is a sibling concern).
//! 2. Offline mode: `args.contains("--offline") || PI_OFFLINE` →
//!    `PackageHandler::set_offline(true)` (no process-env mutation under
//!    `unsafe_code = "forbid"`).
//! 3. Package/config short-circuit (`pi install …`, `pi config …`).
//! 4. `parse_args` + report diagnostics (errors exit 1, warnings continue).
//! 5. `--version` → print version, exit 0.
//! 6. `--export` → export session file, exit 0/1.
//! 7. `resolve_app_mode` + `take_over_stdout` (unless interactive or plain
//!    runtime metadata command).
//! 8. RPC + `@file` guard.
//! 9. Fork / session-id flag validation.
//! 10. `run_migrations` (records `migrated_providers` + `deprecation_warnings`).
//! 11. Startup settings + first-time setup (interactive only — gated, the UI
//!     surface itself is a sibling slice).
//! 12. Session-dir resolution + `create_session_manager`.
//! 13. `--name` append + validation.
//! 14. Trust store + resource paths.
//! 15. Build runtime via factory.
//! 16. `--help` (with extension flags) → exit 0.
//! 17. `--list-models` → exit 0.
//! 18. Read piped stdin (skip RPC); demote interactive→print when stdin present.
//! 19. Prepare initial message.
//! 20. Report runtime diagnostics (errors exit 1, with extension hint when
//!     relevant).
//! 21. Non-interactive requires `session.model`, else no-models message + exit 1.
//! 22. `PI_STARTUP_BENCHMARK` guard.
//! 23. Return [`BootstrapOutcome::Dispatch`] with the resolved mode + handle.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures::future::BoxFuture;

use crate::cli::args::{Args, DiagnosticLevel, ListModels, Mode};
use crate::cli::package_manager_cli::{self, DispatchPlatform, PackageHandler, PackageOutput};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::config::{ENV_SESSION_DIR, VERSION, expand_tilde_path};
use crate::core::migrations::{self, MigrationResult};
use crate::core::output_guard::{self, ProductOutput};
use crate::core::sessions::SessionManager;

/// Resolved application mode.
///
/// Mirrors TypeScript `AppMode = interactive | print | json | rpc`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppMode {
    /// Interactive TUI session.
    Interactive,
    /// Single-shot text print.
    Print,
    /// JSONL event print.
    Json,
    /// Headless JSONL RPC server.
    Rpc,
}

impl AppMode {
    /// Returns `true` for the JSON output mode.
    #[must_use]
    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }

    /// Returns `true` for the RPC server mode.
    #[must_use]
    pub const fn is_rpc(self) -> bool {
        matches!(self, Self::Rpc)
    }

    /// Returns `true` for the interactive TUI mode.
    #[must_use]
    pub const fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// Resolve the application mode from parsed args and TTY status.
///
/// Mirrors `resolveAppMode` (`main.ts:99-110`):
/// - `--mode rpc` → RPC (highest precedence, forces non-interactive).
/// - `--mode json` → JSON.
/// - `--print` or non-TTY stdin/stdout → Print.
/// - Otherwise → Interactive.
#[must_use]
pub fn resolve_app_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    match parsed.mode {
        Some(Mode::Rpc) => AppMode::Rpc,
        Some(Mode::Json) => AppMode::Json,
        Some(Mode::Text) | None => {
            if parsed.print || !stdin_is_tty || !stdout_is_tty {
                AppMode::Print
            } else {
                AppMode::Interactive
            }
        }
    }
}

/// Whether `--help`/`--list-models` should run without taking over stdout.
///
/// Mirrors `isPlainRuntimeMetadataCommand` (`main.ts:116-118`): true when the
/// user did not pass `--print` or an explicit `--mode` and is asking for
/// `--help` or `--list-models`. These commands print to stdout directly
/// without the protocol takeover so the output reaches the user even when the
/// runtime is built (e.g. to discover extension flags).
#[must_use]
pub fn is_plain_runtime_metadata_command(parsed: &Args) -> bool {
    !parsed.print
        && parsed.mode.is_none()
        && (parsed.help || !matches!(parsed.list_models, ListModels::None))
}

/// Error returned by [`validate_fork_flags`] / [`validate_session_id_flags`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlagValidationError {
    /// Verbatim message to surface on stderr (prefixed with `Error: `).
    pub message: String,
}

/// `--fork` cannot combine with `--session`/`--continue`/`--resume`/`--no-session`.
///
/// Mirrors `validateForkFlags` (`main.ts:204-218`).
///
/// # Errors
/// Returns an error when `--fork` is combined with any incompatible session flag.
pub fn validate_fork_flags(parsed: &Args) -> Result<(), FlagValidationError> {
    let Some(_fork) = parsed.fork.as_ref() else {
        return Ok(());
    };
    let mut conflicts = Vec::new();
    if parsed.session.is_some() {
        conflicts.push("--session");
    }
    if parsed.r#continue {
        conflicts.push("--continue");
    }
    if parsed.resume {
        conflicts.push("--resume");
    }
    if parsed.no_session {
        conflicts.push("--no-session");
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(FlagValidationError {
            message: format!("--fork cannot be combined with {}", conflicts.join(", ")),
        })
    }
}

/// `--session-id` cannot combine with `--session`/`--continue`/`--resume`.
///
/// Mirrors `validateSessionIdFlags` (`main.ts:220-241`). The ID syntax check
/// itself is delegated to `SessionManager`/`assert_valid_session_id` at
/// session-construction time; this validator only covers the CLI-level
/// conflicts.
///
/// # Errors
/// Returns an error when `--session-id` is combined with `--session`,
/// `--continue`, or `--resume`.
pub fn validate_session_id_flags(parsed: &Args) -> Result<(), FlagValidationError> {
    let Some(_id) = parsed.session_id.as_ref() else {
        return Ok(());
    };
    let mut conflicts = Vec::new();
    if parsed.session.is_some() {
        conflicts.push("--session");
    }
    if parsed.r#continue {
        conflicts.push("--continue");
    }
    if parsed.resume {
        conflicts.push("--resume");
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(FlagValidationError {
            message: format!(
                "--session-id cannot be combined with {}",
                conflicts.join(", ")
            ),
        })
    }
}

/// Validate `--name`/`-n` post-parse.
///
/// The parser records an empty value verbatim (`--name ""` → `name = Some("")`).
/// `main.ts:590-597` rejects empty/whitespace-only names with exit 1 before
/// appending to session info.
///
/// # Errors
/// Returns an error when `--name` is present but empty or whitespace-only.
pub fn validate_name(parsed: &Args) -> Result<(), FlagValidationError> {
    if let Some(name) = parsed.name.as_ref()
        && name.trim().is_empty()
    {
        return Err(FlagValidationError {
            message: "--name requires a non-empty value".to_owned(),
        });
    }
    Ok(())
}

/// Truthy env-flag test matching `isTruthyEnvFlag` (`main.ts:94-97`).
#[must_use]
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let lower = v.to_ascii_lowercase();
            v == "1" || lower == "true" || lower == "yes"
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// I/O injection
// ---------------------------------------------------------------------------

/// Process-level I/O surface the bootstrap needs.
///
/// All methods are sync except [`Self::read_piped_stdin`]. Implementations are
/// expected to be cheap to clone (`Arc`-backed) so the bootstrap can hold them
/// across `.await` points.
pub trait BootstrapIo: Send + Sync {
    /// Read an environment variable.
    fn env(&self, key: &str) -> Option<String>;
    /// Set an environment variable for this process.
    fn set_env(&self, key: &str, value: &str);
    /// Process working directory.
    fn cwd(&self) -> PathBuf;
    /// Whether stdin is attached to a TTY.
    fn stdin_is_tty(&self) -> bool;
    /// Whether stdout is attached to a TTY.
    fn stdout_is_tty(&self) -> bool;
    /// Read piped stdin to EOF, returning `None` when empty/whitespace-only.
    fn read_piped_stdin<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<String>>> + Send + 'a>>;
    /// Write a line to product stdout (newline appended).
    fn write_stdout(&self, line: &str);
    /// Write a line to product stderr (newline appended).
    fn write_stderr(&self, line: &str);
}

// ---------------------------------------------------------------------------
// Runtime factory injection
// ---------------------------------------------------------------------------

/// Inputs to [`RuntimeFactory::create`].
pub struct RuntimeFactoryOptions {
    /// Effective working directory.
    pub cwd: String,
    /// Agent config directory.
    pub agent_dir: String,
    /// Session manager bound to the chosen session.
    pub session_manager: SessionManager,
    /// Parsed CLI args (for extension flags, model overrides, tool gates).
    pub parsed: Args,
}

/// Result of a successful runtime construction.
pub struct RuntimeHandle {
    /// The runtime owning the current session.
    pub runtime: std::sync::Arc<AgentSessionRuntime>,
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("runtime", &"<AgentSessionRuntime>")
            .finish()
    }
}

/// Injected factory that turns a session manager + CLI args into a live
/// [`AgentSessionRuntime`].
///
/// Implementations wrap `create_agent_session_services` +
/// `create_agent_session_from_services` + `AgentSessionRuntime::new`. A fake
/// implementation in tests returns a lightweight runtime without network or
/// filesystem access.
pub trait RuntimeFactory: Send + Sync {
    /// Build the runtime.
    ///
    /// # Errors
    /// Implementation-defined; the error string is surfaced verbatim prefixed
    /// with `Error: `.
    fn create(
        &self,
        options: RuntimeFactoryOptions,
    ) -> BoxFuture<'_, Result<RuntimeHandle, String>>;

    /// Whether the factory has a usable interactive TUI runtime. The default
    /// is `true`; a minimal/fake factory returns `false` so the bootstrap
    /// short-circuits interactive runs with an actionable error instead of
    /// launching an empty TUI.
    fn supports_interactive(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Result of running the bootstrap pipeline.
#[derive(Debug)]
pub enum BootstrapOutcome {
    /// Short-circuit: process should exit with this code and no further work.
    Exit {
        /// Process exit code.
        code: u8,
        /// Win32 `pi update` success must drain naturally.
        drain_quirk: bool,
    },
    /// Continue to mode dispatch.
    Dispatch(Dispatched),
}

/// A bootstrap that reached the dispatch stage.
#[derive(Debug)]
pub struct Dispatched {
    /// Resolved application mode.
    pub mode: AppMode,
    /// Live runtime handle.
    pub handle: RuntimeHandle,
    /// Initial message assembled from stdin + `@file` + first CLI message.
    pub initial_message: Option<String>,
    /// Images attached to the initial message.
    pub initial_images: Vec<pi_ai::ImageContent>,
    /// Remaining CLI messages for follow-up prompts.
    pub remaining_messages: Vec<String>,
    /// Migration result (carried into interactive mode for changelog display).
    pub migrations: MigrationResult,
}

// ---------------------------------------------------------------------------
// Bootstrap driver
// ---------------------------------------------------------------------------

/// Inputs to [`run_bootstrap`].
pub struct BootstrapInputs<'a> {
    /// argv after the program name.
    pub args: Vec<String>,
    /// Process I/O surface.
    pub io: &'a dyn BootstrapIo,
    /// Runtime factory.
    pub factory: &'a dyn RuntimeFactory,
    /// Package-command handler (real or fake).
    pub package_handler: &'a dyn PackageHandler,
    /// Package-command output sink.
    pub package_output: &'a dyn PackageOutput,
}

/// Run the bootstrap pipeline.
///
/// This is the pure-logic orchestration layer; all process interaction is
/// routed through `inputs.io` and side-effecting package operations through
/// `inputs.package_handler`.
pub async fn run_bootstrap(inputs: BootstrapInputs<'_>) -> BootstrapOutcome {
    let parsed = match initialize_bootstrap(&inputs) {
        Ok(parsed) => parsed,
        Err(exit) => return exit.into_outcome(),
    };
    let prepared = match prepare_session(&inputs, parsed).await {
        Ok(prepared) => prepared,
        Err(exit) => return exit.into_outcome(),
    };
    let (state, handle) = match create_runtime(&inputs, prepared).await {
        Ok(runtime) => runtime,
        Err(exit) => return exit.into_outcome(),
    };
    if let Some(outcome) = handle_runtime_metadata(inputs.io, &state, &handle).await {
        return outcome;
    }
    finish_bootstrap(inputs.io, state, handle).await
}

type BootstrapStep<T> = Result<T, BootstrapExit>;

struct PreparedSession {
    parsed: Args,
    app_mode: AppMode,
    should_take_over_stdout: bool,
    cwd: String,
    migrations: MigrationResult,
    agent_dir: String,
    session_manager: SessionManager,
}

struct BootstrapState {
    parsed: Args,
    app_mode: AppMode,
    should_take_over_stdout: bool,
    cwd: String,
    migrations: MigrationResult,
}

#[derive(Clone, Copy)]
struct BootstrapExit {
    code: u8,
    drain_quirk: bool,
}

impl BootstrapExit {
    fn into_outcome(self) -> BootstrapOutcome {
        exit(self.code, self.drain_quirk)
    }
}

fn exit(code: u8, drain_quirk: bool) -> BootstrapOutcome {
    BootstrapOutcome::Exit { code, drain_quirk }
}

fn stop(code: u8, drain_quirk: bool) -> BootstrapExit {
    BootstrapExit { code, drain_quirk }
}

fn fail(io: &dyn BootstrapIo, restore_stdout: bool, message: &str) -> BootstrapExit {
    io.write_stderr(message);
    if restore_stdout {
        output_guard::restore_stdout();
    }
    stop(1, false)
}

fn initialize_bootstrap(inputs: &BootstrapInputs<'_>) -> BootstrapStep<Args> {
    let mut parsed = crate::cli::args::parse_args(&inputs.args);
    let offline_mode = parsed.offline || is_truthy_env_flag(inputs.io.env("PI_OFFLINE").as_deref());
    parsed.offline = offline_mode;
    if offline_mode {
        // Process-env mutation is impossible under `unsafe_code = "forbid"`.
        // Thread the bool into package handlers and any other consumers that
        // previously read PI_OFFLINE / PI_SKIP_VERSION_CHECK from the env.
        inputs.package_handler.set_offline(true);
    }

    if let Some(outcome) = package_manager_cli::handle_package_command(
        &inputs.args,
        inputs.package_handler,
        inputs.package_output,
        current_platform(),
    ) {
        return Err(stop(outcome.exit_code, outcome.drain_quirk));
    }
    if let Some(outcome) = package_manager_cli::handle_config_command(
        &inputs.args,
        inputs.package_handler,
        inputs.package_output,
        inputs.io.stdin_is_tty(),
        inputs.io.stdout_is_tty(),
    ) {
        return Err(stop(outcome.exit_code, outcome.drain_quirk));
    }

    let mut parsed = crate::cli::args::parse_args(&inputs.args);
    parsed.offline = offline_mode;
    report_diagnostics(&parsed, inputs.io);
    if parsed
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
    {
        return Err(stop(1, false));
    }
    if parsed.version {
        inputs.io.write_stdout(VERSION);
        return Err(stop(0, false));
    }
    if let Some(export_path) = parsed.export.as_ref()
        && let Err(message) = run_export(export_path, parsed.messages.first())
    {
        inputs.io.write_stderr(&format!("Error: {message}"));
        return Err(stop(1, false));
    }
    if parsed.export.is_some() {
        return Err(stop(0, false));
    }
    Ok(parsed)
}

async fn prepare_session(
    inputs: &BootstrapInputs<'_>,
    parsed: Args,
) -> BootstrapStep<PreparedSession> {
    let app_mode = resolve_app_mode(&parsed, inputs.io.stdin_is_tty(), inputs.io.stdout_is_tty());
    let plain_metadata = is_plain_runtime_metadata_command(&parsed);
    let should_take_over_stdout = !app_mode.is_interactive() && !plain_metadata && !parsed.help;
    if should_take_over_stdout && let Err(err) = output_guard::take_over_stdout() {
        return Err(fail(inputs.io, false, &format!("Error: {err}")));
    }
    if app_mode.is_rpc() && !parsed.file_args.is_empty() {
        return Err(fail(
            inputs.io,
            should_take_over_stdout,
            "Error: @file arguments are not supported in RPC mode",
        ));
    }
    let flag_error = validate_fork_flags(&parsed)
        .err()
        .or_else(|| validate_session_id_flags(&parsed).err());
    if let Some(err) = flag_error {
        return Err(fail(
            inputs.io,
            should_take_over_stdout,
            &format!("Error: {}", err.message),
        ));
    }

    let cwd = inputs.io.cwd().to_string_lossy().into_owned();
    let migrations = migrations::run_migrations(Path::new(&cwd));
    let agent_dir = resolve_agent_dir(inputs.io);
    let session_dir = resolve_session_dir(&parsed, inputs.io);
    let mut session_manager =
        build_session_manager(&parsed, &cwd, session_dir.as_deref(), app_mode)
            .await
            .map_err(|message| {
                fail(
                    inputs.io,
                    should_take_over_stdout,
                    &format!("Error: {message}"),
                )
            })?;

    if let Some(name) = parsed.name.as_ref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(fail(
                inputs.io,
                should_take_over_stdout,
                "Error: --name requires a non-empty value",
            ));
        }
        session_manager
            .append_session_info(trimmed)
            .map_err(|err| fail(inputs.io, should_take_over_stdout, &format!("Error: {err}")))?;
    }

    Ok(PreparedSession {
        parsed,
        app_mode,
        should_take_over_stdout,
        cwd,
        migrations,
        agent_dir,
        session_manager,
    })
}

async fn create_runtime(
    inputs: &BootstrapInputs<'_>,
    prepared: PreparedSession,
) -> BootstrapStep<(BootstrapState, RuntimeHandle)> {
    let PreparedSession {
        parsed,
        app_mode,
        should_take_over_stdout,
        cwd,
        migrations,
        agent_dir,
        session_manager,
    } = prepared;
    let handle = inputs
        .factory
        .create(RuntimeFactoryOptions {
            cwd: session_manager.get_cwd().to_owned(),
            agent_dir,
            session_manager,
            parsed: parsed.clone(),
        })
        .await
        .map_err(|message| {
            fail(
                inputs.io,
                should_take_over_stdout,
                &format!("Error: {message}"),
            )
        })?;

    let diagnostics = handle.runtime.diagnostics();
    let has_error = diagnostics.iter().any(|diagnostic| {
        diagnostic.kind
            == crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Error
    });
    let has_extension_load_error = diagnostics.iter().any(|diagnostic| {
        diagnostic.kind
            == crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Error
            && diagnostic.message.contains("Failed to load extension")
    });
    for diagnostic in &diagnostics {
        let label = match diagnostic.kind {
            crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Info => "Info",
            crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Warning => {
                "Warning"
            }
            crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Error => {
                "Error"
            }
        };
        inputs
            .io
            .write_stderr(&format!("{label}: {}", diagnostic.message));
    }
    if has_error {
        if has_extension_load_error {
            inputs
                .io
                .write_stderr("Hint: Start without extensions using \"pi -ne\".");
        }
        if should_take_over_stdout {
            output_guard::restore_stdout();
        }
        return Err(stop(1, false));
    }

    Ok((
        BootstrapState {
            parsed,
            app_mode,
            should_take_over_stdout,
            cwd,
            migrations,
        },
        handle,
    ))
}

async fn handle_runtime_metadata(
    io: &dyn BootstrapIo,
    state: &BootstrapState,
    handle: &RuntimeHandle,
) -> Option<BootstrapOutcome> {
    if state.parsed.help {
        let extension_flags: Vec<crate::cli::help::ExtensionFlagHelp> =
            collect_extension_flags(&handle.runtime);
        let text = crate::cli::help::format_help(
            Some(&extension_flags),
            crate::cli::help::HelpStyle { styled: false },
        );
        io.write_stdout(&text);
        if state.should_take_over_stdout {
            output_guard::restore_stdout();
        }
        return Some(exit(0, false));
    }
    if matches!(state.parsed.list_models, ListModels::None) {
        return None;
    }
    if state.should_take_over_stdout {
        output_guard::restore_stdout();
    }
    // Pass the original token: fuzzy matching lowercases internally and the
    // no-match message must echo the user's input verbatim (upstream parity).
    let pattern = match &state.parsed.list_models {
        ListModels::Search(search) => Some(search.clone()),
        ListModels::All | ListModels::None => None,
    };
    let model_runtime = handle.runtime.session().model_runtime_handle()?;
    render_list_models(io, model_runtime.as_ref(), pattern.as_deref()).await
}

/// Render the `--list-models` table or its empty/no-match messages.
async fn render_list_models(
    io: &dyn BootstrapIo,
    model_runtime: &crate::core::model_runtime::ModelRuntime,
    pattern: Option<&str>,
) -> Option<BootstrapOutcome> {
    let Ok(models) = model_runtime.get_available(None).await else {
        return Some(exit(0, false));
    };
    let load_error = model_runtime.get_error();
    if let Some(message) = load_error.as_deref() {
        io.write_stderr(&format!("Warning: errors loading models.json:\n{message}"));
    }
    if models.is_empty() {
        io.write_stdout(&crate::core::agent_session_services::format_no_models_available_message());
        return Some(exit(0, false));
    }

    let mut filtered: Vec<&pi_ai::Model> = if let Some(search) = pattern {
        #[derive(Clone)]
        struct ModelRefWithText<'a> {
            model: &'a pi_ai::Model,
            text: String,
        }
        let wrapped: Vec<ModelRefWithText> = models
            .iter()
            .map(|model| ModelRefWithText {
                model,
                text: format!("{} {}", model.provider, model.id),
            })
            .collect();
        pi_tui::fuzzy::fuzzy_filter(&wrapped, search, |item| &item.text)
            .iter()
            .map(|item| item.model)
            .collect()
    } else {
        models.iter().collect()
    };

    if filtered.is_empty()
        && let Some(search) = pattern
    {
        io.write_stdout(&format!(r#"No models matching "{search}""#));
        return Some(exit(0, false));
    }

    filtered.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.id.cmp(&right.id))
    });
    let header = ListModelsRow {
        provider: "provider".to_owned(),
        model: "model".to_owned(),
        context: "context".to_owned(),
        max_out: "max-out".to_owned(),
        thinking: "thinking".to_owned(),
        images: "images".to_owned(),
    };
    let rows: Vec<ListModelsRow> = filtered
        .iter()
        .map(|model| ListModelsRow {
            provider: model.provider.clone(),
            model: model.id.clone(),
            context: format_token_count(model.context_window),
            max_out: format_token_count(model.max_tokens),
            thinking: if model.reasoning { "yes" } else { "no" }.to_owned(),
            images: if model
                .input
                .iter()
                .any(|m| matches!(m, pi_ai::ModelInput::Image))
            {
                "yes"
            } else {
                "no"
            }
            .to_owned(),
        })
        .collect();
    emit_list_models_table(io, &header, &rows);
    Some(exit(0, false))
}

/// One row of the `--list-models` table.
///
/// Mirrors the upstream `listModels` row shape (provider/model/context/max-out
/// /thinking/images) so column widths are computed from the rendered strings.
struct ListModelsRow {
    provider: String,
    model: String,
    context: String,
    max_out: String,
    thinking: String,
    images: String,
}

/// Format a token count using the upstream-friendly `K` / `M` style.
///
/// Mirrors `.references/pi/packages/coding-agent/src/cli/list-models.ts`
/// `formatTokenCount`: integer buckets use a bare count; sub-integer are kept
/// at one decimal; values under 1k render as the raw number.
fn format_token_count(count: u64) -> String {
    const ONE_MILLION: u64 = 1_000_000;
    const ONE_THOUSAND: u64 = 1_000;
    if count >= ONE_MILLION {
        let whole = count / ONE_MILLION;
        let remainder = count % ONE_MILLION;
        if remainder == 0 {
            format!("{whole}M")
        } else {
            let tenths = (remainder * 10 + ONE_MILLION / 2) / ONE_MILLION;
            if tenths == 10 {
                format!("{}.0M", whole + 1)
            } else {
                format!("{whole}.{tenths}M")
            }
        }
    } else if count >= ONE_THOUSAND {
        let whole = count / ONE_THOUSAND;
        let remainder = count % ONE_THOUSAND;
        if remainder == 0 {
            format!("{whole}K")
        } else {
            let tenths = (remainder * 10 + ONE_THOUSAND / 2) / ONE_THOUSAND;
            if tenths == 10 {
                format!("{}.0K", whole + 1)
            } else {
                format!("{whole}.{tenths}K")
            }
        }
    } else {
        count.to_string()
    }
}

/// Emit the `--list-models` table with header + data rows.
///
/// Column widths use the max of the header label and every rendered cell, then
/// pad with a two-space gutter (matching upstream `padEnd` + `"  "`).
fn emit_list_models_table(io: &dyn BootstrapIo, header: &ListModelsRow, rows: &[ListModelsRow]) {
    let width = |label: &str, key: fn(&ListModelsRow) -> &str| -> usize {
        label
            .len()
            .max(rows.iter().map(|row| key(row).len()).max().unwrap_or(0))
    };
    let w_provider = width(&header.provider, |row| &row.provider);
    let w_model = width(&header.model, |row| &row.model);
    let w_context = width(&header.context, |row| &row.context);
    let w_max_out = width(&header.max_out, |row| &row.max_out);
    let w_thinking = width(&header.thinking, |row| &row.thinking);
    let w_images = width(&header.images, |row| &row.images);
    io.write_stdout(&format_list_models_row(
        &header.provider,
        &header.model,
        &header.context,
        &header.max_out,
        &header.thinking,
        &header.images,
        w_provider,
        w_model,
        w_context,
        w_max_out,
        w_thinking,
        w_images,
    ));
    for row in rows {
        io.write_stdout(&format_list_models_row(
            &row.provider,
            &row.model,
            &row.context,
            &row.max_out,
            &row.thinking,
            &row.images,
            w_provider,
            w_model,
            w_context,
            w_max_out,
            w_thinking,
            w_images,
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn format_list_models_row(
    provider: &str,
    model: &str,
    context: &str,
    max_out: &str,
    thinking: &str,
    images: &str,
    w_provider: usize,
    w_model: usize,
    w_context: usize,
    w_max_out: usize,
    w_thinking: usize,
    w_images: usize,
) -> String {
    [
        provider.pad_to(w_provider),
        model.pad_to(w_model),
        context.pad_to(w_context),
        max_out.pad_to(w_max_out),
        thinking.pad_to(w_thinking),
        images.pad_to(w_images),
    ]
    .join("  ")
}

trait PadTo {
    fn pad_to(&self, width: usize) -> String;
}

impl PadTo for str {
    fn pad_to(&self, width: usize) -> String {
        if width <= self.len() {
            self.to_owned()
        } else {
            let mut out = String::with_capacity(width);
            out.push_str(self);
            for _ in 0..(width - self.len()) {
                out.push(' ');
            }
            out
        }
    }
}

async fn finish_bootstrap(
    io: &dyn BootstrapIo,
    mut state: BootstrapState,
    handle: RuntimeHandle,
) -> BootstrapOutcome {
    let stdin_content = if state.app_mode.is_rpc() {
        None
    } else {
        match io.read_piped_stdin().await {
            Ok(content) => content,
            Err(err) => {
                return fail(
                    io,
                    state.should_take_over_stdout,
                    &format!("Error: failed to read stdin: {err}"),
                )
                .into_outcome();
            }
        }
    };
    if stdin_content.is_some() && state.app_mode.is_interactive() {
        state.app_mode = AppMode::Print;
    }
    let (initial_message, initial_images, remaining_messages) = prepare_initial_message(
        &mut state.parsed,
        stdin_content.as_deref(),
        &handle.runtime,
        &state.cwd,
    )
    .await;

    if !state.app_mode.is_interactive() {
        let session = handle.runtime.session();
        if session.model().provider == "unknown" {
            return fail(
                io,
                state.should_take_over_stdout,
                &crate::core::agent_session_services::format_no_models_available_message(),
            )
            .into_outcome();
        }
    }
    if is_truthy_env_flag(io.env("PI_STARTUP_BENCHMARK").as_deref())
        && !state.app_mode.is_interactive()
    {
        return fail(
            io,
            state.should_take_over_stdout,
            "Error: PI_STARTUP_BENCHMARK only supports interactive mode",
        )
        .into_outcome();
    }

    BootstrapOutcome::Dispatch(Dispatched {
        mode: state.app_mode,
        handle,
        initial_message,
        initial_images,
        remaining_messages,
        migrations: state.migrations,
    })
}

/// Report parse diagnostics to stderr (no exit decision here).
fn report_diagnostics(parsed: &Args, io: &dyn BootstrapIo) {
    for d in &parsed.diagnostics {
        let label = match d.level {
            DiagnosticLevel::Error => "Error",
            DiagnosticLevel::Warning => "Warning",
        };
        io.write_stderr(&format!("{label}: {}", d.message));
    }
}

/// Resolve the agent dir from process env (`PI_CODING_AGENT_DIR`) or home.
///
/// Reads the real process environment via [`crate::core::config::get_agent_dir`]
/// so the path matches what the settings/session managers resolve
/// independently. Tests that need a specific agent dir set the env var
/// before invoking the bootstrap.
fn resolve_agent_dir(_io: &dyn BootstrapIo) -> String {
    crate::core::config::get_agent_dir()
        .to_string_lossy()
        .into_owned()
}

/// Resolve the session directory from `--session-dir`, env, or settings.
fn resolve_session_dir(parsed: &Args, io: &dyn BootstrapIo) -> Option<String> {
    if let Some(dir) = parsed.session_dir.as_ref() {
        let normalized =
            crate::core::config::normalize_path(dir, crate::core::config::PathInputOptions::new());
        return Some(normalized.to_string_lossy().into_owned());
    }
    if let Some(env_dir) = io.env(ENV_SESSION_DIR) {
        return Some(expand_tilde_path(&env_dir).to_string_lossy().into_owned());
    }
    None
}

/// Build the [`SessionManager`] using the same branching as
/// `createSessionManager` (`main.ts:263-354`).
///
/// The interactive `--resume`/global-search paths require UI selectors owned
/// by a sibling slice; for now those paths fall back to an in-memory manager
/// with the requested id so the runtime still builds. The integrator wires the
/// real selector when the TUI lands.
async fn build_session_manager(
    parsed: &Args,
    cwd: &str,
    session_dir: Option<&str>,
    app_mode: AppMode,
) -> Result<SessionManager, String> {
    let id_opt = parsed.session_id.as_deref();

    if parsed.no_session || parsed.help || !matches!(parsed.list_models, ListModels::None) {
        return SessionManager::in_memory(Some(cwd), id_opt.map(sessions_id_opt))
            .map_err(|e| e.to_string());
    }

    if let Some(fork_arg) = parsed.fork.as_deref() {
        let resolved = resolve_session_path(fork_arg, cwd, session_dir).await;
        return SessionManager::fork_from(
            &resolved.path,
            cwd,
            session_dir,
            id_opt.map(sessions_id_opt),
        )
        .map_err(|e| e.to_string());
    }

    if let Some(session_arg) = parsed.session.as_deref() {
        let resolved = resolve_session_path(session_arg, cwd, session_dir).await;
        return SessionManager::open(&resolved.path, session_dir, None).map_err(|e| e.to_string());
    }

    if parsed.resume {
        let _ = app_mode;
        return SessionManager::continue_recent(cwd, session_dir).map_err(|e| e.to_string());
    }

    if parsed.r#continue {
        return SessionManager::continue_recent(cwd, session_dir).map_err(|e| e.to_string());
    }

    if let Some(id) = id_opt
        && let Some(path) = find_local_session_by_exact_id(id, cwd, session_dir).await
    {
        return SessionManager::open(&path, session_dir, None).map_err(|e| e.to_string());
    }

    SessionManager::create(cwd, session_dir, id_opt.map(sessions_id_opt)).map_err(|e| e.to_string())
}

fn sessions_id_opt(id: &str) -> crate::core::sessions::NewSessionOptions {
    crate::core::sessions::NewSessionOptions {
        id: Some(id.to_owned()),
        parent_session: None,
    }
}

/// Result of resolving a session argument to a file path.
#[derive(Clone, Debug)]
struct ResolvedSession {
    path: String,
}

/// Resolve a session argument to a file path.
///
/// If it looks like a path (contains `/` or `\` or ends with `.jsonl`), use it
/// as-is. Otherwise try to match as a session ID in the current project. The
/// global-search path is omitted until the interactive selector lands.
async fn resolve_session_path(arg: &str, cwd: &str, session_dir: Option<&str>) -> ResolvedSession {
    if arg.contains('/') || arg.contains('\\') || has_jsonl_extension(arg) {
        let resolved = crate::core::config::resolve_path_with(
            arg,
            Path::new(cwd),
            crate::core::config::PathInputOptions::new(),
        );
        return ResolvedSession {
            path: resolved.to_string_lossy().into_owned(),
        };
    }
    if let Some(path) = find_local_session_by_exact_id(arg, cwd, session_dir).await {
        return ResolvedSession { path };
    }
    // No match — hand the literal arg back so SessionManager produces the
    // canonical "No session found" error.
    ResolvedSession {
        path: crate::core::config::resolve_path_with(
            arg,
            Path::new(cwd),
            crate::core::config::PathInputOptions::new(),
        )
        .to_string_lossy()
        .into_owned(),
    }
}

fn has_jsonl_extension(path: &str) -> bool {
    path.as_bytes()
        .get(path.len().saturating_sub(6)..)
        .is_some_and(|extension| extension.eq_ignore_ascii_case(b".jsonl"))
}

/// Match a session ID exactly in the current project.
///
/// `list_sessions_for_cwd` is async, so this helper is async too.
async fn find_local_session_by_exact_id(
    session_id: &str,
    cwd: &str,
    session_dir: Option<&str>,
) -> Option<String> {
    let session_dir_path = std::path::Path::new(session_dir.unwrap_or("."));
    let sessions =
        crate::core::sessions::list_sessions_for_cwd(cwd, session_dir_path, true, None).await;
    sessions
        .into_iter()
        .find(|s| s.id.as_deref() == Some(session_id))
        .map(|s| s.path)
}

/// Run `--export` via the shared export engine.
fn run_export(input_path: &str, output_path: Option<&String>) -> Result<(), String> {
    let result = crate::core::export_html::export_from_file(
        input_path,
        crate::core::export_html::ExportOptions {
            output_path: output_path.map(PathBuf::from),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    ProductOutput::writeln(&format!("Exported to: {result}"));
    Ok(())
}

/// Collect extension CLI flag metadata from the runtime's resource loader.
///
/// Returns an empty list until the resource loader exposes its flag map; the
/// `--help` block still renders the canonical options.
fn collect_extension_flags(
    runtime: &std::sync::Arc<AgentSessionRuntime>,
) -> Vec<crate::cli::help::ExtensionFlagHelp> {
    let session = runtime.session();
    let Some(host_runner) = session.host_extension_runner() else {
        return Vec::new();
    };
    let registry = host_runner.registry();
    registry
        .flags()
        .iter()
        .map(|flag| {
            let extension_path = match flag.extension_path.as_deref() {
                Some(path) if !path.is_empty() => path.to_owned(),
                _ => "<extension>".to_owned(),
            };
            crate::cli::help::ExtensionFlagHelp {
                name: flag.name.clone(),
                description: flag.description.clone(),
                takes_value: matches!(flag.kind, pi_ext::adapters::FlagKind::String),
                extension_path,
            }
        })
        .collect()
}

/// Prepare the initial message and split remaining CLI messages.
///
/// Expands `@file` arguments through the shared image pipeline using the
/// runtime's `images.autoResize` setting, then merges with stdin and the
/// first CLI message in the same join order as the TypeScript reference.
async fn prepare_initial_message(
    parsed: &mut Args,
    stdin_content: Option<&str>,
    runtime: &std::sync::Arc<AgentSessionRuntime>,
    cwd: &str,
) -> (Option<String>, Vec<pi_ai::ImageContent>, Vec<String>) {
    if parsed.file_args.is_empty() {
        let prompt = crate::modes::print::build_initial_message(
            &mut parsed.messages,
            stdin_content,
            None,
            Vec::new(),
        );
        return (
            prompt.initial_message,
            prompt.initial_images,
            parsed.messages.clone(),
        );
    }
    let session = runtime.session();
    let auto_resize = session.lock_settings().get_image_auto_resize();
    let files = match crate::modes::print::process_file_arguments(
        &parsed.file_args,
        cwd,
        crate::modes::print::ProcessFileOptions {
            auto_resize_images: auto_resize,
        },
    )
    .await
    {
        Ok(files) => files,
        Err(err) => {
            // The TypeScript reference exits 1 on file-argument failures. The
            // bootstrap caller checks stderr; here we emit the message and
            // fall through with empty file content.
            ProductOutput::writeln(&format!("Error: {err}"));
            crate::modes::print::ProcessedFiles::default()
        }
    };
    let prompt = crate::modes::print::build_initial_message(
        &mut parsed.messages,
        stdin_content,
        Some(&files.text),
        files.images,
    );
    (
        prompt.initial_message,
        prompt.initial_images,
        parsed.messages.clone(),
    )
}

/// Detect the current dispatch platform.
fn current_platform() -> DispatchPlatform {
    if cfg!(target_os = "windows") {
        DispatchPlatform::Windows
    } else {
        DispatchPlatform::Unix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::{Mutex, MutexGuard};

    fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn validation_error(
        result: Result<(), FlagValidationError>,
        context: &str,
    ) -> Result<FlagValidationError, String> {
        match result {
            Ok(()) => Err(format!("{context}: validation unexpectedly succeeded")),
            Err(error) => Ok(error),
        }
    }

    fn assert_exit_code(outcome: BootstrapOutcome, expected: u8) -> Result<(), String> {
        match outcome {
            BootstrapOutcome::Exit { code, .. } => {
                assert_eq!(code, expected);
                Ok(())
            }
            BootstrapOutcome::Dispatch(dispatched) => Err(format!(
                "expected Exit with code {expected}, got Dispatch({:?})",
                dispatched.mode
            )),
        }
    }

    fn args_vec(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    /// In-memory I/O surface for tests.
    #[derive(Default)]
    struct FakeIo {
        env: Mutex<std::collections::HashMap<String, String>>,
        stdout: Mutex<Vec<String>>,
        stderr: Mutex<Vec<String>>,
        stdin_payload: Option<String>,
        stdin_is_tty: bool,
        stdout_is_tty: bool,
        cwd: PathBuf,
    }

    impl BootstrapIo for Arc<FakeIo> {
        fn env(&self, key: &str) -> Option<String> {
            lock_recover(&self.env).get(key).cloned()
        }
        fn set_env(&self, key: &str, value: &str) {
            lock_recover(&self.env).insert(key.to_owned(), value.to_owned());
        }
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }
        fn stdin_is_tty(&self) -> bool {
            self.stdin_is_tty
        }
        fn stdout_is_tty(&self) -> bool {
            self.stdout_is_tty
        }
        fn read_piped_stdin<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = io::Result<Option<String>>> + Send + 'a>> {
            Box::pin(async move { Ok(self.stdin_payload.clone()) })
        }
        fn write_stdout(&self, line: &str) {
            lock_recover(&self.stdout).push(line.to_owned());
        }
        fn write_stderr(&self, line: &str) {
            lock_recover(&self.stderr).push(line.to_owned());
        }
    }

    impl FakeIo {
        fn new() -> Self {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "PI_CODING_AGENT_DIR".to_owned(),
                std::env::temp_dir()
                    .join("pi-bootstrap-test")
                    .to_string_lossy()
                    .into_owned(),
            );
            Self {
                env: Mutex::new(env),
                stdout: Mutex::new(Vec::new()),
                stderr: Mutex::new(Vec::new()),
                stdin_payload: None,
                stdin_is_tty: true,
                stdout_is_tty: true,
                cwd: std::env::temp_dir(),
            }
        }

        fn stdout_lines(&self) -> Vec<String> {
            lock_recover(&self.stdout).clone()
        }

        fn stderr_lines(&self) -> Vec<String> {
            lock_recover(&self.stderr).clone()
        }
    }

    /// Fake runtime factory. Defaults to returning a fixed error so short-circuit
    /// tests can assert factory failures; set `succeed = true` when a real
    /// runtime handle is required (e.g. `PI_STARTUP_BENCHMARK` guard).
    #[derive(Default)]
    struct FakeFactory {
        calls: Mutex<Vec<String>>,
        supports_interactive: bool,
        /// When true, build a lightweight runtime with a non-`unknown` model.
        succeed: bool,
        /// Models exposed by the runtime's model catalog. Empty yields an empty
        /// availability list for `--list-models` tests.
        models: Vec<pi_ai::Model>,
    }

    #[derive(Clone)]
    struct StubProvider;

    impl pi_ai::Provider for StubProvider {
        fn stream(
            &self,
            _model: &pi_ai::Model,
            _context: pi_ai::Context,
            _options: pi_ai::StreamOptions,
        ) -> futures::stream::BoxStream<
            'static,
            Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>,
        > {
            Box::pin(futures::stream::empty())
        }
    }

    fn fake_runtime_model() -> pi_ai::Model {
        pi_ai::Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![pi_ai::ModelInput::Text],
            cost: pi_ai::ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    /// Build a deterministic in-memory model runtime seeded with `models`.
    ///
    /// Each model's provider is registered with a synthetic API key so the
    /// composed models appear in `get_available(None)`.
    async fn build_fake_model_runtime(
        models: Vec<pi_ai::Model>,
    ) -> Result<Arc<crate::core::model_runtime::ModelRuntime>, String> {
        use std::collections::BTreeMap;

        use crate::core::model_runtime::{
            CreateModelRuntimeOptions, ModelRuntime, ModelsJsonConfig, ProviderConfigInput,
            ProviderModelDefinition,
        };
        use pi_ai::auth::InMemoryCredentialStore;
        use pi_ai::models_store::InMemoryModelsStore;

        let mut grouped: BTreeMap<String, (ProviderConfigInput, Vec<ProviderModelDefinition>)> =
            BTreeMap::new();
        for model in models {
            let (_, defs) = grouped.entry(model.provider.clone()).or_insert_with(|| {
                (
                    ProviderConfigInput {
                        name: Some(model.provider.clone()),
                        api: Some(model.api.clone()),
                        base_url: Some(model.base_url.clone()),
                        ..Default::default()
                    },
                    Vec::new(),
                )
            });
            defs.push(ProviderModelDefinition {
                id: model.id,
                name: Some(model.name),
                api: Some(model.api),
                base_url: Some(model.base_url),
                reasoning: model.reasoning,
                thinking_level_map: model.thinking_level_map,
                input: Some(model.input),
                cost: Some(model.cost),
                context_window: Some(model.context_window),
                max_tokens: Some(model.max_tokens),
                headers: model.headers,
                compat: model.compat,
            });
        }
        let providers: BTreeMap<String, ProviderConfigInput> = grouped
            .into_iter()
            .map(|(id, (mut config, defs))| {
                config.models = Some(defs);
                (id, config)
            })
            .collect();

        let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: Some(Arc::new(InMemoryCredentialStore::new())),
            models_store: Some(Arc::new(InMemoryModelsStore::new())),
            models_config: Some(ModelsJsonConfig::from_providers(providers.clone())),
            allow_model_network: Some(false),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;
        for provider in providers.keys() {
            runtime
                .set_runtime_api_key(provider, "sk-test")
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(Arc::new(runtime))
    }

    #[derive(Default)]
    struct StubRuntimeFactory;

    impl crate::core::agent_session_runtime::CreateAgentSessionRuntimeFactory for StubRuntimeFactory {
        fn create(
            &self,
            _options: crate::core::agent_session_runtime::CreateAgentSessionRuntimeOptions,
        ) -> BoxFuture<
            '_,
            Result<
                crate::core::agent_session_runtime::CreateAgentSessionRuntimeResult,
                crate::core::agent_session_runtime::AgentSessionRuntimeError,
            >,
        > {
            Box::pin(async {
                Err(
                    crate::core::agent_session_runtime::AgentSessionRuntimeError::Factory(
                        "stub runtime factory is not expected to create replacements".to_owned(),
                    ),
                )
            })
        }
    }

    impl RuntimeFactory for Arc<FakeFactory> {
        fn create(
            &self,
            options: RuntimeFactoryOptions,
        ) -> BoxFuture<'_, Result<RuntimeHandle, String>> {
            lock_recover(&self.calls).push(format!(
                "create:{}:{}",
                options.cwd,
                options.session_manager.get_session_id()
            ));
            let succeed = self.succeed;
            let models = self.models.clone();
            Box::pin(async move {
                if !succeed {
                    return Err("__fake_factory_unavailable__".to_owned());
                }
                let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
                    Arc::new(StubProvider),
                    fake_runtime_model(),
                )
                .map_err(|e| e.to_string())?;
                config.model_runtime = Some(build_fake_model_runtime(models).await?);
                let session = crate::core::agent_session::AgentSession::new(config)
                    .map_err(|e| e.to_string())?;
                let runtime = AgentSessionRuntime::new(
                    session,
                    crate::core::agent_session_runtime::AgentSessionRuntimeServices {
                        cwd: PathBuf::from(options.cwd),
                        agent_dir: PathBuf::from(options.agent_dir),
                    },
                    Arc::new(StubRuntimeFactory),
                    Vec::new(),
                    None,
                );
                Ok(RuntimeHandle {
                    runtime: Arc::new(runtime),
                })
            })
        }
        fn supports_interactive(&self) -> bool {
            self.supports_interactive
        }
    }

    #[derive(Default, Clone)]
    struct CapturedOutput {
        inner: Arc<Mutex<CapturedInner>>,
    }

    #[derive(Default)]
    struct CapturedInner {
        status: Vec<String>,
        status_dim: Vec<String>,
        success: Vec<String>,
        error: Vec<String>,
    }

    impl PackageOutput for CapturedOutput {
        fn status(&self, line: &str) {
            lock_recover(&self.inner).status.push(line.to_owned());
        }
        fn status_dim(&self, line: &str) {
            lock_recover(&self.inner).status_dim.push(line.to_owned());
        }
        fn success(&self, line: &str) {
            lock_recover(&self.inner).success.push(line.to_owned());
        }
        fn error(&self, line: &str) {
            lock_recover(&self.inner).error.push(line.to_owned());
        }
    }

    #[derive(Default, Clone)]
    struct FakePackageHandler {
        trusted: bool,
        offline: Arc<Mutex<bool>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl PackageHandler for FakePackageHandler {
        fn install(&self, source: &str, local: bool) -> Result<(), String> {
            lock_recover(&self.calls).push(format!("install:{source}:{local}"));
            Ok(())
        }
        fn remove(&self, source: &str, local: bool) -> Result<bool, String> {
            lock_recover(&self.calls).push(format!("remove:{source}:{local}"));
            Ok(true)
        }
        fn list(&self) -> Result<Vec<package_manager_cli::ListedPackage>, String> {
            lock_recover(&self.calls).push("list".to_owned());
            Ok(Vec::new())
        }
        fn set_offline(&self, offline: bool) {
            *lock_recover(&self.offline) = offline;
        }
        fn is_project_trusted(&self) -> bool {
            self.trusted
        }
        fn refresh_models(&self) -> Result<(), String> {
            Ok(())
        }
        fn update_extensions(&self, source: Option<&str>) -> Result<(), String> {
            lock_recover(&self.calls).push(format!("update_ext:{source:?}"));
            Ok(())
        }
        fn update_self(&self, force: bool) -> Result<bool, String> {
            lock_recover(&self.calls).push(format!("update_self:{force}"));
            Ok(true)
        }
        fn open_config_selector(
            &self,
            local: bool,
            project_trust_override: Option<bool>,
        ) -> Result<(), String> {
            lock_recover(&self.calls)
                .push(format!("open_config:{local}:{project_trust_override:?}"));
            Ok(())
        }
    }

    // ----- resolve_app_mode tests -----------------------------------------

    #[test]
    fn resolve_app_mode_rpc_forces_non_interactive() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--mode", "rpc"]));
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Rpc);
    }

    #[test]
    fn resolve_app_mode_json_forces_non_interactive() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--mode", "json"]));
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Json);
    }

    #[test]
    fn resolve_app_mode_text_with_dual_tty_is_interactive() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--mode", "text"]));
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Interactive);
    }

    #[test]
    fn resolve_app_mode_print_flag_demotes() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--print"]));
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Print);
    }

    #[test]
    fn resolve_app_mode_non_tty_stdin_demotes() {
        let parsed = crate::cli::args::parse_args(&args_vec(&[]));
        assert_eq!(resolve_app_mode(&parsed, false, true), AppMode::Print);
    }

    #[test]
    fn resolve_app_mode_non_tty_stdout_demotes() {
        let parsed = crate::cli::args::parse_args(&args_vec(&[]));
        assert_eq!(resolve_app_mode(&parsed, true, false), AppMode::Print);
    }

    // ----- is_plain_runtime_metadata_command tests -----------------------

    #[test]
    fn plain_metadata_for_help_without_mode() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--help"]));
        assert!(is_plain_runtime_metadata_command(&parsed));
    }

    #[test]
    fn plain_metadata_for_list_models_without_mode() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--list-models"]));
        assert!(is_plain_runtime_metadata_command(&parsed));
    }

    #[test]
    fn plain_metadata_false_when_print_set() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--print", "--help"]));
        assert!(!is_plain_runtime_metadata_command(&parsed));
    }

    #[test]
    fn plain_metadata_false_when_mode_set() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--mode", "json", "--help"]));
        assert!(!is_plain_runtime_metadata_command(&parsed));
    }

    // ----- validators ------------------------------------------------------

    #[test]
    fn fork_conflicts_with_session() -> Result<(), String> {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--fork", "abc", "--session", "x"]));
        let err = validation_error(validate_fork_flags(&parsed), "fork with session")?;
        assert!(
            err.message
                .contains("--fork cannot be combined with --session")
        );
        Ok(())
    }

    #[test]
    fn fork_conflicts_with_continue_and_resume() -> Result<(), String> {
        let parsed =
            crate::cli::args::parse_args(&args_vec(&["--fork", "abc", "--continue", "--resume"]));
        let err = validation_error(
            validate_fork_flags(&parsed),
            "fork with continue and resume",
        )?;
        assert!(err.message.contains("--continue, --resume"));
        Ok(())
    }

    #[test]
    fn fork_alone_is_ok() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--fork", "abc"]));
        assert!(validate_fork_flags(&parsed).is_ok());
    }

    #[test]
    fn session_id_conflicts_with_continue() -> Result<(), String> {
        let parsed =
            crate::cli::args::parse_args(&args_vec(&["--session-id", "xyz", "--continue"]));
        let err = validation_error(
            validate_session_id_flags(&parsed),
            "session id with continue",
        )?;
        assert!(
            err.message
                .contains("--session-id cannot be combined with --continue")
        );
        Ok(())
    }

    #[test]
    fn session_id_alone_is_ok() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--session-id", "xyz"]));
        assert!(validate_session_id_flags(&parsed).is_ok());
    }

    #[test]
    fn name_rejects_empty_value() -> Result<(), String> {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--name", ""]));
        let err = validation_error(validate_name(&parsed), "empty name")?;
        assert_eq!(err.message, "--name requires a non-empty value");
        Ok(())
    }

    #[test]
    fn name_rejects_whitespace_only_value() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--name", "   "]));
        assert!(validate_name(&parsed).is_err());
    }

    #[test]
    fn name_accepts_non_empty_value() {
        let parsed = crate::cli::args::parse_args(&args_vec(&["--name", "my session"]));
        assert!(validate_name(&parsed).is_ok());
    }

    // ----- is_truthy_env_flag ---------------------------------------------

    #[test]
    fn truthy_env_flag_values() {
        assert!(!is_truthy_env_flag(None));
        assert!(is_truthy_env_flag(Some("1")));
        assert!(is_truthy_env_flag(Some("true")));
        assert!(is_truthy_env_flag(Some("TRUE")));
        assert!(is_truthy_env_flag(Some("yes")));
        assert!(!is_truthy_env_flag(Some("0")));
        assert!(!is_truthy_env_flag(Some("false")));
        assert!(!is_truthy_env_flag(Some("")));
    }

    // ----- bootstrap pipeline tests ---------------------------------------

    #[tokio::test]
    async fn bootstrap_version_short_circuit() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--version"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 0)?;
        let stdout = io.stdout_lines();
        assert!(!stdout.is_empty());
        assert!(stdout[0].chars().any(|c| c.is_ascii_digit()));
        assert!(lock_recover(&factory.calls).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_help_short_circuit_before_factory() -> Result<(), String> {
        // --help should NOT reach the factory (parsed but no runtime needed
        // when help is the plain-metadata short-circuit at exit-time). Note:
        // the bootstrap currently builds the runtime THEN renders help with
        // extension flags, matching main.ts. So the factory IS called. This
        // test documents the behavior.
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--help"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        // The fake factory returns an error, so the bootstrap exits 1 with
        // the error message. The point of this test is to document that the
        // factory is reached when help is requested (so extension flags can
        // be discovered).
        assert_exit_code(outcome, 1)?;
        assert!(!lock_recover(&factory.calls).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_parse_error_exits_one() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--name"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(stderr.iter().any(|s| s.contains("--name requires a value")));
        assert!(lock_recover(&factory.calls).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_thinking_warning_does_not_exit() -> Result<(), String> {
        // A warning diagnostic should not short-circuit; the bootstrap should
        // continue past the parse-diagnostics step. The fake factory then
        // returns an error, so we still exit 1 — but the warning must appear
        // on stderr first.
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--thinking", "bogus"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(stderr.iter().any(|s| s.contains("Invalid thinking level")));
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rpc_with_file_arg_exits_one() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--mode", "rpc", "@file.txt"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(
            stderr
                .iter()
                .any(|s| s.contains("@file arguments are not supported in RPC mode"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_fork_conflict_exits_one() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--fork", "abc", "--continue"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(
            stderr
                .iter()
                .any(|s| s.contains("--fork cannot be combined"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_package_command_short_circuits() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["list"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 0)?;
        assert!(lock_recover(&factory.calls).is_empty());
        assert!(!lock_recover(&pkg.calls).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_threads_offline_to_package_handler() {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let _ = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--version", "--offline"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert!(
            *lock_recover(&pkg.offline),
            "package handler must observe bootstrap offline mode"
        );
        // Production RealIo cannot mutate process env; FakeIo still records the
        // historical set_env call sites only if callers invoke them. We no longer
        // rely on env mutation for offline propagation.
    }

    #[tokio::test]
    async fn bootstrap_export_missing_file_exits_one() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--export", "/nonexistent/path/missing.jsonl"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(stderr.iter().any(|s| s.contains("Error: File not found")));
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_unknown_short_flag_is_parse_error() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["-Z"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(stderr.iter().any(|s| s.contains("Unknown option: -Z")));
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_factory_error_surfaces_verbatim() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory::default());
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--no-session", "hello"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(
            stderr
                .iter()
                .any(|s| s.contains("__fake_factory_unavailable__"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_pi_startup_benchmark_guard() -> Result<(), String> {
        // PI_STARTUP_BENCHMARK is set, mode is print (because stdin not TTY
        // would also demote). Use --print to force print mode cleanly.
        let io_state = FakeIo::new();
        lock_recover(&io_state.env).insert("PI_STARTUP_BENCHMARK".to_owned(), "1".to_owned());
        let io = Arc::new(io_state);
        let factory = Arc::new(FakeFactory {
            succeed: true,
            ..FakeFactory::default()
        });
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--print", "hello"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 1)?;
        let stderr = io.stderr_lines();
        assert!(
            stderr
                .iter()
                .any(|s| s.contains("PI_STARTUP_BENCHMARK only supports interactive mode"))
        );
        Ok(())
    }

    /// Flag snapshot the fake host serves for `extensions.load`.
    fn fake_flag_snapshot() -> serde_json::Value {
        serde_json::json!({
            "flags": [
                {
                    "name": "verbose-log",
                    "type": "boolean",
                    "description": "Enable verbose logging",
                    "extensionPath": "/plugins/logger"
                },
                {
                    "name": "api-url",
                    "type": "string",
                    "description": "Custom API URL",
                    "extensionPath": "/plugins/api"
                },
                {
                    "name": "fallback-flag",
                    "type": "boolean",
                    "extensionPath": "/plugins/fallback"
                },
                {
                    "name": "legacy-flag",
                    "type": "string"
                }
            ]
        })
    }

    /// Build a test runtime, optionally binding a host extension runner.
    /// Propagates typed construction errors instead of panicking.
    fn build_test_runtime(
        runner: Option<Arc<crate::core::extension_host::HostExtensionRunner>>,
    ) -> Result<Arc<AgentSessionRuntime>, String> {
        let cwd = std::env::current_dir().map_err(|e| format!("current_dir: {e}"))?;
        let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
            Arc::new(StubProvider),
            fake_runtime_model(),
        )
        .map_err(|e| format!("test_config: {e}"))?;
        config.host_extension_runner = runner;
        let session = crate::core::agent_session::AgentSession::new(config)
            .map_err(|e| format!("AgentSession::new: {e}"))?;
        Ok(Arc::new(AgentSessionRuntime::new(
            session,
            crate::core::agent_session_runtime::AgentSessionRuntimeServices {
                cwd: cwd.clone(),
                agent_dir: cwd,
            },
            Arc::new(StubRuntimeFactory),
            Vec::new(),
            None,
        )))
    }

    /// Answer one fake-host request line, propagating encode/serialization
    /// errors instead of unwrapping.
    async fn serve_fake_host_line(
        line: &str,
        snapshot: &serde_json::Value,
        writer: &mut tokio::io::DuplexStream,
    ) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let req =
            pi_ext::protocol::decode_frame_str(line).map_err(|e| format!("decode request: {e}"))?;
        let payload = if req.method == "hello" {
            serde_json::to_value(pi_ext::protocol::HelloAck::local())
                .map_err(|e| format!("encode hello: {e}"))?
        } else if req.method == "extensions.load" {
            snapshot.clone()
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };
        let resp = pi_ext::protocol::Frame {
            id: req.id,
            kind: pi_ext::protocol::FrameKind::Res,
            method: req.method,
            payload,
        };
        let bytes =
            pi_ext::protocol::encode_frame(&resp).map_err(|e| format!("encode frame: {e}"))?;
        writer
            .write_all(&bytes)
            .await
            .map_err(|e| format!("write frame: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("flush frame: {e}"))?;
        Ok(())
    }

    /// Spawn a fake extension host over a duplex pair. Encode/serialization
    /// failures are recorded into `errors` (never silently swallowed) and the
    /// task exits so the client observes EOF.
    fn spawn_fake_extension_host(
        host_from_client: tokio::io::DuplexStream,
        host_to_client: tokio::io::DuplexStream,
        snapshot: serde_json::Value,
        errors: Arc<Mutex<Vec<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncBufReadExt;
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(host_from_client);
            let mut writer = host_to_client;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Err(message) = serve_fake_host_line(&line, &snapshot, &mut writer).await {
                    lock_recover(&errors).push(message);
                    break;
                }
            }
        })
    }

    fn assert_extension_flag(
        flag: &crate::cli::help::ExtensionFlagHelp,
        name: &str,
        takes_value: bool,
        description: Option<&str>,
        extension_path: &str,
    ) {
        assert_eq!(flag.name, name);
        assert_eq!(flag.takes_value, takes_value);
        assert_eq!(flag.description.as_deref(), description);
        assert_eq!(flag.extension_path, extension_path);
    }

    #[tokio::test]
    async fn test_collect_extension_flags() -> Result<(), String> {
        // 1. No runner → empty flag list.
        let runtime = build_test_runtime(None)?;
        assert!(
            collect_extension_flags(&runtime).is_empty(),
            "expected empty flags when no runner is present"
        );

        // 2. Wire a fake host serving boolean/string flags with varied metadata.
        let (client_to_host, host_from_client) = tokio::io::duplex(64 * 1024);
        let (host_to_client, client_from_host) = tokio::io::duplex(64 * 1024);
        let (client_err, _host_err) = tokio::io::duplex(4096);
        let client = Arc::new(pi_ext::client::HostClient::connect_boxed(
            Box::new(client_to_host),
            Box::new(client_from_host),
            Box::new(client_err),
            None,
        ));
        let host_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let _host = spawn_fake_extension_host(
            host_from_client,
            host_to_client,
            fake_flag_snapshot(),
            Arc::clone(&host_errors),
        );

        let runner = crate::core::extension_host::HostExtensionRunner::connect(client, vec![])
            .await
            .map_err(|e| format!("HostExtensionRunner::connect: {e}"))?;

        let runtime_with_runner = build_test_runtime(Some(runner))?;
        let flags = collect_extension_flags(&runtime_with_runner);
        assert_eq!(flags.len(), 4, "expected four registered flags");

        // Order, description, takes_value, and extension_path mapping.
        assert_extension_flag(
            &flags[0],
            "verbose-log",
            false,
            Some("Enable verbose logging"),
            "/plugins/logger",
        );
        assert_extension_flag(
            &flags[1],
            "api-url",
            true,
            Some("Custom API URL"),
            "/plugins/api",
        );
        // No description → falls back to extension_path in help rendering.
        assert_extension_flag(&flags[2], "fallback-flag", false, None, "/plugins/fallback");
        // No description and no extensionPath → maps to "<extension>".
        assert_extension_flag(&flags[3], "legacy-flag", true, None, "<extension>");

        // Help rendering surfaces the description-fallback source.
        let help_text = crate::cli::help::format_help(
            Some(&flags),
            crate::cli::help::HelpStyle { styled: false },
        );
        assert!(
            help_text.contains("Registered by /plugins/fallback"),
            "help missing fallback-path registration line"
        );
        assert!(
            help_text.contains("Registered by <extension>"),
            "help missing generic-extension registration line"
        );
        // The fake host must not have silently hidden any encode/serialization failure.
        assert!(
            lock_recover(&host_errors).is_empty(),
            "fake host reported errors: {:?}",
            lock_recover(&host_errors).clone()
        );
        Ok(())
    }

    // ----- list-models tests -------------------------------------------------

    #[test]
    fn format_token_count_boundary_cases() {
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1K");
        assert_eq!(format_token_count(1500), "1.5K");
        assert_eq!(format_token_count(1_000_000), "1M");
        assert_eq!(format_token_count(1_500_000), "1.5M");
    }

    #[tokio::test]
    async fn bootstrap_list_models_empty_catalog_message() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory {
            succeed: true,
            ..FakeFactory::default()
        });
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--list-models"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 0)?;
        let stdout = io.stdout_lines();
        let expected = crate::core::agent_session_services::format_no_models_available_message();
        assert!(
            stdout.iter().any(|line| line.contains(&expected)),
            "expected empty-catalog message in stdout: {stdout:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_list_models_no_match_message() -> Result<(), String> {
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory {
            succeed: true,
            models: vec![fake_runtime_model()],
            ..FakeFactory::default()
        });
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--list-models", "NoMatch"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 0)?;
        let stdout = io.stdout_lines();
        assert!(
            stdout
                .iter()
                .any(|line| line.contains(r#"No models matching "NoMatch""#)),
            "no-match message must echo the user's token verbatim: {stdout:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_list_models_fuzzy_non_substring_match() -> Result<(), String> {
        let mut model = fake_runtime_model();
        model.provider = "openai".to_owned();
        model.id = "o3-mini".to_owned();
        model.name = model.id.clone();
        model.api = "openai".to_owned();
        model.base_url = "https://example.com".to_owned();
        let io = Arc::new(FakeIo::new());
        let factory = Arc::new(FakeFactory {
            succeed: true,
            models: vec![model],
            ..FakeFactory::default()
        });
        let pkg_out = CapturedOutput::default();
        let pkg = FakePackageHandler::default();
        let outcome = run_bootstrap(BootstrapInputs {
            args: args_vec(&["--list-models", "opmini"]),
            io: &io,
            factory: &factory,
            package_handler: &pkg,
            package_output: &pkg_out,
        })
        .await;
        assert_exit_code(outcome, 0)?;
        let stdout = io.stdout_lines();
        assert!(
            stdout
                .iter()
                .any(|line| line.contains("openai") && line.contains("o3-mini")),
            "expected fuzzy-matched model row in stdout: {stdout:?}"
        );
        Ok(())
    }
}
