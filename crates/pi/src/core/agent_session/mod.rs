//! `AgentSession` — mode-agnostic orchestration over `pi-agent` + product services.
//!
//! This module owns the event/persistence foundation:
//! - [`AgentSession`] / [`AgentSessionConfig`] / [`AgentSessionInner`]
//! - raw-serde [`AgentSessionEvent`] superset
//! - [`ExtensionRunner`] seam + [`NullExtensionRunner`]
//! - [`SessionHooks`] shared with agent tool / next-turn closures
//! - exactly one lossless event pump (extension-before-public)
//!
//! Sibling modules (prompt, retry, compaction, model, tools, bash, tree,
//! extension) add `impl AgentSession` blocks in later slices. Only
//! `pub(super)` invariants needed by those slices are exposed here.
//!
//! # Lock order
//!
//! Never hold a sync lock across `.await`. Never nest locks out of order:
//!
//! 0. `bind_lock` (`tokio::sync::Mutex`) — serializes the entire
//!    `bind_extensions` lifecycle and the whole `reload` transaction; held
//!    across `.await`. Acquire before any `lock_inner()`.
//! 1. `AgentSessionInner` (`std::sync::Mutex`) — flags, mirror queues, retry
//!    counters, listener list, pump handle, cancellation slots.
//! 2. `session_manager` (`tokio::sync::Mutex<SessionManager>`) — single-writer
//!    async mutex; event pump and public mutators share it. Documented as the
//!    sole writer of the append-only session tree for this session.
//! 3. `SessionHooks` `RwLocks` (`runner` → `system_prompt` → `tools`) — only one
//!    at a time, never nested with (1) or (2).
//!
//! Public listeners are invoked without holding any lock.

pub mod bash;
pub mod compaction;
pub mod events;
pub mod extension;
pub mod extension_runner;
pub mod model;
pub mod persistence;
pub mod prompt;
pub mod retry;
pub mod stats;
pub mod subscribe;
pub mod tools;
pub mod tree;

pub use events::{
    AgentSessionEvent, AgentSessionEventListener, CompactionReason, ModelSelectSource,
    SessionBeforeForkPosition, SessionBeforeSwitchReason, SessionShutdownReason, SessionStartEvent,
    SessionStartReason,
};
pub use extension::{
    ExtensionBindError, ExtensionBindings, ExtensionMode, ExtensionUiContext,
    ReplacedSessionContext,
};
pub use extension_runner::{
    BeforeAgentStartResult, CancelResult, ExtensionRunner, ExtensionRunnerError,
    InputTransformResult, NullExtensionRunner, SessionHooks, SystemPromptState,
};

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_agent::{Agent, AgentLoopConfig, AgentMessage, AgentOptions, AgentTool, QueueMode};
use pi_ai::{AssistantMessage, Model, ModelThinkingLevel, Provider};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::core::model_runtime::ModelRuntime;
use crate::core::sessions::{SessionError, SessionManager};
use crate::core::settings::SettingsManager;
use events::AgentSessionEvent as Event;
use subscribe::EventPump;

/// Optional model + thinking pair for `--models` scoped cycling.
#[derive(Clone, Debug)]
pub struct ScopedModel {
    /// Model entry.
    pub model: Model,
    /// Optional thinking level override for this model.
    pub thinking_level: Option<ModelThinkingLevel>,
}

/// Construction inputs for [`AgentSession`].
///
/// The services factory builds this and passes it to [`AgentSession::new`].
/// Product dependencies remain concrete: `model_runtime` is a typed handle,
/// while compaction test overrides use their own typed seam.
pub struct AgentSessionConfig {
    /// Pre-built agent. When `None`, [`AgentSession::new`] builds one from
    /// `provider` + defaults and installs `SessionHooks` closures.
    pub agent: Option<Agent>,
    /// Provider used when `agent` is `None`.
    pub provider: Option<Arc<dyn Provider>>,
    /// Session persistence manager (moved into an async mutex).
    pub session_manager: SessionManager,
    /// Settings manager (owned; later slices mutate retry/compaction flags).
    pub settings_manager: SettingsManager,
    /// Working directory.
    pub cwd: String,
    /// Scoped models from `--models`.
    pub scoped_models: Vec<ScopedModel>,
    /// Initial active built-in tool names.
    pub initial_active_tool_names: Option<Vec<String>>,
    /// Optional tool allowlist.
    pub allowed_tool_names: Option<Vec<String>>,
    /// Optional tool denylist.
    pub excluded_tool_names: Option<Vec<String>>,
    /// Initial tools installed on the agent when building from provider.
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Initial system prompt.
    pub system_prompt: String,
    /// Initial model (when building agent).
    pub model: Option<Model>,
    /// Initial thinking level.
    pub thinking_level: ModelThinkingLevel,
    /// Initial transcript messages.
    pub messages: Vec<AgentMessage>,
    /// Extension runner (defaults to [`NullExtensionRunner`]).
    pub extension_runner: Option<Arc<dyn ExtensionRunner>>,
    /// Concrete host runner retained for reload/restart (no trait downcast).
    pub host_extension_runner: Option<Arc<crate::core::extension_host::HostExtensionRunner>>,
    /// Typed model/auth runtime used by model selection and compaction.
    pub model_runtime: Option<Arc<ModelRuntime>>,
    /// Optional compaction stream override for tests and headless integrations.
    pub compaction_stream_override: Option<compaction::CompactionStreamHandle>,
    /// Skills for `/skill:name` expansion (populated by resources slice).
    pub skills: Vec<crate::core::resources::skills::Skill>,
    /// Prompt templates for `/template` expansion.
    pub prompt_templates: Vec<crate::core::resources::prompts::PromptTemplate>,
    /// Resource loader retained for extension-driven resource refreshes.
    pub resource_loader: Option<crate::core::resources::DefaultResourceLoader>,
    /// Session-start metadata emitted to extensions on first bind
    /// (`None` = default startup).
    pub session_start_event: Option<SessionStartEvent>,
    /// Base agent loop config overrides (hooks are always installed by session).
    pub base_config: Option<AgentLoopConfig>,
}

impl AgentSessionConfig {
    /// Minimal config for tests: in-memory session + null extensions.
    ///
    /// `SessionManager::in_memory` with no custom id only fails if validation
    /// is introduced later; the `None` options path is the documented default.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSessionError::Session`] if both in-memory session
    /// construction attempts fail.
    pub fn test_config(
        provider: Arc<dyn Provider>,
        model: Model,
    ) -> Result<Self, AgentSessionError> {
        let session_manager = SessionManager::in_memory(Some("."), None)
            .or_else(|_| SessionManager::in_memory(None, None))?;
        let settings_manager = SettingsManager::in_memory(
            &crate::core::settings::Settings::default(),
            crate::core::settings::SettingsManagerCreateOptions {
                project_trusted: true,
            },
        );
        Ok(Self {
            agent: None,
            provider: Some(provider),
            session_manager,
            settings_manager,
            cwd: ".".to_owned(),
            scoped_models: Vec::new(),
            initial_active_tool_names: None,
            allowed_tool_names: None,
            excluded_tool_names: None,
            tools: Vec::new(),
            system_prompt: String::new(),
            model: Some(model),
            thinking_level: ModelThinkingLevel::Off,
            messages: Vec::new(),
            extension_runner: None,
            host_extension_runner: None,
            model_runtime: None,
            compaction_stream_override: None,
            skills: Vec::new(),
            prompt_templates: Vec::new(),
            resource_loader: None,
            session_start_event: None,
            base_config: None,
        })
    }
}

/// Build host-command path provenance from the `ResourceLoader` extension snapshot.
pub(super) fn extension_source_infos(
    loader: Option<&crate::core::resources::DefaultResourceLoader>,
) -> std::collections::HashMap<String, crate::core::resources::SourceInfo> {
    let Some(loader) = loader else {
        return std::collections::HashMap::new();
    };
    crate::core::resources::ResourceLoader::get_extensions(loader)
        .paths
        .iter()
        .flat_map(|extension| {
            let configured = (extension.path.clone(), extension.source_info.clone());
            let resolved = (!extension.resolved_path.is_empty()).then(|| {
                let mut source_info = extension.source_info.clone();
                source_info.path.clone_from(&extension.resolved_path);
                (extension.resolved_path.clone(), source_info)
            });
            std::iter::once(configured).chain(resolved)
        })
        .collect()
}

/// Remove transcript messages explicitly excluded from provider context.
///
/// The flag is product metadata on custom transcript messages, not a
/// role-specific behavior: every custom role that sets `excludeFromContext`
/// must be absent from provider context and context-token estimates.
pub(super) fn retain_context_visible_messages(messages: &mut Vec<AgentMessage>) {
    messages.retain(|message| {
        !matches!(
            message,
            AgentMessage::Custom(custom)
                if custom.payload.get("excludeFromContext")
                    == Some(&serde_json::Value::Bool(true))
        )
    });
}

/// Build the product-owned context converter for every constructed session.
fn product_convert_to_llm_hook() -> pi_agent::ConvertToLlm {
    Arc::new(|mut messages| {
        Box::pin(async move {
            retain_context_visible_messages(&mut messages);
            crate::core::messages::convert_to_llm(&messages)
                .map_err(|error| pi_agent::AgentLoopError::message(error.to_string()))
        })
    })
}

/// Mutable session state shared by the event pump and public methods.
///
/// Guarded by `std::sync::Mutex`. Never hold across `.await`.
pub(super) struct AgentSessionInner {
    /// Session lifecycle and automatic-action flags.
    lifecycle: SessionLifecycle,
    /// Pending steering message texts for UI (mirror of agent queue).
    pub(super) steering_messages: Vec<String>,
    /// Pending follow-up message texts for UI.
    pub(super) follow_up_messages: Vec<String>,
    /// Current auto-retry attempt (0 = not retrying).
    pub(super) retry_attempt: u32,
    /// Max retries from settings (cached for `will_retry` checks).
    pub(super) max_retries: u32,
    /// Last assistant message observed on `message_end`.
    pub(super) last_assistant_message: Option<AssistantMessage>,
    /// Public event listeners with stable ids for unsubscribe.
    pub(super) listeners: Vec<(u64, AgentSessionEventListener)>,
    /// Monotonic listener id allocator.
    pub(super) next_listener_id: u64,
    /// Awaited event backpressure hooks with stable ids.
    pub(super) backpressure_hooks: Vec<(u64, EventBackpressureHook)>,
    /// Monotonic backpressure-hook id allocator.
    pub(super) next_backpressure_hook_id: u64,
    /// Typed persistence failure awaiting prompt completion.
    pub(super) pending_session_error: Option<SessionError>,
    /// Active event pump (at most one), encapsulated within this module.
    pump: Option<EventPump>,
    /// Idle waiters for session-level idle.
    pub(super) idle_notify: Arc<Notify>,
    /// Completed `agent_end` events processed by the session event pump.
    pub(super) processed_agent_ends: u64,
    /// Wakes prompt lifecycle barriers after a complete `agent_end`.
    pub(super) agent_end_notify: Arc<Notify>,
    /// Cancels prompt lifecycle barriers when the event pump disconnects.
    pub(super) agent_end_wait_cancel: CancellationToken,
    /// Scoped models list.
    pub(super) scoped_models: Vec<ScopedModel>,
    /// Active tool names.
    pub(super) active_tool_names: Vec<String>,
    /// Base system prompt (mirrored into `SessionHooks`).
    pub(super) base_system_prompt: String,
    /// Cancellation: retry sleep.
    pub(super) retry_abort: Option<CancellationToken>,
    /// Cancellation: manual compaction.
    pub(super) compaction_abort: Option<CancellationToken>,
    /// Cancellation: auto compaction.
    pub(super) auto_compaction_abort: Option<CancellationToken>,
    /// Cancellation: branch summary.
    pub(super) branch_summary_abort: Option<CancellationToken>,
    /// Cancellation: bash execution.
    pub(super) bash_abort: Option<CancellationToken>,
    /// Pending nextTurn custom messages injected into the next prompt.
    pub(super) pending_next_turn_messages: Vec<AgentMessage>,
    /// Bound extension mode (set by `bind_extensions`).
    pub(super) extension_mode: Option<crate::core::agent_session::extension::ExtensionMode>,
    /// Bound extension UI context tag (interactive mode only).
    pub(super) extension_ui_tag: Option<String>,
    /// Bound extension shutdown handler.
    pub(super) extension_shutdown_handler: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Bound extension error listener.
    pub(super) extension_error_listener: Option<ExtensionErrorListener>,
    /// Bound extension command-context actions (opaque JSON).
    pub(super) extension_command_context: Option<serde_json::Value>,
    /// Session-start event stored at construction, consumed by the first
    /// `bind_extensions` call (take-guard against duplicate emission).
    pub(super) pending_session_start: Option<SessionStartEvent>,
    /// Whether bind-time startup resource discovery already ran
    /// (same-session rediscovery guard).
    pub(super) initial_resources_discovered: bool,
    /// Base built-in tool definitions (insertion-ordered, first-wins on dupes).
    pub(super) base_tool_definitions: Vec<std::sync::Arc<dyn AgentTool>>,
    /// Active tool registry (built-in + extension + custom, insertion-ordered).
    pub(super) tool_registry: Vec<std::sync::Arc<dyn AgentTool>>,
    /// Optional tool allowlist.
    pub(super) allowed_tool_names: Option<std::collections::HashSet<String>>,
    /// Optional tool denylist.
    pub(super) excluded_tool_names: Option<std::collections::HashSet<String>>,
    /// Pending bash messages awaiting flush after `agent_end`.
    pub(super) pending_bash_messages: Vec<crate::core::messages::BashExecutionMessage>,
}

#[derive(Default)]
pub(super) struct AutomaticActionFlags {
    auto_retry_enabled: bool,
    auto_compaction_enabled: bool,
}

pub(super) struct SessionLifecycle {
    automatic_actions: AutomaticActionFlags,
    is_agent_run_active: bool,
    overflow_recovery_attempted: bool,
    disposed: bool,
}

impl Default for SessionLifecycle {
    fn default() -> Self {
        Self {
            automatic_actions: AutomaticActionFlags {
                auto_retry_enabled: true,
                auto_compaction_enabled: true,
            },
            is_agent_run_active: false,
            overflow_recovery_attempted: false,
            disposed: false,
        }
    }
}

impl std::ops::Deref for SessionLifecycle {
    type Target = AutomaticActionFlags;

    fn deref(&self) -> &Self::Target {
        &self.automatic_actions
    }
}

impl std::ops::DerefMut for SessionLifecycle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.automatic_actions
    }
}

impl std::ops::Deref for AgentSessionInner {
    type Target = SessionLifecycle;

    fn deref(&self) -> &Self::Target {
        &self.lifecycle
    }
}

impl std::ops::DerefMut for AgentSessionInner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lifecycle
    }
}

type ExtensionErrorListener = Arc<dyn Fn(&str) + Send + Sync>;

/// Awaited barrier invoked after a public event has reached synchronous listeners.
pub type EventBackpressureHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

impl AgentSessionInner {
    fn new(scoped_models: Vec<ScopedModel>, base_system_prompt: String) -> Self {
        Self {
            lifecycle: SessionLifecycle::default(),
            steering_messages: Vec::new(),
            follow_up_messages: Vec::new(),
            retry_attempt: 0,
            max_retries: 3,
            last_assistant_message: None,
            listeners: Vec::new(),
            next_listener_id: 1,
            backpressure_hooks: Vec::new(),
            next_backpressure_hook_id: 1,
            pending_session_error: None,
            pump: None,
            idle_notify: Arc::new(Notify::new()),
            processed_agent_ends: 0,
            agent_end_notify: Arc::new(Notify::new()),
            agent_end_wait_cancel: CancellationToken::new(),
            scoped_models,
            active_tool_names: Vec::new(),
            base_system_prompt,
            retry_abort: None,
            compaction_abort: None,
            auto_compaction_abort: None,
            branch_summary_abort: None,
            bash_abort: None,
            pending_next_turn_messages: Vec::new(),
            extension_mode: None,
            extension_ui_tag: None,
            extension_shutdown_handler: None,
            extension_error_listener: None,
            extension_command_context: None,
            pending_session_start: None,
            initial_resources_discovered: false,
            base_tool_definitions: Vec::new(),
            tool_registry: Vec::new(),
            allowed_tool_names: None,
            excluded_tool_names: None,
            pending_bash_messages: Vec::new(),
        }
    }
}

/// Mode-agnostic agent session.
///
/// Not `Clone`. Modes hold it behind their own reference (`Arc` at the runtime
/// layer if needed). Interior mutability covers pump/listener/queue state.
pub struct AgentSession {
    /// Underlying agent turn loop.
    pub agent: Agent,
    /// Session tree (single-writer async mutex).
    pub(super) session_manager: Arc<AsyncMutex<SessionManager>>,
    /// Serializes pending-bash flushes without owning queue data or nesting locks.
    pub(super) bash_flush_lock: AsyncMutex<()>,
    /// Settings manager (interior-mutable so every accessor / mutator on
    /// `AgentSession` can operate through `&self`). Lock briefly and drop
    /// before any `.await` — see [`AgentSession::lock_settings`].
    pub(super) settings_manager: std::sync::Mutex<SettingsManager>,
    /// Working directory.
    pub cwd: String,
    /// Shared hooks for agent closures + extension runner.
    pub(super) hooks: Arc<SessionHooks>,
    /// Mutable inner state.
    pub(super) inner: Mutex<AgentSessionInner>,
    /// Concrete host runner for reload (optional; no trait downcast).
    pub(super) host_extension_runner:
        std::sync::RwLock<Option<Arc<crate::core::extension_host::HostExtensionRunner>>>,
    /// Typed model runtime shared across product-owned session boundaries.
    pub(super) model_runtime: Option<Arc<ModelRuntime>>,
    /// Optional compaction-only stream override.
    pub(super) compaction_stream_override: Option<compaction::CompactionStreamHandle>,
    /// Skills for `/skill:name` expansion.
    pub(super) skills: Mutex<Vec<crate::core::resources::skills::Skill>>,
    /// Prompt templates for `/template` expansion.
    pub(super) prompt_templates: Mutex<Vec<crate::core::resources::prompts::PromptTemplate>>,
    /// Resource loader for extension-discovered skills, prompts, and themes.
    pub(super) resource_loader: Option<AsyncMutex<crate::core::resources::DefaultResourceLoader>>,
    /// ResourceLoader-resolved provenance for extension paths, keyed by both
    /// configured and resolved paths for host command lookup.
    pub(super) extension_source_infos:
        Mutex<std::collections::HashMap<String, crate::core::resources::SourceInfo>>,
    /// Self handle for pump (set after construction).
    pub(super) self_handle: Mutex<Option<std::sync::Weak<AgentSession>>>,
    /// Serializes the whole `bind_extensions` lifecycle (record → emit →
    /// discover). Lives on the session (not `AgentSessionInner`) because it
    /// is held across `.await`; acquire it before any `lock_inner()`, never
    /// hold `lock_inner` across an await.
    pub(super) bind_lock: AsyncMutex<()>,
}

/// Errors from [`AgentSession::new`].
#[derive(Debug, thiserror::Error)]
pub enum AgentSessionError {
    /// Missing both a pre-built agent and a provider.
    #[error("AgentSessionConfig requires `agent` or `provider`")]
    MissingAgentOrProvider,
    /// Session manager error during setup.
    #[error(transparent)]
    Session(#[from] crate::core::sessions::SessionError),
}

fn default_agent_loop_config(model: &Model) -> AgentLoopConfig {
    AgentLoopConfig {
        model: model.clone(),
        reasoning: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        transport: None,
        cache_retention: None,
        thinking_budgets: None,
        max_retry_delay_ms: None,
        metadata: None,
        headers: None,
        env: None,
        stream_extra: serde_json::Map::new(),
        tool_execution: pi_agent::ToolExecutionMode::Parallel,
        convert_to_llm: product_convert_to_llm_hook(),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        before_tool_call: None,
        after_tool_call: None,
        on_payload: None,
        on_response: None,
    }
}

impl AgentSession {
    /// Construct a session, install hooks, and spawn the event pump.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSessionError::MissingAgentOrProvider`] when neither an
    /// agent nor a provider is supplied.
    pub fn new(config: AgentSessionConfig) -> Result<Arc<Self>, AgentSessionError> {
        let runner = config
            .extension_runner
            .unwrap_or_else(|| Arc::new(NullExtensionRunner));
        let hooks = Arc::new(SessionHooks::new(runner));
        hooks.set_base_system_prompt(config.system_prompt.clone());
        hooks.set_tools(config.tools.clone());

        let agent = if let Some(agent) = config.agent {
            agent
        } else {
            let provider = config
                .provider
                .ok_or(AgentSessionError::MissingAgentOrProvider)?;
            let model = config
                .model
                .clone()
                .unwrap_or_else(pi_agent::state::default_model);
            let mut base = match config.base_config {
                Some(mut base) => {
                    base.convert_to_llm = product_convert_to_llm_hook();
                    base
                }
                None => default_agent_loop_config(&model),
            };
            // Upstream sdk.ts sets sessionId on the Agent config so provider
            // session-affinity, prompt-cache keys, and opencode session
            // headers fire. The config still owns the manager by value here;
            // session switches construct a fresh AgentSession, so the id
            // snapshot tracks the live session.
            base.session_id = Some(config.session_manager.get_session_id().to_owned());
            base.before_tool_call = Some(hooks.before_tool_call_hook());
            base.after_tool_call = Some(hooks.after_tool_call_hook());
            base.prepare_next_turn = Some(hooks.prepare_next_turn_hook());
            Agent::new(AgentOptions {
                system_prompt: config.system_prompt.clone(),
                model,
                thinking_level: config.thinking_level,
                tools: config.tools.clone(),
                messages: config.messages,
                config: base,
                provider,
            })
        };

        let retry = config.settings_manager.get_retry_settings();
        let compaction = config.settings_manager.get_compaction_settings();
        let extension_source_infos = extension_source_infos(config.resource_loader.as_ref());
        let mut inner = AgentSessionInner::new(config.scoped_models, config.system_prompt.clone());
        inner.pending_session_start = Some(config.session_start_event.unwrap_or_default());
        inner.auto_retry_enabled = retry.enabled;
        inner.max_retries = u32::try_from(retry.max_retries).unwrap_or(u32::MAX);
        inner.auto_compaction_enabled = compaction.enabled;
        if let Some(ref names) = config.initial_active_tool_names {
            inner.active_tool_names.clone_from(names);
        }

        let session = Arc::new(Self {
            agent,
            session_manager: Arc::new(AsyncMutex::new(config.session_manager)),
            bash_flush_lock: AsyncMutex::new(()),
            settings_manager: std::sync::Mutex::new(config.settings_manager),
            cwd: config.cwd,
            hooks,
            inner: Mutex::new(inner),
            host_extension_runner: std::sync::RwLock::new(config.host_extension_runner),
            model_runtime: config.model_runtime,
            compaction_stream_override: config.compaction_stream_override,
            skills: Mutex::new(config.skills),
            prompt_templates: Mutex::new(config.prompt_templates),
            resource_loader: config.resource_loader.map(AsyncMutex::new),
            extension_source_infos: Mutex::new(extension_source_infos),
            self_handle: Mutex::new(None),
            bind_lock: AsyncMutex::new(()),
        });

        // Build the initial tool registry from configured base tools and active
        // names. Extension tools will be picked up on the first reload.
        session.build_initial_tool_registry(
            config.tools.clone(),
            config.initial_active_tool_names.clone(),
            config.allowed_tool_names.clone(),
            config.excluded_tool_names.clone(),
        );

        // Store weak self and spawn pump.
        if let Ok(mut guard) = session.self_handle.lock() {
            *guard = Some(Arc::downgrade(&session));
        }
        let pump = session.spawn_event_pump();
        session.store_pump(pump);

        Ok(session)
    }

    // -------------------------------------------------------------------------
    // Accessors (public surface for modes / later slices)
    // -------------------------------------------------------------------------

    /// Underlying agent.
    #[must_use]
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Extension runner snapshot.
    #[must_use]
    pub fn extension_runner(&self) -> Arc<dyn ExtensionRunner> {
        self.hooks.runner()
    }

    /// [`SessionHooks`] handle (for reload / sibling modules).
    #[must_use]
    pub fn hooks(&self) -> Arc<SessionHooks> {
        Arc::clone(&self.hooks)
    }

    /// Session manager async mutex (single-writer).
    #[must_use]
    pub fn session_manager(&self) -> Arc<AsyncMutex<SessionManager>> {
        Arc::clone(&self.session_manager)
    }

    /// Current model from agent state.
    #[must_use]
    pub fn model(&self) -> Model {
        self.agent.state().model
    }

    /// Current thinking level.
    #[must_use]
    pub fn thinking_level(&self) -> ModelThinkingLevel {
        self.agent.state().thinking_level
    }

    /// Whether the agent is currently streaming.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.agent.state().is_streaming
    }

    /// Whether the agent has no active run.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        let inner = self.lock_inner();
        !inner.is_agent_run_active && !self.agent.state().is_streaming
    }

    /// Whether session-level auto-compaction is in progress.
    #[must_use]
    pub fn is_compacting(&self) -> bool {
        let inner = self.lock_inner();
        inner.compaction_abort.is_some() || inner.auto_compaction_abort.is_some()
    }

    /// Whether auto-retry sleep is in progress.
    #[must_use]
    pub fn is_retrying(&self) -> bool {
        self.lock_inner().retry_abort.is_some()
    }

    /// Whether bash is running.
    #[must_use]
    pub fn is_bash_running(&self) -> bool {
        self.lock_inner().bash_abort.is_some()
    }
    /// Whether branch summarization is in progress.
    #[must_use]
    pub fn is_summarizing(&self) -> bool {
        self.lock_inner().branch_summary_abort.is_some()
    }

    /// Session file path, if any.
    pub async fn session_file(&self) -> Option<String> {
        self.session_manager
            .lock()
            .await
            .get_session_file()
            .map(str::to_owned)
    }

    /// Session id.
    pub async fn session_id(&self) -> String {
        self.session_manager
            .lock()
            .await
            .get_session_id()
            .to_owned()
    }

    /// Session display name.
    pub async fn session_name(&self) -> Option<String> {
        self.session_manager.lock().await.get_session_name()
    }

    /// Scoped models list.
    #[must_use]
    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        self.lock_inner().scoped_models.clone()
    }

    /// Pending steering + follow-up count.
    #[must_use]
    pub fn pending_message_count(&self) -> usize {
        let inner = self.lock_inner();
        inner
            .steering_messages
            .len()
            .saturating_add(inner.follow_up_messages.len())
    }
    /// Pending steering and follow-up message mirrors.
    #[must_use]
    pub fn pending_messages(&self) -> (Vec<String>, Vec<String>) {
        let inner = self.lock_inner();
        (
            inner.steering_messages.clone(),
            inner.follow_up_messages.clone(),
        )
    }

    /// Active tool names.
    #[must_use]
    pub fn active_tool_names(&self) -> Vec<String> {
        self.lock_inner().active_tool_names.clone()
    }

    /// Transcript message count.
    #[must_use]
    pub fn message_count(&self) -> usize {
        self.agent.transcript().len()
    }

    /// Clone of current transcript.
    #[must_use]
    pub fn messages(&self) -> Vec<AgentMessage> {
        self.agent.transcript()
    }

    /// Steering queue mode.
    #[must_use]
    pub fn steering_mode(&self) -> QueueMode {
        self.agent.steering_mode()
    }

    /// Follow-up queue mode.
    #[must_use]
    pub fn follow_up_mode(&self) -> QueueMode {
        self.agent.follow_up_mode()
    }

    /// Auto-compaction enabled flag.
    #[must_use]
    pub fn auto_compaction_enabled(&self) -> bool {
        self.lock_inner().auto_compaction_enabled
    }

    /// Auto-retry enabled flag.
    #[must_use]
    pub fn auto_retry_enabled(&self) -> bool {
        self.lock_inner().auto_retry_enabled
    }

    /// Typed model-runtime handle.
    #[must_use]
    pub fn model_runtime_handle(&self) -> Option<Arc<ModelRuntime>> {
        self.model_runtime.clone()
    }

    // -------------------------------------------------------------------------
    // Subscribe / emit
    // -------------------------------------------------------------------------

    /// Subscribe to public session events. Returns an unsubscribe token.
    ///
    /// Listeners are invoked without holding the inner mutex.
    pub fn subscribe<F>(&self, listener: F) -> impl Fn() + Send + Sync + 'static
    where
        F: Fn(&AgentSessionEvent) + Send + Sync + 'static,
    {
        let listener: AgentSessionEventListener = Arc::new(listener);
        let listener_id = {
            let mut inner = self.lock_inner();
            let id = inner.next_listener_id;
            inner.next_listener_id = inner.next_listener_id.saturating_add(1);
            inner.listeners.push((id, Arc::clone(&listener)));
            id
        };

        let session_for_unsub = self.upgrade_self();
        move || {
            if let Some(session) = session_for_unsub
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
            {
                let mut guard = session.lock_inner();
                guard.listeners.retain(|(id, _)| *id != listener_id);
            }
        }
    }

    /// Register an awaited event-production barrier.
    ///
    /// The event pump and compaction paths invoke these hooks after synchronous
    /// public listeners. The returned closure removes the hook by stable id.
    pub fn register_event_backpressure_hook(
        &self,
        hook: EventBackpressureHook,
    ) -> Box<dyn Fn() + Send + Sync> {
        let hook_id = {
            let mut inner = self.lock_inner();
            let id = inner.next_backpressure_hook_id;
            inner.next_backpressure_hook_id = inner.next_backpressure_hook_id.saturating_add(1);
            inner.backpressure_hooks.push((id, hook));
            id
        };
        let session_for_unsub = self.upgrade_self();
        Box::new(move || {
            if let Some(session) = session_for_unsub
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
            {
                session
                    .lock_inner()
                    .backpressure_hooks
                    .retain(|(id, _)| *id != hook_id);
            }
        })
    }

    pub(super) async fn await_event_backpressure(&self) {
        let hooks = self
            .lock_inner()
            .backpressure_hooks
            .iter()
            .map(|(_, hook)| Arc::clone(hook))
            .collect::<Vec<_>>();
        for hook in hooks {
            hook().await;
        }
    }

    pub(super) async fn emit_public_awaited(&self, event: &Event) {
        self.emit_public(event);
        self.await_event_backpressure().await;
    }

    pub(super) fn record_session_error(&self, error: SessionError) {
        let mut inner = self.lock_inner();
        if inner.pending_session_error.is_none() {
            inner.pending_session_error = Some(error);
        }
    }

    pub(super) fn take_session_error(&self) -> Option<SessionError> {
        self.lock_inner().pending_session_error.take()
    }

    pub(super) fn emit_public<E>(&self, event: E)
    where
        E: std::borrow::Borrow<Event>,
    {
        let event = event.borrow();
        let listeners = {
            let inner = self.lock_inner();
            inner
                .listeners
                .iter()
                .map(|(_, listener)| Arc::clone(listener))
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener(event);
        }
    }

    /// Emit a `queue_update` snapshot from current mirrors.
    pub(super) fn emit_queue_update(&self) {
        let (steering, follow_up) = {
            let inner = self.lock_inner();
            (
                inner.steering_messages.clone(),
                inner.follow_up_messages.clone(),
            )
        };
        self.emit_public(&Event::QueueUpdate {
            steering,
            follow_up,
        });
    }

    /// Push a steering mirror entry and emit `queue_update`.
    pub(super) fn mirror_steering_push(&self, text: String) {
        {
            let mut inner = self.lock_inner();
            inner.steering_messages.push(text);
        }
        self.emit_queue_update();
    }

    /// Push a follow-up mirror entry and emit `queue_update`.
    pub(super) fn mirror_follow_up_push(&self, text: String) {
        {
            let mut inner = self.lock_inner();
            inner.follow_up_messages.push(text);
        }
        self.emit_queue_update();
    }

    /// Clear both mirror queues and emit `queue_update`.
    pub fn clear_queue(&self) {
        {
            let mut inner = self.lock_inner();
            inner.steering_messages.clear();
            inner.follow_up_messages.clear();
        }
        self.agent.clear_queues();
        self.emit_queue_update();
    }

    // -------------------------------------------------------------------------
    // Cancellation slots
    // -------------------------------------------------------------------------

    /// Replace the retry abort token; returns the new token.
    pub(super) fn begin_retry_abort(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.retry_abort.take() {
            prev.cancel();
        }
        inner.retry_abort = Some(token.clone());
        token
    }

    /// Clear the retry abort token.
    pub(super) fn clear_retry_abort(&self) {
        let mut inner = self.lock_inner();
        inner.retry_abort = None;
    }

    /// Abort in-flight retry sleep.
    pub fn abort_retry(&self) {
        let mut inner = self.lock_inner();
        if let Some(token) = inner.retry_abort.take() {
            token.cancel();
        }
    }

    /// Begin compaction abort slot.
    pub(super) fn begin_compaction_abort(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.compaction_abort.take() {
            prev.cancel();
        }
        inner.compaction_abort = Some(token.clone());
        token
    }

    /// Clear compaction abort.
    pub(super) fn clear_compaction_abort(&self) {
        self.lock_inner().compaction_abort = None;
    }

    /// Abort manual compaction.
    pub fn abort_compaction(&self) {
        let mut inner = self.lock_inner();
        if let Some(token) = inner.compaction_abort.take() {
            token.cancel();
        }
        if let Some(token) = inner.auto_compaction_abort.take() {
            token.cancel();
        }
    }

    /// Begin bash abort slot.
    pub(super) fn begin_bash_abort(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.bash_abort.take() {
            prev.cancel();
        }
        inner.bash_abort = Some(token.clone());
        token
    }

    /// Clear bash abort.
    pub(super) fn clear_bash_abort(&self) {
        self.lock_inner().bash_abort = None;
    }

    /// Abort bash.
    pub fn abort_bash(&self) {
        let mut inner = self.lock_inner();
        if let Some(token) = inner.bash_abort.take() {
            token.cancel();
        }
    }

    /// Abort every active session operation, then wait for session idle.
    pub async fn abort(&self) {
        self.abort_retry();
        self.abort_compaction();
        self.abort_branch_summary();
        self.abort_bash();
        self.agent.abort();
        self.wait_for_idle().await;
    }

    /// Wait until the session-level run is idle (no active retries/continuations).
    pub async fn wait_for_idle(&self) {
        loop {
            let notified = {
                let inner = self.lock_inner();
                if !inner.is_agent_run_active && !self.agent.state().is_streaming {
                    return;
                }
                Arc::clone(&inner.idle_notify)
            };
            // Also wait for agent idle so we don't spin.
            tokio::select! {
                () = notified.notified() => {}
                () = self.agent.wait_for_idle() => {
                    let inner = self.lock_inner();
                    if !inner.is_agent_run_active {
                        return;
                    }
                }
            }
        }
    }

    /// Dispose this session's local resources.
    ///
    /// Cancels session-owned operations, disconnects the event pump, aborts
    /// and drains the agent, invalidates the extension context, then awaits
    /// host process reap exactly once when a concrete host is present — even
    /// if no `session_shutdown` handlers were registered. The runtime
    /// replacement layer owns the single reason-specific extension shutdown
    /// event and must emit it before calling this method when handlers exist.
    pub async fn dispose(&self) {
        {
            let mut inner = self.lock_inner();
            if inner.disposed {
                return;
            }
            inner.disposed = true;
            if let Some(token) = inner.retry_abort.take() {
                token.cancel();
            }
            if let Some(token) = inner.compaction_abort.take() {
                token.cancel();
            }
            if let Some(token) = inner.auto_compaction_abort.take() {
                token.cancel();
            }
            if let Some(token) = inner.branch_summary_abort.take() {
                token.cancel();
            }
            if let Some(token) = inner.bash_abort.take() {
                token.cancel();
            }
        }
        self.disconnect_from_agent();
        self.agent.abort();
        self.agent.wait_for_idle().await;
        self.hooks.runner().invalidate();
        // Always await process reap exactly once when a host was bound.
        let host = {
            if let Ok(mut guard) = self.host_extension_runner.write() {
                guard.take()
            } else {
                None
            }
        };
        if let Some(host) = host {
            host.shutdown_once().await;
        }
    }

    // -------------------------------------------------------------------------
    // pub(super) helpers for sibling modules / pump
    // -------------------------------------------------------------------------

    pub(super) fn lock_inner(&self) -> std::sync::MutexGuard<'_, AgentSessionInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock the settings manager with poison recovery.
    ///
    /// Callers must drop the guard before any `.await`. Read what you need
    /// into locals first when the surrounding function is async.
    pub fn lock_settings(&self) -> std::sync::MutexGuard<'_, SettingsManager> {
        self.settings_manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn store_pump(&self, pump: EventPump) {
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.pump.take() {
            prev.cancel.cancel();
            prev.join.abort();
        }
        inner.pump = Some(pump);
    }

    fn take_pump(&self) -> Option<EventPump> {
        self.lock_inner().pump.take()
    }

    fn pump_is_active(&self) -> bool {
        let inner = self.lock_inner();
        inner
            .pump
            .as_ref()
            .is_some_and(|p| p.active.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn upgrade_self(&self) -> Option<std::sync::Weak<AgentSession>> {
        self.self_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set auto-retry enabled.
    ///
    /// Updates the runtime cache used by `prepare_retry` / `will_retry` and the
    /// settings document (TypeScript `setAutoRetryEnabled` writes settings).
    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.lock_inner().auto_retry_enabled = enabled;
        self.lock_settings().set_retry_enabled(enabled);
    }

    /// Set auto-compaction enabled.
    ///
    /// Updates both the runtime cache and the persisted settings document.
    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.lock_inner().auto_compaction_enabled = enabled;
        self.lock_settings().set_compaction_enabled(enabled);
    }

    /// Set steering mode on the agent.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.agent.set_steering_mode(mode);
    }

    /// Set follow-up mode on the agent.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.agent.set_follow_up_mode(mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use futures::stream::{self, BoxStream, StreamExt};
    use pi_agent::{AgentEvent, user_text};
    use pi_ai::{
        AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, Model,
        ModelCost, ModelInput, Provider, ProviderError, StopReason, StreamOptions, TextContent,
    };
    use tokio::sync::mpsc;
    use tokio::time::sleep;

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

    fn assistant(text: &str) -> AssistantMessage {
        let mut message =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        message
            .content
            .push(AssistantContent::Text(TextContent::new(text)));
        message.stop_reason = StopReason::Stop;
        message
    }

    fn start_event() -> AssistantMessageEvent {
        AssistantMessageEvent::Start {
            partial: AssistantMessage::new(
                "test-api",
                "test-provider",
                "m",
                pi_agent::now_millis(),
            ),
        }
    }

    fn done_event(text: &str) -> AssistantMessageEvent {
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: assistant(text),
        }
    }

    #[derive(Clone)]
    struct MockProvider(Vec<Result<AssistantMessageEvent, ProviderError>>);

    impl Provider for MockProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            stream::iter(self.0.clone()).boxed()
        }
    }

    #[derive(Clone)]
    struct ContextRecordingProvider {
        contexts: Arc<Mutex<Vec<Context>>>,
    }

    impl Provider for ContextRecordingProvider {
        fn stream(
            &self,
            _model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            self.contexts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(context);
            stream::iter([Ok(start_event()), Ok(done_event("ok"))]).boxed()
        }
    }

    #[derive(Clone)]
    struct PendingDeltaProvider;

    impl Provider for PendingDeltaProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            let delta = AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".to_owned(),
                partial: assistant("hello"),
            };
            stream::iter([Ok(start_event()), Ok(delta)])
                .chain(stream::pending())
                .boxed()
        }
    }

    /// Extension runner that records emit order and can delay `message_end`.
    struct RecordingRunner {
        order: Arc<Mutex<Vec<String>>>,
        delay_message_end: Duration,
        replace_with: Mutex<Option<AgentMessage>>,
        message_update_cancel_reason: Option<String>,
    }

    impl RecordingRunner {
        fn new(order: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                order,
                delay_message_end: Duration::ZERO,
                replace_with: Mutex::new(None),
                message_update_cancel_reason: None,
            }
        }

        fn cancelling_message_updates(reason: &str) -> Self {
            Self {
                message_update_cancel_reason: Some(reason.to_owned()),
                ..Self::new(Arc::new(Mutex::new(Vec::new())))
            }
        }
    }

    impl ExtensionRunner for RecordingRunner {
        fn has_handlers(&self, _event: &str) -> bool {
            true
        }

        fn emit(
            &self,
            event: AgentSessionEvent,
        ) -> futures::future::BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>>
        {
            let label = format!("ext:{}", event.type_name());
            Box::pin(async move {
                self.order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(label);
                Ok(None)
            })
        }

        fn emit_message_update_delta<'a>(
            &'a self,
            _event: &'a AssistantMessageEvent,
        ) -> futures::future::BoxFuture<'a, Result<Option<CancelResult>, ExtensionRunnerError>>
        {
            Box::pin(async move {
                Ok(self
                    .message_update_cancel_reason
                    .clone()
                    .map(|reason| CancelResult {
                        cancel: true,
                        reason: Some(reason),
                    }))
            })
        }

        fn emit_message_end(
            &self,
            message: AgentMessage,
        ) -> futures::future::BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>>
        {
            let delay = self.delay_message_end;
            Box::pin(async move {
                if !delay.is_zero() {
                    sleep(delay).await;
                }
                self.order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push("ext:message_end".into());
                let replacement = self
                    .replace_with
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                Ok(replacement.or(Some(message)))
            })
        }

        fn emit_tool_call(
            &self,
            _tool_name: &str,
            _tool_call_id: &str,
            _input: serde_json::Map<String, serde_json::Value>,
        ) -> futures::future::BoxFuture<
            '_,
            Result<Option<pi_agent::BeforeToolCallResult>, ExtensionRunnerError>,
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
        ) -> futures::future::BoxFuture<
            '_,
            Result<Option<pi_agent::AfterToolCallResult>, ExtensionRunnerError>,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_input(
            &self,
            _text: &str,
            _images: Option<serde_json::Value>,
            _source: &str,
            _streaming_behavior: Option<&str>,
        ) -> futures::future::BoxFuture<'_, Result<InputTransformResult, ExtensionRunnerError>>
        {
            Box::pin(async { Ok(InputTransformResult::default()) })
        }

        fn emit_before_agent_start(
            &self,
            _prompt: &str,
            _images: Option<serde_json::Value>,
            _system_prompt: Option<String>,
        ) -> futures::future::BoxFuture<
            '_,
            Result<Option<BeforeAgentStartResult>, ExtensionRunnerError>,
        > {
            Box::pin(async { Ok(None) })
        }

        fn emit_resources_discover(
            &self,
            _cwd: &str,
            _reason: &str,
        ) -> futures::future::BoxFuture<
            '_,
            Result<crate::core::resources::ResourceExtensionPaths, ExtensionRunnerError>,
        > {
            Box::pin(async { Ok(crate::core::resources::ResourceExtensionPaths::default()) })
        }

        fn execute_command<'a>(
            &'a self,
            _name: &'a str,
            _args: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<bool, ExtensionRunnerError>> {
            Box::pin(async { Ok(false) })
        }

        fn get_registered_commands(&self) -> Vec<String> {
            Vec::new()
        }

        fn get_all_registered_tools(
            &self,
        ) -> std::collections::HashMap<String, Arc<dyn AgentTool>> {
            std::collections::HashMap::new()
        }

        fn get_flag_values(&self) -> std::collections::HashMap<String, serde_json::Value> {
            std::collections::HashMap::new()
        }

        fn invalidate(&self) {}

        fn emit_error(&self, _message: String) {}
    }

    fn collect_types(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn message_update_cancel_aborts_stream_with_extension_reason()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut config =
            AgentSessionConfig::test_config(Arc::new(PendingDeltaProvider), test_model())?;
        config.extension_runner = Some(Arc::new(RecordingRunner::cancelling_message_updates(
            "policy stopped this stream",
        )));
        let session = AgentSession::new(config)?;

        tokio::time::timeout(
            Duration::from_secs(1),
            session
                .agent
                .prompt(vec![user_text("hi", std::iter::empty())]),
        )
        .await??;

        let assistant = session
            .agent
            .last_assistant()
            .ok_or("missing aborted assistant")?;
        assert_eq!(assistant.stop_reason, StopReason::Aborted);
        assert_eq!(
            assistant.error_message.as_deref(),
            Some("policy stopped this stream")
        );
        Ok(())
    }

    #[tokio::test]
    async fn single_prompt_event_order() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("hello")),
        ]));
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.system_prompt = "sys".into();
        let session = AgentSession::new(config)?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        session.mark_agent_run_active();
        let processed_agent_ends = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(processed_agent_ends)
                .await,
            "event pump disconnected before agent_end"
        );
        session.emit_agent_settled().await;

        let types = collect_types(&mut rx);
        // agent_start -> turn_start -> message_start -> message_end (user)
        // -> message_start -> message_end (assistant) -> turn_end -> agent_end -> agent_settled
        assert!(
            types.iter().any(|t| t == "agent_start"),
            "missing agent_start in {types:?}"
        );
        assert!(
            types.iter().any(|t| t == "agent_end"),
            "missing agent_end in {types:?}"
        );
        assert_eq!(
            types.iter().filter(|t| *t == "agent_settled").count(),
            1,
            "exactly one agent_settled: {types:?}"
        );
        let start = types
            .iter()
            .position(|t| t == "agent_start")
            .ok_or_else(|| std::io::Error::other("missing agent_start"))?;
        let end = types
            .iter()
            .position(|t| t == "agent_end")
            .ok_or_else(|| std::io::Error::other("missing agent_end"))?;
        let settled = types
            .iter()
            .position(|t| t == "agent_settled")
            .ok_or_else(|| std::io::Error::other("missing agent_settled"))?;
        assert!(start < end && end < settled, "order {types:?}");
        Ok(())
    }

    #[tokio::test]
    async fn stream_options_carry_the_session_id() -> Result<(), Box<dyn std::error::Error>> {
        // Upstream sdk.ts sets sessionId on the Agent so provider
        // session-affinity / prompt-cache headers fire; prove the id survives
        // AgentSession::new -> Agent -> StreamOptions at the provider boundary.
        struct CapturingProvider {
            seen: Arc<std::sync::Mutex<Vec<Option<String>>>>,
        }
        impl Provider for CapturingProvider {
            fn stream(
                &self,
                _model: &Model,
                _context: Context,
                options: StreamOptions,
            ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                self.seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(options.session_id);
                Box::pin(futures::stream::iter(vec![
                    Ok(start_event()),
                    Ok(done_event("ok")),
                ]))
            }
        }

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CapturingProvider {
            seen: Arc::clone(&seen),
        });
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;
        let expected = session.session_id().await;

        session.mark_agent_run_active();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;

        let captured = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first()
            .cloned()
            .ok_or("provider was never called")?;
        assert_eq!(
            captured.as_deref(),
            Some(expected.as_str()),
            "StreamOptions.session_id must match the live session"
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_before_public_ordering() -> Result<(), Box<dyn std::error::Error>> {
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecordingRunner::new(Arc::clone(&order)));
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.extension_runner = Some(runner);
        let session = AgentSession::new(config)?;

        let public_order = Arc::clone(&order);
        let _unsub = session.subscribe(move |event| {
            public_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("pub:{}", event.type_name()));
        });

        session.mark_agent_run_active();
        let processed_agent_ends = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(processed_agent_ends)
                .await,
            "event pump disconnected before agent_end"
        );

        let recorded = order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // For each event type that has both, ext must appear before pub.
        // Check agent_start specifically.
        let ext_start = recorded
            .iter()
            .position(|s| s == "ext:agent_start")
            .ok_or("missing extension agent_start")?;
        let pub_start = recorded
            .iter()
            .position(|s| s == "pub:agent_start")
            .ok_or("missing public agent_start")?;
        assert!(
            ext_start < pub_start,
            "extension before public: {recorded:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn queue_update_before_message_start_public() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("ok")),
            Ok(start_event()),
            Ok(done_event("ok2")),
        ]));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;

        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        let pending_at_user_start = Arc::new(AtomicUsize::new(usize::MAX));
        let pending_flag = Arc::clone(&pending_at_user_start);
        let session_for_count = Arc::clone(&session);
        let _unsub = session.subscribe(move |event| match event {
            AgentSessionEvent::MessageStart { message } if message.role() == "user" => {
                pending_flag.store(session_for_count.pending_message_count(), Ordering::SeqCst);
                let _ = tx.send("message_start:user".into());
            }
            AgentSessionEvent::QueueUpdate { .. } => {
                let _ = tx.send("queue_update".into());
            }
            _ => {}
        });

        // First prompt to establish assistant tail, then steer, then continue.
        session.mark_agent_run_active();
        let first_processed_agent_end = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("first", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(first_processed_agent_end)
                .await,
            "event pump disconnected before first agent_end"
        );

        session.mirror_steering_push("steer-text".into());
        session
            .agent
            .steer(user_text("steer-text", std::iter::empty()));
        assert_eq!(session.pending_message_count(), 1);

        let second_processed_agent_end = session.processed_agent_end_count();
        session.agent.continue_run().await?;
        assert!(
            session
                .wait_for_processed_agent_end(second_processed_agent_end)
                .await,
            "event pump disconnected before second agent_end"
        );

        // pendingMessageCount is already decremented by the time message_start is observed.
        let pending = pending_at_user_start.load(Ordering::SeqCst);
        assert_eq!(
            pending, 0,
            "pending must be 0 at message_start:user, got {pending}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn message_end_replacement_updates_live_and_persists()
    -> Result<(), Box<dyn std::error::Error>> {
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            delay_message_end: Duration::ZERO,
            replace_with: Mutex::new(None),
            message_update_cancel_reason: None,
        });

        // Replace assistant text with "replaced".
        let mut replaced = assistant("replaced");
        replaced.stop_reason = StopReason::Stop;
        *runner
            .replace_with
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(AgentMessage::Llm(
            Box::new(pi_ai::Message::Assistant(replaced)),
        ));

        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("original")),
        ]));
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.extension_runner = Some(runner);
        let session = AgentSession::new(config)?;

        session.mark_agent_run_active();
        let processed_agent_ends = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(processed_agent_ends)
                .await,
            "event pump disconnected before agent_end"
        );

        // Live agent transcript tail should be the replacement.
        let last = session.agent.last_assistant();
        assert!(last.is_some(), "expected last assistant");
        let text = last
            .as_ref()
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
            })
            .unwrap_or("");
        assert_eq!(text, "replaced");

        // Persistence: session entries should include the replacement text.
        let sm = session.session_manager.lock().await;
        let entries = sm.get_entries();
        let encoded = serde_json::to_string(&entries).unwrap_or_default();
        assert!(
            encoded.contains("replaced"),
            "persisted entries should contain replacement: {encoded}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn single_settled_across_simulated_continuation() -> Result<(), Box<dyn std::error::Error>>
    {
        let provider = Arc::new(MockProvider(vec![
            Ok(start_event()),
            Ok(done_event("a")),
            Ok(start_event()),
            Ok(done_event("b")),
        ]));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;

        let settled = Arc::new(AtomicUsize::new(0));
        let settled_c = Arc::clone(&settled);
        let _unsub = session.subscribe(move |event| {
            if matches!(event, AgentSessionEvent::AgentSettled) {
                settled_c.fetch_add(1, Ordering::SeqCst);
            }
        });

        session.mark_agent_run_active();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;

        // Simulate retry/continuation without settling yet.
        session
            .agent
            .prompt(vec![user_text("again", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;

        // Exactly one settle after the full session-level lifecycle.
        session.emit_agent_settled().await;
        // Second call must not double-emit.
        session.emit_agent_settled().await;

        assert_eq!(settled.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn slow_extension_does_not_reorder() -> Result<(), Box<dyn std::error::Error>> {
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecordingRunner {
            order: Arc::clone(&order),
            delay_message_end: Duration::from_millis(30),
            replace_with: Mutex::new(None),
            message_update_cancel_reason: None,
        });
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.extension_runner = Some(runner);
        let session = AgentSession::new(config)?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _unsub = session.subscribe(move |event| {
            let _ = tx.send(event.type_name().to_owned());
        });

        session.mark_agent_run_active();
        let processed_agent_ends = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(processed_agent_ends)
                .await,
            "event pump disconnected before agent_end"
        );

        let types = collect_types(&mut rx);
        // message_end (user) must come before message_start (assistant), etc.
        let user_end = types
            .iter()
            .enumerate()
            .filter(|(_, t)| *t == "message_end")
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert!(
            user_end.len() >= 2,
            "expected user+assistant message_end: {types:?}"
        );
        assert!(user_end[0] < user_end[1], "order preserved: {types:?}");
        Ok(())
    }

    #[tokio::test]
    async fn listener_unsubscribe() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;

        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        let unsub = session.subscribe(move |_e| {
            c1.fetch_add(1, Ordering::SeqCst);
        });
        unsub();

        session.mark_agent_run_active();
        let processed_agent_ends = session.processed_agent_end_count();
        session
            .agent
            .prompt(vec![user_text("hi", std::iter::empty())])
            .await?;
        assert!(
            session
                .wait_for_processed_agent_end(processed_agent_ends)
                .await,
            "event pump disconnected before agent_end"
        );

        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "unsubscribed listener silent"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispose_cancels_pump_without_emitting_shutdown()
    -> Result<(), Box<dyn std::error::Error>> {
        let order = Arc::new(Mutex::new(Vec::new()));
        let runner = Arc::new(RecordingRunner::new(Arc::clone(&order)));
        let provider = Arc::new(MockProvider(vec![Ok(start_event()), Ok(done_event("ok"))]));
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.extension_runner = Some(runner);
        let session = AgentSession::new(config)?;
        assert!(session.pump_is_active());
        session.dispose().await;
        assert!(!session.pump_is_active());
        assert!(
            order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .all(|entry| !entry.starts_with("shutdown:")),
            "runtime teardown owns the single reason-specific shutdown event"
        );
        Ok(())
    }

    #[tokio::test]
    async fn recorded_bash_result_enters_transcript_and_session_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(Vec::new()));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;
        session
            .record_bash_result(
                "printf ok",
                super::bash::BashResult {
                    output: "ok".to_owned(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                },
                &super::bash::ExecuteBashOptions::default(),
            )
            .await?;

        assert_eq!(session.message_count(), 1);
        assert_eq!(session.messages()[0].role(), "bashExecution");
        assert_eq!(session.session_manager.lock().await.get_entries().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn excluded_bash_result_remains_in_transcript_and_session_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(Vec::new()));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;
        session
            .record_bash_result(
                "printf hidden",
                super::bash::BashResult {
                    output: "hidden".to_owned(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                },
                &super::bash::ExecuteBashOptions {
                    exclude_from_context: true,
                    ..Default::default()
                },
            )
            .await?;

        let messages = session.messages();
        assert_eq!(messages.len(), 1);
        let AgentMessage::Custom(message) = &messages[0] else {
            return Err("expected persisted bash custom message".into());
        };
        assert_eq!(message.role, "bashExecution");
        assert_eq!(
            message.payload.get("excludeFromContext"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(session.session_manager.lock().await.get_entries().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn product_converter_keeps_visible_messages_and_excludes_hidden_custom_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(ContextRecordingProvider {
            contexts: Arc::clone(&contexts),
        });
        let bash_message = |command: &str, output: &str, exclude_from_context: bool| {
            let mut payload = serde_json::Map::from_iter([
                (
                    "command".to_owned(),
                    serde_json::Value::String(command.to_owned()),
                ),
                (
                    "output".to_owned(),
                    serde_json::Value::String(output.to_owned()),
                ),
                ("exitCode".to_owned(), serde_json::Value::from(0)),
                ("cancelled".to_owned(), serde_json::Value::Bool(false)),
                ("truncated".to_owned(), serde_json::Value::Bool(false)),
                ("timestamp".to_owned(), serde_json::Value::from(1)),
            ]);
            if exclude_from_context {
                payload.insert(
                    "excludeFromContext".to_owned(),
                    serde_json::Value::Bool(true),
                );
            }
            AgentMessage::Custom(pi_agent::CustomAgentMessage::new("bashExecution", payload))
        };
        let mut config = AgentSessionConfig::test_config(provider, test_model())?;
        config.messages = vec![
            bash_message("echo visible", "visible", false),
            bash_message("echo hidden", "hidden", true),
            AgentMessage::Custom(pi_agent::CustomAgentMessage::new(
                "custom",
                serde_json::Map::from_iter([(
                    "excludeFromContext".to_owned(),
                    serde_json::Value::Bool(true),
                )]),
            )),
        ];
        let session = AgentSession::new(config)?;

        session
            .agent
            .prompt(vec![user_text("continue", std::iter::empty())])
            .await?;
        session.agent.wait_for_idle().await;

        let contexts = contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let context = contexts
            .first()
            .ok_or("provider did not receive a model context")?;
        assert_eq!(context.messages.len(), 2);
        let text_at = |index: usize| -> Result<&str, Box<dyn std::error::Error>> {
            let Some(pi_ai::Message::User(message)) = context.messages.get(index) else {
                return Err(format!("context message {index} was not a user message").into());
            };
            match &message.content {
                pi_ai::UserMessageContent::Text(text) => Ok(text),
                pi_ai::UserMessageContent::Blocks(blocks) => match blocks.as_slice() {
                    [pi_ai::UserContent::Text(text)] => Ok(&text.text),
                    _ => Err(format!("context message {index} had unexpected blocks").into()),
                },
            }
        };
        assert_eq!(text_at(0)?, "Ran `echo visible`\n```\nvisible\n```");
        assert_eq!(text_at(1)?, "continue");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abort_stops_running_bash() -> Result<(), Box<dyn std::error::Error>> {
        let provider = Arc::new(MockProvider(Vec::new()));
        let session = AgentSession::new(AgentSessionConfig::test_config(provider, test_model())?)?;
        let running = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .execute_bash(
                        "sleep 30",
                        None::<fn(&str)>,
                        super::bash::ExecuteBashOptions::default(),
                    )
                    .await
            }
        });
        for _ in 0..100 {
            if session.is_bash_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(session.is_bash_running());

        session.abort().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), running).await??;
        assert!(!session.is_bash_running());
        Ok(())
    }

    #[tokio::test]
    async fn tool_turn_order_includes_tool_events() -> Result<(), Box<dyn std::error::Error>> {
        // Without a real tool-using provider fixture, verify that manually
        // injected agent events through the pump preserve tool_* types.
        // Full tool interleave is covered once tools slices land; here we
        // assert the session event mapping for tool variants.
        let event = AgentSessionEvent::from_agent_event(
            AgentEvent::ToolExecutionStart {
                tool_call_id: "1".into(),
                tool_name: "read".into(),
                args: serde_json::Map::new(),
            },
            false,
        );
        assert_eq!(event.type_name(), "tool_execution_start");
        let encoded = serde_json::to_value(&event)?;
        assert_eq!(encoded["toolCallId"], "1");
        assert_eq!(encoded["toolName"], "read");
        Ok(())
    }
}
