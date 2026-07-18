//! Extension binding, reload, and replaced-context seam.
//!
//! Ports `bindExtensions`, `extendResourcesFromExtensions`,
//! `createReplacedSessionContext`, `hasExtensionHandlers`, and `reload` from
//! `coding-agent/src/core/agent-session.ts`.
//!
//! All extension interaction goes through the [`ExtensionRunner`] trait seam
//! (defined in `extension_runner.rs`). This module owns:
//! - binding UI/mode/command/error listeners (recorded locally; pushed to the
//!   runner once the trait gains `set_ui_context` / `bind_command_context`)
//! - extension-driven resource discovery (skills/prompts/themes)
//! - reload (preserves flag values, reloads settings, re-discovers resources,
//!   and re-emits `session_start{reload}` only when bindings exist)
//! - the replaced-session context handed to `withSession` after runtime swap
//! - extension error isolation (host errors never abort the session)
//!
//! ## Pending foundation work
//!
//! `AgentSessionEvent::SessionStart { reason, previous_session_file }` and
//! `AgentSessionEvent::SessionShutdown { reason, target_session_file }` are
//! not yet on the event enum (owned by the foundation slice). Until they
//! land, `bind_extensions` and `reload` record state + drive resource
//! discovery + `runner.shutdown()` but do not emit a typed lifecycle event.
//! The `has_handlers("session_start"|"session_shutdown")` gates still run so
//! pi-ext can observe handler presence.

use crate::core::resources::{
    ResourceLoader, SlashCommandInfo, SlashCommandSource, SyntheticSourceInfoOptions,
    create_synthetic_source_info,
};
use std::sync::{Arc, Mutex};

use super::AgentSession;
#[cfg(test)]
use super::events::AgentSessionEvent;

/// Reason passed to `session_start`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStartReason {
    /// First bind for this session.
    Startup,
    /// Bind after `/reload`.
    Reload,
    /// Bind after new/switch/fork/import.
    Resume,
}

impl SessionStartReason {
    /// Wire discriminant matching TS.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Reload => "reload",
            Self::Resume => "resume",
        }
    }
}

/// Reason passed to `session_shutdown`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionShutdownReason {
    /// New session replacing this one.
    New,
    /// Resume/switch/import replacing this one.
    Resume,
    /// Fork replacing this one.
    Fork,
    /// `/reload`.
    Reload,
    /// Runtime disposal.
    Quit,
}

impl SessionShutdownReason {
    /// Wire discriminant matching TS.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resume => "resume",
            Self::Fork => "fork",
            Self::Reload => "reload",
            Self::Quit => "quit",
        }
    }
}

/// Mode the session is bound to (mirrors `AppMode` minus `Interactive`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionMode {
    /// Interactive TUI.
    Tui,
    /// Print mode (text).
    Print,
    /// JSON print mode.
    Json,
    /// RPC server.
    Rpc,
}

impl ExtensionMode {
    /// Wire discriminant matching TS.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Print => "print",
            Self::Json => "json",
            Self::Rpc => "rpc",
        }
    }
}

/// UI context marker for extensions (opaque until pi-tui integration).
///
/// The interactive mode supplies a concrete `ExtensionUiContext`; other modes
/// pass `None`. Stored as an opaque tag so the host can detect presence.
#[derive(Clone, Debug, Default)]
pub struct ExtensionUiContext {
    /// Opaque caller-supplied tag (component handle, mode marker, etc.).
    pub tag: Option<String>,
}

/// Shared callback signature for extension error notifications.
pub type ExtensionErrorListener = Arc<dyn Fn(&str) + Send + Sync>;

/// Inputs accepted by [`AgentSession::bind_extensions`].
#[derive(Clone, Default)]
pub struct ExtensionBindings {
    /// Optional UI context (interactive only).
    pub ui_context: Option<ExtensionUiContext>,
    /// Mode the session is bound to.
    pub mode: Option<ExtensionMode>,
    /// Opaque command-context actions map (interactive/rpc).
    pub command_context_actions: Option<serde_json::Value>,
    /// Optional shutdown handler.
    pub shutdown_handler: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Optional error listener invoked when the host reports an extension error.
    pub on_error: Option<ExtensionErrorListener>,
    /// Override `session_start` reason (defaults to `Startup`).
    pub start_reason: Option<SessionStartReason>,
    /// Optional previous session file (carried into `session_start`).
    pub previous_session_file: Option<String>,
}

/// Context handed to `withSession` after a runtime replacement.
///
/// Records the new session id. `send_custom_message` / `send_user_message`
/// dispatch into the prompt slice once it lands; until then they record the
/// intent so tests can assert the runtime invoked the callback on the new
/// session.
#[derive(Clone)]
pub struct ReplacedSessionContext {
    /// Session id of the new session.
    pub session_id: String,
    /// Recorded custom-message sends (test observable).
    pub sent_custom_messages: Arc<Mutex<Vec<String>>>,
    /// Recorded user-message sends (test observable).
    pub sent_user_messages: Arc<Mutex<Vec<String>>>,
}

impl ReplacedSessionContext {
    /// Record a custom-message send (placeholder until prompt slice lands).
    pub fn send_custom_message(&self, message: impl Into<String>) {
        if let Ok(mut g) = self.sent_custom_messages.lock() {
            g.push(message.into());
        }
    }

    /// Record a user-message send (placeholder until prompt slice lands).
    pub fn send_user_message(&self, content: impl Into<String>) {
        if let Ok(mut g) = self.sent_user_messages.lock() {
            g.push(content.into());
        }
    }
}

/// Errors raised by extension binding / reload.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionBindError {
    /// `session_shutdown` runner call failed.
    #[error("extension shutdown failed: {0}")]
    Shutdown(super::extension_runner::ExtensionRunnerError),
    /// `resources_discover` failed.
    #[error("extension resource discovery failed: {0}")]
    ResourceDiscover(super::extension_runner::ExtensionRunnerError),
    /// Resource loader reload failed.
    #[error("resource reload failed: {0}")]
    ResourceReload(String),
    /// Host restart after reload failed.
    #[error("extension host restart failed: {0}")]
    HostRestart(String),
}

impl AgentSession {
    /// Returns true when at least one extension handler is registered for
    /// `event_type`. Cheap delegation to the runner; safe to call from any
    /// thread without locking session state.
    #[must_use]
    pub fn has_extension_handlers(&self, event_type: &str) -> bool {
        self.hooks.runner().has_handlers(event_type)
    }

    /// Bind extension UI/mode/error/shutdown listeners and drive resource
    /// discovery.
    ///
    /// Stores the bindings locally (forwarded to the runner once
    /// `set_ui_context` / `bind_command_context` join the trait) and invokes
    /// [`AgentSession::extend_resources_from_extensions`] with the bind
    /// reason.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError::ResourceDiscover`] when the runner fails
    /// to discover resources.
    pub async fn bind_extensions(
        &self,
        bindings: ExtensionBindings,
    ) -> Result<(), ExtensionBindError> {
        let reason = bindings.start_reason.unwrap_or(SessionStartReason::Startup);
        // Persist bindings on the inner state.
        {
            let mut inner = self.lock_inner();
            inner.extension_mode = bindings.mode;
            inner.extension_ui_tag = bindings.ui_context.as_ref().and_then(|c| c.tag.clone());
            inner
                .extension_shutdown_handler
                .clone_from(&bindings.shutdown_handler);
            inner
                .extension_error_listener
                .clone_from(&bindings.on_error);
            inner
                .extension_command_context
                .clone_from(&bindings.command_context_actions);
            inner
                .previous_session_file
                .clone_from(&bindings.previous_session_file);
        }

        // Reserved: emit SessionStart { reason, previous_session_file } once
        // AgentSessionEvent gains the typed variant. Until then, resource
        // discovery is the observable side effect.
        let _ = reason;

        self.extend_resources_from_extensions(reason.as_str())
            .await?;
        Ok(())
    }

    /// Discover skills/prompts/themes from extensions and merge into the
    /// resource loader.
    ///
    /// No-op when no `resources_discover` handlers are registered.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError::ResourceDiscover`] when the runner fails.
    pub async fn extend_resources_from_extensions(
        &self,
        reason: &str,
    ) -> Result<(), ExtensionBindError> {
        if reason == SessionStartReason::Startup.as_str()
            && self.lock_inner().initial_resources_discovered
        {
            return Ok(());
        }

        if reason == SessionStartReason::Reload.as_str()
            && let Some(loader) = &self.resource_loader
        {
            let mut loader = loader.lock().await;
            loader
                .reload()
                .await
                .map_err(|error| ExtensionBindError::ResourceReload(error.to_string()))?;
            self.apply_resource_snapshot(&loader);
        }

        let runner = self.hooks.runner();
        if !runner.has_handlers("resources_discover") {
            return Ok(());
        }
        let paths = runner
            .emit_resources_discover(&self.cwd, reason)
            .await
            .map_err(ExtensionBindError::ResourceDiscover)?;
        if let Some(loader) = &self.resource_loader {
            let mut loader = loader.lock().await;
            loader.extend_resources(paths);
            self.apply_resource_snapshot(&loader);
        }
        if reason == SessionStartReason::Startup.as_str() {
            self.lock_inner().initial_resources_discovered = true;
        }
        Ok(())
    }

    fn apply_resource_snapshot(&self, loader: &crate::core::resources::DefaultResourceLoader) {
        let skills = loader.get_skills().0.to_vec();
        let prompt_templates = loader.get_prompts().0.to_vec();
        let append = (!loader.get_append_system_prompt().is_empty())
            .then(|| loader.get_append_system_prompt().join("\n\n"));
        let selected_tools = self.lock_inner().active_tool_names.clone();
        let system_prompt = crate::core::system_prompt::build_system_prompt(
            &crate::core::system_prompt::BuildSystemPromptOptions {
                custom_prompt: loader.get_system_prompt().map(str::to_owned),
                selected_tools: Some(selected_tools),
                append,
                cwd: self.cwd.clone(),
                context_files: Some(loader.get_agents_files().to_vec()),
                skills: Some(skills.clone()),
                ..crate::core::system_prompt::BuildSystemPromptOptions::default()
            },
        );
        *self
            .skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = skills;
        *self
            .prompt_templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prompt_templates;
        self.lock_inner()
            .base_system_prompt
            .clone_from(&system_prompt);
        self.hooks.set_base_system_prompt(system_prompt.clone());
        self.hooks.set_system_prompt_override(None);
        self.agent.set_system_prompt(system_prompt);
    }

    /// Source label for an extension path (`extension:<basename-without-ext>`).
    ///
    /// Angle-bracketed names (in-memory extensions) are emitted verbatim
    /// minus the brackets.
    #[must_use]
    pub fn get_extension_source_label(extension_path: &str) -> String {
        if extension_path.starts_with('<') {
            let trimmed = extension_path.trim_start_matches('<').trim_end_matches('>');
            return format!("extension:{trimmed}");
        }
        let base = std::path::Path::new(extension_path)
            .file_stem()
            .map_or_else(
                || extension_path.to_owned(),
                |s| s.to_string_lossy().into_owned(),
            );
        format!("extension:{base}")
    }

    /// Reload extensions.
    ///
    /// Mirrors TS `reload`:
    /// 1. Capture previous flag values (preserved across the swap).
    /// 2. Emit `session_shutdown{reload}` when handlers exist.
    /// 3. When a concrete host is present: sequential restart-and-rewire
    ///    (await old transport reap exactly once, re-register providers,
    ///    restore flags, swap runner, refresh tools).
    /// 4. Reload base resources regardless of bindings.
    /// 5. Re-discover extension resources when the hook exists.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError`] on shutdown, host restart, or
    /// resource-discovery failure.
    pub async fn reload(&self) -> Result<(), ExtensionBindError> {
        let runner = self.hooks.runner();
        let previous_flag_values = runner.get_flag_values();

        // Lifecycle event when handlers are registered. Host transport reaping
        // is handled below even when no session_shutdown handlers exist.
        if runner.has_handlers("session_shutdown") {
            runner
                .shutdown(SessionShutdownReason::Reload.as_str())
                .await
                .map_err(ExtensionBindError::Shutdown)?;
        }

        if let Some(host) = self.host_extension_runner() {
            let Some(runtime) = self.model_runtime() else {
                // No runtime to re-register providers against: still reap the
                // old host so dispose paths stay single-reap clean.
                host.shutdown_once().await;
                self.set_host_extension_runner(None);
                self.hooks
                    .set_runner(Arc::new(super::extension_runner::NullExtensionRunner));
                self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                    .await?;
                return Ok(());
            };
            let new_host = host
                .restart_and_rewire(&runtime, previous_flag_values)
                .await
                .map_err(|error| ExtensionBindError::HostRestart(error.to_string()))?;
            // Swap trait runner + concrete host handle without downcast.
            self.hooks.set_runner(
                Arc::clone(&new_host) as Arc<dyn super::extension_runner::ExtensionRunner>
            );
            self.set_host_extension_runner(Some(new_host));
            // Refresh tools so newly registered extension tools replace the old set.
            self.refresh_tool_registry(&super::tools::RefreshToolRegistryOptions {
                active_tool_names: None,
                include_all_extension_tools: true,
            });
            self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                .await?;
            return Ok(());
        }

        // Trait-only / test path (no concrete host).
        self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
            .await?;
        Ok(())
    }

    /// Build the [`ReplacedSessionContext`] for `withSession` callbacks.
    ///
    /// Records the new session id. `send_custom_message` /
    /// `send_user_message` will dispatch into the prompt slice once it lands.
    pub async fn create_replaced_session_context(&self) -> ReplacedSessionContext {
        let session_id = self.session_id().await;
        ReplacedSessionContext {
            session_id,
            sent_custom_messages: Arc::new(Mutex::new(Vec::new())),
            sent_user_messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Build the current extension/prompt/skill slash-command catalog.
    #[must_use]
    pub fn slash_commands(&self) -> Vec<SlashCommandInfo> {
        let mut commands = Vec::new();
        let mut extension_names = std::collections::HashSet::new();

        if let Some(host) = self.host_extension_runner() {
            for command in host.registry().commands() {
                extension_names.insert(command.name.clone());
                let path = command
                    .source
                    .clone()
                    .unwrap_or_else(|| "<extension>".to_owned());
                commands.push(SlashCommandInfo {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    source: SlashCommandSource::Extension,
                    source_info: create_synthetic_source_info(
                        path,
                        SyntheticSourceInfoOptions {
                            source: "extension".to_owned(),
                            scope: None,
                            origin: None,
                            base_dir: None,
                        },
                    ),
                });
            }
        }

        for name in self.hooks.runner().get_registered_commands() {
            if extension_names.insert(name.clone()) {
                commands.push(SlashCommandInfo {
                    name,
                    description: None,
                    source: SlashCommandSource::Extension,
                    source_info: create_synthetic_source_info(
                        "<extension>",
                        SyntheticSourceInfoOptions {
                            source: "extension".to_owned(),
                            scope: None,
                            origin: None,
                            base_dir: None,
                        },
                    ),
                });
            }
        }

        commands.extend(
            self.prompt_templates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|template| SlashCommandInfo {
                    name: template.name.clone(),
                    description: Some(template.description.clone()),
                    source: SlashCommandSource::Prompt,
                    source_info: template.source_info.clone(),
                }),
        );
        commands.extend(
            self.skills
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|skill| SlashCommandInfo {
                    name: format!("skill:{}", skill.name),
                    description: Some(skill.description.clone()),
                    source: SlashCommandSource::Skill,
                    source_info: skill.source_info.clone(),
                }),
        );
        commands
    }

    /// Report an extension error to the registered listener (isolation boundary).
    pub fn report_extension_error(&self, message: impl Into<String>) {
        let message = message.into();
        let listener = self.lock_inner().extension_error_listener.clone();
        if let Some(listener) = listener {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&message)));
        }
    }

    /// Invoke the shutdown handler, if one is bound.
    pub fn invoke_extension_shutdown_handler(&self) {
        let handler = self.lock_inner().extension_shutdown_handler.clone();
        if let Some(handler) = handler {
            handler();
        }
    }

    /// Snapshot of the currently bound extension mode.
    #[must_use]
    pub fn extension_mode(&self) -> Option<ExtensionMode> {
        self.lock_inner().extension_mode
    }

    /// Snapshot of the previous session file (if any).
    #[must_use]
    pub fn previous_session_file(&self) -> Option<String> {
        self.lock_inner().previous_session_file.clone()
    }

    /// Concrete host runner handle (no trait downcast).
    #[must_use]
    pub fn host_extension_runner(
        &self,
    ) -> Option<Arc<crate::core::extension_host::HostExtensionRunner>> {
        self.host_extension_runner
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the concrete host runner handle (reload path).
    pub fn set_host_extension_runner(
        &self,
        runner: Option<Arc<crate::core::extension_host::HostExtensionRunner>>,
    ) {
        if let Ok(mut guard) = self.host_extension_runner.write() {
            *guard = runner;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_session::extension_runner::ExtensionRunner;
    use crate::core::agent_session::{AgentSession, AgentSessionConfig};
    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider, ProviderError,
        StreamOptions,
    };
    use std::collections::HashMap;
    use std::error::Error;
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn test_model() -> Model {
        Model {
            id: "m".to_owned(),
            name: "m".to_owned(),
            api: "test-api".to_owned(),
            provider: "test-provider".to_owned(),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 8_192,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[derive(Clone)]
    struct StubProvider;

    impl Provider for StubProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            stream::empty().boxed()
        }
    }

    fn make_session() -> Result<Arc<AgentSession>, crate::core::agent_session::AgentSessionError> {
        let config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        AgentSession::new(config)
    }

    fn locked_clone<T: Clone>(value: &Mutex<T>, label: &str) -> TestResult<T> {
        value
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| io::Error::other(format!("{label} mutex poisoned")).into())
    }

    /// Runner that records calls and supports toggling handler presence.
    struct TestRunner {
        has_start: AtomicBool,
        has_shutdown: AtomicBool,
        has_resources: AtomicBool,
        shutdown_calls: Arc<Mutex<Vec<String>>>,
        resource_calls: Arc<Mutex<Vec<String>>>,
        flag_values: Arc<Mutex<HashMap<String, serde_json::Value>>>,
        resource_paths: Arc<Mutex<crate::core::resources::ResourceExtensionPaths>>,
    }

    impl TestRunner {
        fn new() -> Self {
            Self {
                has_start: AtomicBool::new(false),
                has_shutdown: AtomicBool::new(false),
                has_resources: AtomicBool::new(false),
                shutdown_calls: Arc::new(Mutex::new(Vec::new())),
                resource_calls: Arc::new(Mutex::new(Vec::new())),
                flag_values: Arc::new(Mutex::new(HashMap::new())),
                resource_paths: Arc::new(Mutex::new(
                    crate::core::resources::ResourceExtensionPaths::default(),
                )),
            }
        }
    }

    impl ExtensionRunner for TestRunner {
        fn has_handlers(&self, event: &str) -> bool {
            match event {
                "session_start" => self.has_start.load(Ordering::SeqCst),
                "session_shutdown" => self.has_shutdown.load(Ordering::SeqCst),
                "resources_discover" => self.has_resources.load(Ordering::SeqCst),
                _ => false,
            }
        }

        fn emit(
            &self,
            _event: AgentSessionEvent,
        ) -> BoxFuture<
            '_,
            Result<
                Option<super::super::extension_runner::CancelResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_message_end(
            &self,
            message: pi_agent::AgentMessage,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::AgentMessage>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async move { Ok(Some(message)) })
        }

        fn emit_tool_call(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: serde_json::Map<String, serde_json::Value>,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::BeforeToolCallResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_tool_result(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: serde_json::Map<String, serde_json::Value>,
            _content: Vec<pi_ai::ToolResultContent>,
            _details: serde_json::Value,
            _is_error: bool,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::AfterToolCallResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_input(
            &self,
            _text: &str,
            _images: Option<serde_json::Value>,
            _source: &str,
            _streaming_behavior: Option<&str>,
        ) -> BoxFuture<
            '_,
            Result<
                super::super::extension_runner::InputTransformResult,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(super::super::extension_runner::InputTransformResult::default()) })
        }

        fn emit_before_agent_start(
            &self,
            _prompt: &str,
            _images: Option<serde_json::Value>,
        ) -> BoxFuture<
            '_,
            Result<
                Option<super::super::extension_runner::BeforeAgentStartResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_resources_discover(
            &self,
            cwd: &str,
            reason: &str,
        ) -> BoxFuture<
            '_,
            Result<
                crate::core::resources::ResourceExtensionPaths,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            let entry = format!("{cwd}:{reason}");
            if let Ok(mut g) = self.resource_calls.lock() {
                g.push(entry);
            }
            let paths = self
                .resource_paths
                .lock()
                .map(|paths| paths.clone())
                .unwrap_or_default();
            Box::pin(async move { Ok(paths) })
        }

        fn get_registered_commands(&self) -> Vec<String> {
            Vec::new()
        }

        fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn pi_agent::AgentTool>> {
            HashMap::new()
        }

        fn get_flag_values(&self) -> HashMap<String, serde_json::Value> {
            self.flag_values
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default()
        }
        fn execute_command(
            &self,
            _name: &str,
            _args: &str,
        ) -> BoxFuture<'_, Result<bool, super::super::extension_runner::ExtensionRunnerError>>
        {
            Box::pin(async { Ok(false) })
        }

        fn invalidate(&self) {}

        fn emit_error(&self, _message: String) {}

        fn shutdown(
            &self,
            reason: &str,
        ) -> BoxFuture<'_, Result<(), super::super::extension_runner::ExtensionRunnerError>>
        {
            if let Ok(mut g) = self.shutdown_calls.lock() {
                g.push(reason.to_owned());
            }
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn has_extension_handlers_delegates_to_runner() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;
        assert!(!session.has_extension_handlers("session_start"));
        runner.has_start.store(true, Ordering::SeqCst);
        assert!(session.has_extension_handlers("session_start"));
        Ok(())
    }

    #[tokio::test]
    async fn bind_extensions_records_bindings_and_discovers_resources() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_resources.store(true, Ordering::SeqCst);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;

        let error_hit = Arc::new(AtomicBool::new(false));
        let error_hit_clone = Arc::clone(&error_hit);
        let bindings = ExtensionBindings {
            mode: Some(ExtensionMode::Rpc),
            on_error: Some(Arc::new(move |_msg: &str| {
                error_hit_clone.store(true, Ordering::SeqCst);
            })),
            previous_session_file: Some("/old/path".into()),
            ..Default::default()
        };
        session.bind_extensions(bindings).await?;

        // Resource discovery invoked with "startup".
        let resource_calls = locked_clone(&runner.resource_calls, "resource calls")?;
        assert!(resource_calls.iter().any(|c| c.ends_with(":startup")));

        // Bindings recorded.
        assert_eq!(session.extension_mode(), Some(ExtensionMode::Rpc));
        assert_eq!(
            session.previous_session_file().as_deref(),
            Some("/old/path")
        );

        // Error listener is routed.
        session.report_extension_error("boom");
        assert!(error_hit.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn extension_resources_refresh_session_skills_and_system_prompt() -> TestResult {
        let temp = tempfile::tempdir()?;
        let cwd = temp.path().join("project");
        let agent_dir = temp.path().join("agent");
        let extension_dir = temp.path().join("extension");
        let skill_dir = extension_dir.join("skills");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent_dir)?;
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: extension-skill\ndescription: extension\n---\nbody\n",
        )?;
        let extension_path = extension_dir.join("plugin.ts");
        std::fs::write(&extension_path, "")?;
        let extension_path = extension_path.to_string_lossy().into_owned();

        let runner = Arc::new(TestRunner::new());
        runner.has_resources.store(true, Ordering::SeqCst);
        *runner
            .resource_paths
            .lock()
            .map_err(|_| io::Error::other("resource paths mutex poisoned"))? =
            crate::core::resources::ResourceExtensionPaths {
                skill_paths: vec![crate::core::resources::ExtensionResourcePath::discovered(
                    "skills".to_owned(),
                    &extension_path,
                )],
                ..crate::core::resources::ResourceExtensionPaths::default()
            };

        let settings = crate::core::settings::SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            crate::core::settings::SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let mut loader = crate::core::resources::DefaultResourceLoader::new(
            crate::core::resources::DefaultResourceLoaderOptions {
                cwd: cwd.clone(),
                agent_dir,
                settings_manager: Some(settings),
                ..crate::core::resources::DefaultResourceLoaderOptions::default()
            },
        );
        loader.reload().await?;
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.cwd = cwd.to_string_lossy().into_owned();
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        config.initial_active_tool_names = Some(vec!["read".to_owned()]);
        config.tools = vec![Arc::new(crate::core::tools::read::ReadTool::new(&cwd))];
        config.resource_loader = Some(loader);
        config.system_prompt = "stale".to_owned();
        let session = AgentSession::new(config)?;

        session
            .bind_extensions(ExtensionBindings::default())
            .await?;
        assert!(
            session
                .agent
                .state()
                .system_prompt
                .contains("<name>extension-skill</name>")
        );
        assert!(
            session
                .skills
                .lock()
                .map_err(|_| io::Error::other("skills mutex poisoned"))?
                .iter()
                .any(|skill| skill.name == "extension-skill")
        );

        *runner
            .resource_paths
            .lock()
            .map_err(|_| io::Error::other("resource paths mutex poisoned"))? =
            crate::core::resources::ResourceExtensionPaths::default();
        session.reload().await?;
        assert!(
            !session
                .agent
                .state()
                .system_prompt
                .contains("extension-skill")
        );
        assert!(
            session
                .skills
                .lock()
                .map_err(|_| io::Error::other("skills mutex poisoned"))?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn reload_emits_shutdown_and_rediscover_resources_when_bound() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_shutdown.store(true, Ordering::SeqCst);
        runner.has_resources.store(true, Ordering::SeqCst);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;

        session
            .bind_extensions(ExtensionBindings {
                mode: Some(ExtensionMode::Rpc),
                ..Default::default()
            })
            .await?;
        runner
            .resource_calls
            .lock()
            .map_err(|_| io::Error::other("resource calls mutex poisoned"))?
            .clear();

        session.reload().await?;

        let shutdown_calls = locked_clone(&runner.shutdown_calls, "shutdown calls")?;
        assert_eq!(shutdown_calls, vec!["reload".to_owned()]);

        let resource_calls = locked_clone(&runner.resource_calls, "resource calls")?;
        assert!(resource_calls.iter().any(|c| c.ends_with(":reload")));
        Ok(())
    }

    #[tokio::test]
    async fn reload_rediscovers_resources_without_bindings() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_shutdown.store(true, Ordering::SeqCst);
        runner.has_resources.store(true, Ordering::SeqCst);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;
        session.reload().await?;
        let shutdown_calls = locked_clone(&runner.shutdown_calls, "shutdown calls")?;
        assert_eq!(shutdown_calls, vec!["reload".to_owned()]);
        let resource_calls = locked_clone(&runner.resource_calls, "resource calls")?;
        assert!(
            resource_calls.iter().any(|call| call.ends_with(":reload")),
            "reload must refresh resources even without bindings, got {resource_calls:?}"
        );
        Ok(())
    }

    struct FailingRunner;

    impl ExtensionRunner for FailingRunner {
        fn has_handlers(&self, _event: &str) -> bool {
            true
        }

        fn shutdown(
            &self,
            _reason: &str,
        ) -> BoxFuture<'_, Result<(), super::super::extension_runner::ExtensionRunnerError>>
        {
            Box::pin(async {
                Err(
                    super::super::extension_runner::ExtensionRunnerError::Failed(
                        "host gone".into(),
                    ),
                )
            })
        }

        // All other methods default to null-runner behavior.
        fn emit(
            &self,
            _event: AgentSessionEvent,
        ) -> BoxFuture<
            '_,
            Result<
                Option<super::super::extension_runner::CancelResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_message_end(
            &self,
            message: pi_agent::AgentMessage,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::AgentMessage>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async move { Ok(Some(message)) })
        }

        fn emit_tool_call(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: serde_json::Map<String, serde_json::Value>,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::BeforeToolCallResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_tool_result(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: serde_json::Map<String, serde_json::Value>,
            _content: Vec<pi_ai::ToolResultContent>,
            _details: serde_json::Value,
            _is_error: bool,
        ) -> BoxFuture<
            '_,
            Result<
                Option<pi_agent::AfterToolCallResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_input(
            &self,
            _text: &str,
            _images: Option<serde_json::Value>,
            _source: &str,
            _streaming_behavior: Option<&str>,
        ) -> BoxFuture<
            '_,
            Result<
                super::super::extension_runner::InputTransformResult,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(super::super::extension_runner::InputTransformResult::default()) })
        }

        fn emit_before_agent_start(
            &self,
            _prompt: &str,
            _images: Option<serde_json::Value>,
        ) -> BoxFuture<
            '_,
            Result<
                Option<super::super::extension_runner::BeforeAgentStartResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_resources_discover(
            &self,
            _cwd: &str,
            _reason: &str,
        ) -> BoxFuture<
            '_,
            Result<
                crate::core::resources::ResourceExtensionPaths,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(crate::core::resources::ResourceExtensionPaths::default()) })
        }

        fn get_registered_commands(&self) -> Vec<String> {
            Vec::new()
        }

        fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn pi_agent::AgentTool>> {
            HashMap::new()
        }

        fn get_flag_values(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn execute_command(
            &self,
            _name: &str,
            _args: &str,
        ) -> BoxFuture<'_, Result<bool, super::super::extension_runner::ExtensionRunnerError>>
        {
            Box::pin(async { Ok(false) })
        }

        fn invalidate(&self) {}
        fn emit_error(&self, _message: String) {}
    }

    #[tokio::test]
    async fn reload_propagates_shutdown_error() -> TestResult {
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(Arc::new(FailingRunner) as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;
        let err = match session.reload().await {
            Ok(()) => return Err(io::Error::other("reload unexpectedly succeeded").into()),
            Err(err) => err,
        };
        assert!(matches!(err, ExtensionBindError::Shutdown(_)));
        Ok(())
    }

    #[tokio::test]
    async fn create_replaced_session_context_carries_session_id() -> TestResult {
        let session = make_session()?;
        let ctx = session.create_replaced_session_context().await;
        assert!(!ctx.session_id.is_empty());
        ctx.send_custom_message("hi");
        ctx.send_user_message("yo");
        assert_eq!(
            locked_clone(&ctx.sent_custom_messages, "custom messages")?.len(),
            1
        );
        assert_eq!(
            locked_clone(&ctx.sent_user_messages, "user messages")?.len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_extension_source_label_strips_extension() -> TestResult {
        assert_eq!(
            AgentSession::get_extension_source_label("/foo/bar/myext.ts"),
            "extension:myext"
        );
        assert_eq!(
            AgentSession::get_extension_source_label("<inline>"),
            "extension:inline"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_error_listener_receives_exact_message_and_is_isolated() -> TestResult {
        let session = make_session()?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);
        session
            .bind_extensions(ExtensionBindings {
                on_error: Some(Arc::new(move |message| {
                    received_clone
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message.to_owned());
                })),
                ..Default::default()
            })
            .await?;
        session.report_extension_error("host crashed");
        assert_eq!(
            received
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["host crashed"]
        );

        session
            .bind_extensions(ExtensionBindings {
                on_error: Some(Arc::new(|_| panic!("listener panic"))),
                ..Default::default()
            })
            .await?;
        session.report_extension_error("boom");
        assert!(!session.session_id().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bind_extensions_with_null_runner_is_noop() -> TestResult {
        let session = make_session()?;
        session
            .bind_extensions(ExtensionBindings {
                mode: Some(ExtensionMode::Print),
                ..Default::default()
            })
            .await?;
        assert!(!session.has_extension_handlers("session_start"));
        Ok(())
    }

    #[tokio::test]
    async fn invoke_extension_shutdown_handler_calls_bound_closure() -> TestResult {
        let session = make_session()?;
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        session
            .bind_extensions(ExtensionBindings {
                shutdown_handler: Some(Arc::new(move || {
                    called_clone.store(true, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .await?;
        session.invoke_extension_shutdown_handler();
        assert!(called.load(Ordering::SeqCst));
        Ok(())
    }
}
