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
//! - emitting the stored `session_start` event exactly once per session
//!   instance on the first `bind_extensions` call (under `bind_lock`)
//! - extension-driven resource discovery (skills/prompts/themes)
//! - reload (emits `session_shutdown{reload}`, preserves flag values, swaps the
//!   runtime facade's generation, re-emits `session_start{reload}`, then
//!   re-discovers resources)
//! - the replaced-session context handed to `withSession` after runtime swap
//! - extension error isolation (host errors never abort the session)
//!
//! Divergence from upstream: reload emission is not gated on recorded
//! bindings (`hasBindings` in `agent-session.ts`). All Rust modes bind, and
//! `emit` self-gates on handler presence, so the gate would only suppress
//! correct emissions.

use crate::core::agent_session_runtime::{
    AgentSessionRuntime, AgentSessionRuntimeError, ForkPosition, NewSessionOptions,
    PrepareReplacementOutcome, PreparedReplacement, SwitchSessionOptions,
};
use crate::core::extension_host::SessionBridgeEvent;
use crate::core::extension_runtime_set::{
    ExtensionRuntimeSet, ExtensionSetDiagnostic, PendingReadyOp, SessionBridgeRoute,
    SessionTargetBinding,
};
use crate::core::messages::CustomMessageContent;
use crate::core::resources::{
    ResourceLoader, SlashCommandInfo, SlashCommandSource, SourceInfo, SourceOrigin, SourceScope,
    SyntheticSourceInfoOptions, create_synthetic_source_info,
};
use pi_ai::{ImageContent, Model, ModelThinkingLevel};
use pi_ext::protocol::{
    self, SessionCommand, SessionCommandInfoWire, SessionForkPosition, SessionNavigateTreeResponse,
    SessionScopedModelWire, SessionSetupEntriesRequest, SessionStateWire, SessionToolWire,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use super::prompt::{CustomMessageInput, DeliverAs};
use super::tools::RefreshToolRegistryOptions;
use super::tree::NavigateTreeOptions;

use super::AgentSession;
use super::events::{
    AgentSessionEvent, SessionShutdownReason, SessionStartEvent, SessionStartReason,
};

const RELOAD_BUSY: &str = "session replacement in progress";
const RELOAD_INVALIDATED: &str = "extension runtime was invalidated during reload";

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
    /// `resources_discover` failed.
    #[error("extension resource discovery failed: {0}")]
    ResourceDiscover(super::extension_runner::ExtensionRunnerError),
    /// Resource loader reload failed.
    #[error("resource reload failed: {0}")]
    ResourceReload(String),
    /// Host restart after reload failed.
    #[error("extension host restart failed: {0}")]
    HostRestart(String),
    /// Settings-refresh blocking task failed to join (panic/cancellation).
    #[error("settings reload failed: {0}")]
    SettingsReload(String),
}

impl AgentSession {
    /// Returns true when at least one extension handler is registered for
    /// `event_type`. Cheap delegation to the runner; safe to call from any
    /// thread without locking session state.
    #[must_use]
    pub fn has_extension_handlers(&self, event_type: &str) -> bool {
        self.hooks.runner().has_handlers(event_type)
    }

    /// Bind extension UI/mode/error/shutdown listeners, emit the stored
    /// `session_start` event (first bind only), and drive resource discovery.
    ///
    /// The whole lifecycle runs under the session `bind_lock`, so concurrent
    /// binds are serialized: the losing bind waits for the winner's full
    /// session_start-then-discovery sequence. The stored event is consumed
    /// with `Option::take`, so repeated binds on the same session instance
    /// never re-emit.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError::ResourceDiscover`] when the runner fails
    /// to discover resources.
    pub async fn bind_extensions(
        &self,
        bindings: ExtensionBindings,
    ) -> Result<(), ExtensionBindError> {
        let _bind_guard = self.bind_lock.lock().await;
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
        }

        // Claim the host session-action bridge (first bind per host instance
        // wins; later binds are no-ops because the receiver is taken).
        self.bind_session_bridge().await;

        let pending = self.lock_inner().pending_session_start.take();
        if let Some(event) = &pending {
            // emit self-gates on has_handlers("session_start"); host errors
            // are isolated (reported via the host error listener).
            let _ = self
                .hooks
                .runner()
                .emit(AgentSessionEvent::SessionStart {
                    reason: event.reason,
                    previous_session_file: event.previous_session_file.clone(),
                })
                .await;
        }
        // Non-reload start reasons map to "startup" for resources_discover
        // (its wire contract only allows startup|reload).
        let discover_reason = match pending {
            Some(SessionStartEvent {
                reason: SessionStartReason::Reload,
                ..
            }) => "reload",
            _ => "startup",
        };
        self.extend_resources_from_extensions(discover_reason)
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
    /// 0. Re-read settings from storage on a blocking worker before anything
    ///    reads them (authoritative settings refresh for direct and
    ///    interactive callers).
    /// 1. Capture previous flag values (preserved across the swap).
    /// 2. Prepare a concrete runtime replacement before notifying the old
    ///    generation.
    /// 3. Emit `session_shutdown{reload}` only after preparation succeeds,
    ///    then commit the replacement and re-register providers without
    ///    replacing the stable facade.
    /// 4. Emit `session_start{reload}` on the replacement generation.
    /// 5. Reload base resources and re-discover extension resources.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError`] when the settings refresh, host
    /// preparation, or resource discovery fails. Non-fatal extension
    /// diagnostics are returned after a successful reload.
    pub async fn reload(
        self: &Arc<Self>,
    ) -> Result<Vec<ExtensionSetDiagnostic>, ExtensionBindError> {
        // Authoritative settings refresh: re-read storage on a blocking
        // worker before any branch reads settings-derived state.
        self.reload_settings().await?;
        let host = self.host_extension_runner();
        let runner = self.hooks.runner();
        let previous_flag_values = runner.get_flag_values();

        if let (Some(host), Some(runtime)) = (host.as_ref(), self.model_runtime()) {
            let reload_guard = host.reload_lock().lock().await;
            if host.is_pending_busy() {
                return Err(ExtensionBindError::HostRestart(RELOAD_BUSY.to_owned()));
            }
            let prepared = host
                .prepare_reload(previous_flag_values)
                .await
                .map_err(|error| ExtensionBindError::HostRestart(error.to_string()))?;
            let token = host.next_replacement_token();
            let ready_rx = host
                .install_pending(
                    token.clone(),
                    PendingReadyOp::Reload {
                        prepared,
                        model_runtime: runtime,
                    },
                )
                .map_err(|_| ExtensionBindError::HostRestart(RELOAD_BUSY.to_owned()))?;
            drop(reload_guard);

            // Never hold reload_lock across a host callback: a reload hook may
            // synchronously attempt another session operation.
            let _ = runner
                .emit(AgentSessionEvent::SessionShutdown {
                    reason: SessionShutdownReason::Reload,
                    target_session_file: None,
                })
                .await;

            if !host.complete_ready(&token) {
                drop(ready_rx);
                return Err(ExtensionBindError::HostRestart(
                    RELOAD_INVALIDATED.to_owned(),
                ));
            }
            drop(ready_rx);
            let Some((
                PendingReadyOp::Reload {
                    prepared,
                    model_runtime,
                },
                _finalize_guard,
            )) = host.take_finalizing(&token)
            else {
                return Err(ExtensionBindError::HostRestart(
                    RELOAD_INVALIDATED.to_owned(),
                ));
            };
            let reload_guard = host.reload_lock().lock().await;
            let reload = host.commit_reload(&model_runtime, prepared).await;
            let _ = host.finish_finalize(&token);
            if !reload.committed {
                return Err(ExtensionBindError::HostRestart(
                    RELOAD_INVALIDATED.to_owned(),
                ));
            }
            let diagnostics = reload.diagnostics;
            drop(reload_guard);
            self.refresh_tool_registry(&super::tools::RefreshToolRegistryOptions {
                active_tool_names: None,
                include_all_extension_tools: true,
            });
            self.refresh_selected_model_from_runtime();
            self.hydrate_replacement_host().await;
            self.emit_session_start_reload().await;
            self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                .await?;
            return Ok(diagnostics);
        }

        if host.as_ref().is_some_and(|host| !host.can_reload()) {
            return Err(ExtensionBindError::HostRestart(
                "extension runtime is not reloadable".to_owned(),
            ));
        }

        // Lifecycle event on the old host. Emit self-gates on handler
        // presence; host transport reaping is handled below regardless.
        let _ = runner
            .emit(AgentSessionEvent::SessionShutdown {
                reason: SessionShutdownReason::Reload,
                target_session_file: None,
            })
            .await;

        if let Some(host) = host {
            // No runtime to re-register providers against: still reap the
            // old host so dispose paths stay single-reap clean.
            host.shutdown_once().await;
            self.set_host_extension_runner(None);
            self.hooks
                .set_runner(Arc::new(super::extension_runner::NullExtensionRunner));
            self.emit_session_start_reload().await;
            self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                .await?;
            return Ok(Vec::new());
        }

        // Trait-only / test path (no concrete host).
        self.emit_session_start_reload().await;
        self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
            .await?;
        Ok(Vec::new())
    }

    /// Emit `session_start{reload}` on the current (post-swap) runner.
    async fn emit_session_start_reload(&self) {
        let _ = self
            .hooks
            .runner()
            .emit(AgentSessionEvent::SessionStart {
                reason: SessionStartReason::Reload,
                previous_session_file: None,
            })
            .await;
    }

    /// Re-resolve the currently selected model against the current
    /// [`ModelRuntime`] and apply session model-selection semantics.
    ///
    /// After a live provider or reload mutation the provider registry may have
    /// changed: a model's configuration may have been updated, or the model may
    /// have been removed entirely. This looks up the current model in the
    /// refreshed runtime and, when the definition changed, assigns the refreshed
    /// [`Model`] directly to the live agent via the low-level `Agent::set_model`
    /// setter. Unlike [`AgentSession::set_model`], this does not append a
    /// `model_change` session entry, mutate saved defaults, re-clamp the thinking
    /// level, or emit `model_select` — it is a provider-configuration refresh,
    /// not an explicit user selection. When the selected identity was removed
    /// from the registry, the existing model is kept as-is; the pinned
    /// TypeScript `reload` contract performs no automatic fallback.
    pub(super) fn refresh_selected_model_from_runtime(&self) {
        let Some(runtime) = self.model_runtime() else {
            return;
        };
        let current = self.model();
        if let Some(refreshed) = runtime.get_model(&current.provider, &current.id)
            && refreshed != current
        {
            self.agent.set_model(refreshed);
        }
    }

    /// Push the current authoritative session snapshot to the replacement
    /// host so synchronous `session_start{reload}` hooks observe real state,
    /// not initial/fallback defaults. No-op when no concrete host is attached.
    async fn hydrate_replacement_host(&self) {
        let Some(host) = self.host_extension_runner() else {
            return;
        };
        let Some(session) = self.upgrade_self() else {
            return;
        };
        let Some(session) = session.upgrade() else {
            return;
        };
        let Some(binding) = host.session_binding_for(&session) else {
            return;
        };
        let state = self.session_state_wire().await;
        let _ = host.push_session_state_for_binding(binding, &state).await;
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
                let discovered_source = command
                    .source
                    .as_deref()
                    .and_then(|source| host.command_source_info(source));
                let host_source = command.source_info.clone().map(|info| SourceInfo {
                    path: info.path,
                    source: info.source,
                    scope: match info.scope {
                        pi_ext::adapters::CommandSourceScope::User => SourceScope::User,
                        pi_ext::adapters::CommandSourceScope::Project => SourceScope::Project,
                        pi_ext::adapters::CommandSourceScope::Temporary => SourceScope::Temporary,
                    },
                    origin: match info.origin {
                        pi_ext::adapters::CommandSourceOrigin::Package => SourceOrigin::Package,
                        pi_ext::adapters::CommandSourceOrigin::TopLevel => SourceOrigin::TopLevel,
                    },
                    base_dir: info.base_dir,
                });
                let source_info = discovered_source.or(host_source).unwrap_or_else(|| {
                    create_synthetic_source_info(
                        command
                            .source
                            .clone()
                            .unwrap_or_else(|| "<extension>".to_owned()),
                        SyntheticSourceInfoOptions {
                            source: "extension".to_owned(),
                            scope: None,
                            origin: None,
                            base_dir: None,
                        },
                    )
                });
                commands.push(SlashCommandInfo {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    source: SlashCommandSource::Extension,
                    source_info,
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

    /// Concrete host runner handle (no trait downcast).
    #[must_use]
    pub fn host_extension_runner(&self) -> Option<Arc<ExtensionRuntimeSet>> {
        self.host_extension_runner
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Replace the concrete host runner handle (reload path).
    pub fn set_host_extension_runner(&self, runner: Option<Arc<ExtensionRuntimeSet>>) {
        if let Ok(mut guard) = self.host_extension_runner.write() {
            *guard = runner;
        }
    }
}

impl AgentSession {
    // -- Host session-action bridge ----------------------------------------

    /// Claim the host's session-action bridge, push the initial
    /// `session.update` snapshot (awaited, so extension handlers running in
    /// the very next lifecycle event — `session_start` included — observe
    /// real state, not defaults), and spawn the applier task.
    ///
    /// The task consumes `session.command` / `session.setModel` items from
    /// the host, applies them against this session, and keeps the host's
    /// mirror fresh (after every applied command and on relevant public
    /// session events). No-op when no concrete host is attached or the
    /// bridge is already claimed.
    async fn bind_session_bridge(&self) {
        let Some(host) = self.host_extension_runner() else {
            return;
        };
        let Some(session) = self.upgrade_self() else {
            return;
        };
        // Claim the sole session bridge before any bind or publish. A session
        // that loses the claim (the receiver was already taken) must never
        // bind the global target or publish global state.
        let Some(mut bridge_rx) = host.take_session_bridge() else {
            return;
        };
        let diagnostic_session = session.clone();
        let binding = host.bind_session_target(session.clone()).await;
        self.bind_session_mirror(Arc::clone(&host), session, binding)
            .await;

        tokio::spawn(async move {
            while let Some(item) = bridge_rx.recv().await {
                dispatch_session_bridge(&host, item, &diagnostic_session).await;
            }
        });
    }

    async fn bind_session_mirror(
        &self,
        host: Arc<ExtensionRuntimeSet>,
        session: std::sync::Weak<AgentSession>,
        binding: SessionTargetBinding,
    ) {
        let (dirty_tx, mut dirty_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let unsubscribe = self.subscribe(move |event| {
            if matches!(
                event,
                AgentSessionEvent::AgentStart
                    | AgentSessionEvent::AgentEnd { .. }
                    | AgentSessionEvent::AgentSettled
                    | AgentSessionEvent::ModelSelect { .. }
                    | AgentSessionEvent::ThinkingLevelChanged { .. }
                    | AgentSessionEvent::SessionInfoChanged { .. }
                    | AgentSessionEvent::QueueUpdate { .. }
                    | AgentSessionEvent::CompactionEnd { .. }
            ) {
                let _ = dirty_tx.send(());
            }
        });
        // Awaited: lifecycle handlers must observe the new session state.
        if host.is_session_target_current(binding) {
            let state = self.session_state_wire().await;
            let _ = host.activate_session_state(binding, &state).await;
        }
        tokio::spawn(async move {
            while dirty_rx.recv().await.is_some() {
                while dirty_rx.try_recv().is_ok() {}
                if !host.is_active() || !host.is_session_target_current(binding) {
                    break;
                }
                let Some(session) = session.upgrade() else {
                    break;
                };
                let state = session.session_state_wire().await;
                let _ = host.push_session_state_for_binding(binding, &state).await;
            }
            unsubscribe();
        });
    }

    /// Build the `session.update` mirror behind the host's synchronous
    /// session getters.
    pub(crate) async fn session_state_wire(&self) -> SessionStateWire {
        let model = self.model();
        let context_usage = self.get_context_usage().await.map(|usage| {
            json!({
                "tokens": usage.tokens,
                "contextWindow": usage.context_window,
                "percent": usage.percent,
            })
        });
        let prompt = self.hooks.system_prompt_snapshot();
        SessionStateWire {
            session_name: self.session_name().await,
            thinking_level: thinking_level_wire(self.thinking_level()),
            active_tools: self.active_tool_names(),
            all_tools: self
                .get_all_tools()
                .into_iter()
                .map(|tool| SessionToolWire {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                    source: None,
                })
                .collect(),
            commands: self
                .slash_commands()
                .into_iter()
                .map(|command| SessionCommandInfoWire {
                    name: command.name,
                    description: command.description,
                    source: match command.source {
                        SlashCommandSource::Extension => "extension".to_owned(),
                        SlashCommandSource::Prompt => "prompt".to_owned(),
                        SlashCommandSource::Skill => "skill".to_owned(),
                    },
                })
                .collect(),
            model: serde_json::to_value(&model).ok(),
            scoped_models: self
                .scoped_models()
                .into_iter()
                .map(|scoped| SessionScopedModelWire {
                    model: serde_json::to_value(&scoped.model).unwrap_or(Value::Null),
                    thinking_level: scoped.thinking_level.map(thinking_level_wire),
                })
                .collect(),
            is_idle: self.is_idle(),
            has_pending_messages: self.pending_message_count() > 0,
            context_usage,
            system_prompt: prompt.override_prompt.unwrap_or(prompt.base),
        }
    }

    /// Apply one host bridge item. Failures are isolated through
    /// [`AgentSession::report_extension_error`]; nothing aborts the session.
    async fn apply_session_bridge_event(
        self: &Arc<Self>,
        host: &Arc<ExtensionRuntimeSet>,
        event: SessionBridgeEvent,
    ) {
        match event {
            SessionBridgeEvent::Command { envelope, .. } => {
                self.apply_session_command(envelope.command).await;
            }
            SessionBridgeEvent::SetModel { id, request } => {
                let success = match serde_json::from_value::<Model>(request.model) {
                    Ok(model) => match self.set_model(model).await {
                        Ok(()) => true,
                        Err(error) => {
                            self.report_extension_error(format!("setModel: {error}"));
                            false
                        }
                    },
                    Err(error) => {
                        self.report_extension_error(format!("setModel: invalid model: {error}"));
                        false
                    }
                };
                if let Err(error) = host.respond_set_model(id, success).await {
                    self.report_extension_error(format!("setModel response: {error}"));
                }
            }
            SessionBridgeEvent::Compact { id, request } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    let outcome = match session
                        .compact(request.custom_instructions.as_deref())
                        .await
                    {
                        Ok(result) => serde_json::to_value(&result)
                            .map_err(|error| format!("encode compaction result: {error}")),
                        Err(error) => Err(error.to_string()),
                    };
                    if let Err(error) = host.respond_compact(id, outcome).await {
                        session.report_extension_error(format!("compact response: {error}"));
                    }
                });
            }
            SessionBridgeEvent::NewSession { id, request } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session.handle_bridge_new_session(host, id, request).await;
                });
            }
            SessionBridgeEvent::Fork { id, request } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session.handle_bridge_fork(host, id, request).await;
                });
            }
            SessionBridgeEvent::NavigateTree { id, request } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session.handle_bridge_navigate_tree(host, id, request).await;
                });
            }
            SessionBridgeEvent::SwitchSession { id, request } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session
                        .handle_bridge_switch_session(host, id, request)
                        .await;
                });
            }
            SessionBridgeEvent::Reload { id } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session.handle_bridge_reload(host, id).await;
                });
            }
            SessionBridgeEvent::SetupEntries { id, request, .. } => {
                let session = Arc::clone(self);
                let host = Arc::clone(host);
                tokio::spawn(async move {
                    session.handle_bridge_setup_entries(host, id, request).await;
                });
            }
            SessionBridgeEvent::ReplacementReady { .. }
            | SessionBridgeEvent::ReplacementAbort { .. } => {
                self.report_extension_error(
                    "replacement control unexpectedly reached session bridge dispatch",
                );
            }
        }
    }

    async fn handle_bridge_new_session(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
        request: protocol::SessionNewSessionRequest,
    ) {
        if host.is_pending_busy() {
            self.respond_bridge_busy(&host, id, protocol::SESSION_NEW_SESSION_METHOD)
                .await;
            return;
        }
        let Some(runtime) = self.runtime_handle() else {
            self.respond_bridge_error(
                &host,
                id,
                protocol::SESSION_NEW_SESSION_METHOD,
                "session runtime is unavailable",
            )
            .await;
            return;
        };
        let replacement_lock = runtime.replacement_lock();
        let guard = replacement_lock.lock().await;
        let prepared = runtime
            .prepare_new_session(NewSessionOptions {
                parent_session: request.parent_session,
            })
            .await;
        match prepared {
            Ok(PrepareReplacementOutcome::Cancelled) => {
                drop(guard);
                if let Err(error) = host.respond_new_session(id, true, None).await {
                    self.report_extension_error(format!("newSession response: {error}"));
                }
            }
            Ok(PrepareReplacementOutcome::Prepared(prepared)) => {
                let installed = install_bridge_replacement(&host, prepared);
                drop(guard);
                let (token, ready_rx) = match installed {
                    Ok(installed) => installed,
                    Err(prepared) => {
                        runtime.abort_prepared_replacement(prepared).await;
                        self.respond_bridge_busy(&host, id, protocol::SESSION_NEW_SESSION_METHOD)
                            .await;
                        return;
                    }
                };
                if let Err(error) = host.respond_new_session(id, false, Some(&token)).await {
                    abort_bridge_pending(&host, &runtime, &token).await;
                    self.report_extension_error(format!("newSession response: {error}"));
                    return;
                }
                self.await_bridge_replacement(host, runtime, token, ready_rx, "newSession")
                    .await;
            }
            Err(error) => {
                drop(guard);
                self.respond_bridge_runtime_error(
                    &host,
                    id,
                    protocol::SESSION_NEW_SESSION_METHOD,
                    error,
                )
                .await;
            }
        }
    }

    async fn handle_bridge_fork(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
        request: protocol::SessionForkRequest,
    ) {
        if host.is_pending_busy() {
            self.respond_bridge_busy(&host, id, protocol::SESSION_FORK_METHOD)
                .await;
            return;
        }
        let Some(runtime) = self.runtime_handle() else {
            self.respond_bridge_error(
                &host,
                id,
                protocol::SESSION_FORK_METHOD,
                "session runtime is unavailable",
            )
            .await;
            return;
        };
        let position = match request.position.unwrap_or(SessionForkPosition::Before) {
            SessionForkPosition::Before => ForkPosition::Before,
            SessionForkPosition::At => ForkPosition::At,
        };
        let replacement_lock = runtime.replacement_lock();
        let guard = replacement_lock.lock().await;
        let prepared = runtime.prepare_fork(&request.entry_id, position).await;
        match prepared {
            Ok((PrepareReplacementOutcome::Cancelled, _)) => {
                drop(guard);
                if let Err(error) = host.respond_fork(id, true, None, None).await {
                    self.report_extension_error(format!("fork response: {error}"));
                }
            }
            Ok((PrepareReplacementOutcome::Prepared(prepared), selected_text)) => {
                let installed = install_bridge_replacement(&host, prepared);
                drop(guard);
                let (token, ready_rx) = match installed {
                    Ok(installed) => installed,
                    Err(prepared) => {
                        runtime.abort_prepared_replacement(prepared).await;
                        self.respond_bridge_busy(&host, id, protocol::SESSION_FORK_METHOD)
                            .await;
                        return;
                    }
                };
                if let Err(error) = host
                    .respond_fork(id, false, selected_text.as_deref(), Some(&token))
                    .await
                {
                    abort_bridge_pending(&host, &runtime, &token).await;
                    self.report_extension_error(format!("fork response: {error}"));
                    return;
                }
                self.await_bridge_replacement(host, runtime, token, ready_rx, "fork")
                    .await;
            }
            Err(error) => {
                drop(guard);
                self.respond_bridge_runtime_error(&host, id, protocol::SESSION_FORK_METHOD, error)
                    .await;
            }
        }
    }

    async fn handle_bridge_switch_session(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
        request: protocol::SessionSwitchSessionRequest,
    ) {
        if host.is_pending_busy() {
            self.respond_bridge_busy(&host, id, protocol::SESSION_SWITCH_SESSION_METHOD)
                .await;
            return;
        }
        let Some(runtime) = self.runtime_handle() else {
            self.respond_bridge_error(
                &host,
                id,
                protocol::SESSION_SWITCH_SESSION_METHOD,
                "session runtime is unavailable",
            )
            .await;
            return;
        };
        let replacement_lock = runtime.replacement_lock();
        let guard = replacement_lock.lock().await;
        let prepared = runtime
            .prepare_switch_session(&request.session_path, SwitchSessionOptions::default())
            .await;
        match prepared {
            Ok(PrepareReplacementOutcome::Cancelled) => {
                drop(guard);
                if let Err(error) = host.respond_switch_session(id, true, None).await {
                    self.report_extension_error(format!("switchSession response: {error}"));
                }
            }
            Ok(PrepareReplacementOutcome::Prepared(prepared)) => {
                let installed = install_bridge_replacement(&host, prepared);
                drop(guard);
                let (token, ready_rx) = match installed {
                    Ok(installed) => installed,
                    Err(prepared) => {
                        runtime.abort_prepared_replacement(prepared).await;
                        self.respond_bridge_busy(
                            &host,
                            id,
                            protocol::SESSION_SWITCH_SESSION_METHOD,
                        )
                        .await;
                        return;
                    }
                };
                if let Err(error) = host.respond_switch_session(id, false, Some(&token)).await {
                    abort_bridge_pending(&host, &runtime, &token).await;
                    self.report_extension_error(format!("switchSession response: {error}"));
                    return;
                }
                self.await_bridge_replacement(host, runtime, token, ready_rx, "switchSession")
                    .await;
            }
            Err(error) => {
                drop(guard);
                self.respond_bridge_runtime_error(
                    &host,
                    id,
                    protocol::SESSION_SWITCH_SESSION_METHOD,
                    error,
                )
                .await;
            }
        }
    }

    async fn handle_bridge_navigate_tree(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
        request: protocol::SessionNavigateTreeRequest,
    ) {
        if self.is_disposed() {
            let _ = host
                .respond_navigate_tree(id, Err("session replaced".to_owned()))
                .await;
            return;
        }
        // The disposed check above is an early-out for a session already torn
        // down; it is not re-checked here because navigate_tree enforces the
        // same lifecycle rule. Disposal that races the (potentially long)
        // summarization cancels the branch-summary token, which
        // navigate_tree_inner observes and surfaces as an aborted result rather
        // than persisting a stale summary.

        let options = NavigateTreeOptions {
            summarize: request.summarize.unwrap_or(false),
            custom_instructions: request.custom_instructions,
            replace_instructions: request.replace_instructions.unwrap_or(false),
            label: request.label,
        };
        let outcome = if options.summarize {
            match self
                .resolve_summarization_inputs("branch summarization")
                .await
            {
                Ok((auth, summarizer)) => {
                    self.navigate_tree(&request.target_id, options, auth, Some(&summarizer))
                        .await
                }
                Err(error) => {
                    let _ = host.respond_navigate_tree(id, Err(error.to_string())).await;
                    return;
                }
            }
        } else {
            self.navigate_tree(
                &request.target_id,
                options,
                super::tree::SummarizationAuth::default(),
                None,
            )
            .await
        };
        let outcome = match outcome {
            Ok(result) => {
                let summary_entry = match result.summary_entry.map(serde_json::to_value).transpose()
                {
                    Ok(entry) => entry,
                    Err(error) => {
                        let _ = host
                            .respond_navigate_tree(
                                id,
                                Err(format!("encode navigateTree summary: {error}")),
                            )
                            .await;
                        return;
                    }
                };
                Ok(SessionNavigateTreeResponse {
                    cancelled: result.cancelled,
                    editor_text: result.editor_text,
                    aborted: result.aborted.then_some(true),
                    summary_entry,
                })
            }
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = host.respond_navigate_tree(id, outcome).await {
            self.report_extension_error(format!("navigateTree response: {error}"));
        }
    }

    async fn handle_bridge_reload(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
    ) {
        if host.is_pending_busy() {
            self.respond_bridge_busy(&host, id, protocol::SESSION_RELOAD_METHOD)
                .await;
            return;
        }
        let Some(model_runtime) = self.model_runtime() else {
            self.respond_bridge_error(
                &host,
                id,
                protocol::SESSION_RELOAD_METHOD,
                "model runtime is unavailable",
            )
            .await;
            return;
        };
        let flags = self.hooks.runner().get_flag_values();
        let reload_guard = host.reload_lock().lock().await;
        if host.is_pending_busy() {
            drop(reload_guard);
            self.respond_bridge_busy(&host, id, protocol::SESSION_RELOAD_METHOD)
                .await;
            return;
        }
        if !host.can_reload() {
            drop(reload_guard);
            self.respond_bridge_error(
                &host,
                id,
                protocol::SESSION_RELOAD_METHOD,
                "extension runtime is not reloadable",
            )
            .await;
            return;
        }
        let prepared = match host.prepare_reload(flags).await {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(reload_guard);
                self.respond_bridge_error(
                    &host,
                    id,
                    protocol::SESSION_RELOAD_METHOD,
                    &error.to_string(),
                )
                .await;
                return;
            }
        };
        let token = host.next_replacement_token();
        let Ok(ready_rx) = host.install_pending(
            token.clone(),
            PendingReadyOp::Reload {
                prepared,
                model_runtime,
            },
        ) else {
            drop(reload_guard);
            self.respond_bridge_busy(&host, id, protocol::SESSION_RELOAD_METHOD)
                .await;
            return;
        };
        drop(reload_guard);
        if let Err(error) = host.respond_reload(id, Ok(Some(&token))).await {
            let _ = host.abort_pending(&token);
            self.report_extension_error(format!("reload response: {error}"));
            return;
        }
        self.await_bridge_reload(host, token, ready_rx).await;
    }

    /// Handle a correlated `session.setupEntries` request: validate the
    /// replacement token and return the authoritative current entries from
    /// the pending replacement target session. Stale tokens fail closed.
    async fn handle_bridge_setup_entries(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        id: protocol::FrameId,
        request: SessionSetupEntriesRequest,
    ) {
        let Some(target) = host.validate_setup_token(&request.replacement_token) else {
            let _ = host
                .respond_setup_entries(id, Err("stale replacement token".to_owned()))
                .await;
            return;
        };
        // Build the whole snapshot before responding. A partial entry list
        // would lie about the authoritative session state.
        let entries = {
            let sm = target.session_manager.lock().await;
            sm.get_entries()
                .iter()
                .map(|entry| serde_json::to_value(*entry))
                .collect::<Result<Vec<Value>, _>>()
        };
        let outcome = entries
            .map(|entries| protocol::SessionSetupEntriesResponse { entries })
            .map_err(|error| format!("serialize session entries: {error}"));
        let _ = host.respond_setup_entries(id, outcome).await;
    }

    async fn await_bridge_replacement(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        runtime: Arc<AgentSessionRuntime>,
        token: String,
        ready_rx: tokio::sync::oneshot::Receiver<()>,
        operation: &'static str,
    ) {
        if let Ok(()) = ready_rx.await {
            let Some((op, _finalize_guard)) = host.take_finalizing(&token) else {
                self.report_extension_error(format!(
                    "{operation}: replacement ready state was lost"
                ));
                return;
            };
            match pending_replacement(op) {
                Ok(prepared) => {
                    let Some(result) = prepared.result.as_ref() else {
                        let _ = host.finish_finalize(&token);
                        self.report_extension_error(format!(
                            "{operation}: prepared replacement was already consumed"
                        ));
                        return;
                    };
                    // Route buffered bridge commands to the accepted replacement
                    // while teardown drains the prior runtime. Its unpublished
                    // binding suppresses global mirror writes until commit.
                    let new_session = Arc::clone(&result.session);
                    let _ = host.bind_session_target(Arc::downgrade(&new_session)).await;
                    runtime.finalize_replacement(prepared).await;
                    let Some((new_session, binding)) =
                        host.commit_session_replacement(&token).await
                    else {
                        self.report_extension_error(format!(
                            "{operation}: replacement target changed before commit"
                        ));
                        return;
                    };
                    new_session
                        .bind_session_mirror(
                            Arc::clone(&host),
                            Arc::downgrade(&new_session),
                            binding,
                        )
                        .await;
                }
                Err(op) => {
                    drop(op);
                    let _ = host.finish_finalize(&token);
                    self.report_extension_error(format!(
                        "{operation}: replacement ready state had the wrong operation"
                    ));
                }
            }
        } else {
            // Receiver closed without completion: a dropped readiness frame
            // aborted the matching pending token. Clean up any remaining
            // state and report the cause.
            abort_bridge_pending(&host, &runtime, &token).await;
            self.report_extension_error(format!(
                "{operation}: replacement ready wait ended before completion"
            ));
        }
    }

    async fn await_bridge_reload(
        self: &Arc<Self>,
        host: Arc<ExtensionRuntimeSet>,
        token: String,
        ready_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        if let Ok(()) = ready_rx.await {
            let Some((
                PendingReadyOp::Reload {
                    prepared,
                    model_runtime,
                },
                _finalize_guard,
            )) = host.take_finalizing(&token)
            else {
                self.report_extension_error("reload: replacement ready state was lost");
                return;
            };
            let _ = self
                .hooks
                .runner()
                .emit(AgentSessionEvent::SessionShutdown {
                    reason: SessionShutdownReason::Reload,
                    target_session_file: None,
                })
                .await;
            let reload_guard = host.reload_lock().lock().await;
            let reload = host.commit_reload(&model_runtime, prepared).await;
            let _ = host.finish_finalize(&token);
            drop(reload_guard);
            if !reload.committed {
                self.report_extension_error(
                    "reload: extension runtime was invalidated before commit",
                );
                return;
            }
            self.refresh_tool_registry(&RefreshToolRegistryOptions {
                active_tool_names: None,
                include_all_extension_tools: true,
            });
            for diagnostic in reload.diagnostics {
                self.report_extension_error(diagnostic.to_string());
            }
            self.refresh_selected_model_from_runtime();
            self.hydrate_replacement_host().await;
            self.emit_session_start_reload().await;
            if let Err(error) = self
                .extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                .await
            {
                self.report_extension_error(format!("reload resources: {error}"));
            }
        } else {
            // Receiver closed without completion: a dropped readiness frame
            // aborted the matching pending token.
            let _ = host.abort_pending(&token);
            self.report_extension_error("reload: replacement ready wait ended before completion");
        }
    }

    async fn respond_bridge_runtime_error(
        &self,
        host: &ExtensionRuntimeSet,
        id: protocol::FrameId,
        method: &str,
        error: AgentSessionRuntimeError,
    ) {
        if matches!(error, AgentSessionRuntimeError::ReplacementBusy) {
            self.respond_bridge_busy(host, id, method).await;
        } else {
            self.respond_bridge_error(host, id, method, &error.to_string())
                .await;
        }
    }

    async fn respond_bridge_busy(
        &self,
        host: &ExtensionRuntimeSet,
        id: protocol::FrameId,
        method: &str,
    ) {
        if let Err(error) = host.respond_replacement_busy(id, method).await {
            self.report_extension_error(format!("{method} busy response: {error}"));
        }
    }

    async fn respond_bridge_error(
        &self,
        host: &ExtensionRuntimeSet,
        id: protocol::FrameId,
        method: &str,
        message: &str,
    ) {
        if let Err(error) = host.respond_session_error(id, method, message).await {
            self.report_extension_error(format!("{method} error response: {error}"));
        }
    }

    async fn apply_session_command(self: &Arc<Self>, command: SessionCommand) {
        match command {
            SessionCommand::SendMessage { message, options } => {
                let input = match custom_message_input(&message) {
                    Ok(input) => input,
                    Err(error) => {
                        self.report_extension_error(format!("sendMessage: {error}"));
                        return;
                    }
                };
                let trigger_turn = options
                    .as_ref()
                    .and_then(|o| o.get("triggerTurn"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let deliver_as = options
                    .as_ref()
                    .and_then(|o| o.get("deliverAs"))
                    .and_then(Value::as_str)
                    .and_then(parse_deliver_as);
                if let Err(error) = self
                    .send_custom_message(input, trigger_turn, deliver_as)
                    .await
                {
                    self.report_extension_error(format!("sendMessage: {error}"));
                }
            }
            SessionCommand::SendUserMessage { content, options } => {
                let (text, images) = user_message_parts(&content);
                let deliver_as = options
                    .as_ref()
                    .and_then(|o| o.get("deliverAs"))
                    .and_then(Value::as_str)
                    .and_then(parse_deliver_as)
                    // `nextTurn` is not a valid sendUserMessage delivery mode.
                    .filter(|mode| *mode != DeliverAs::NextTurn);
                if let Err(error) = self.send_user_message(&text, images, deliver_as).await {
                    self.report_extension_error(format!("sendUserMessage: {error}"));
                }
            }
            SessionCommand::AppendEntry { custom_type, data } => {
                let entry = {
                    let mut manager = self.session_manager.lock().await;
                    match manager.append_custom_entry(&custom_type, data) {
                        Ok(id) => manager.get_entry(&id).cloned(),
                        Err(error) => {
                            drop(manager);
                            self.report_extension_error(format!("appendEntry: {error}"));
                            return;
                        }
                    }
                };
                if let Some(entry) = entry {
                    self.emit_public(AgentSessionEvent::EntryAppended { entry });
                }
            }
            SessionCommand::SetSessionName { name } => {
                if let Err(error) = self.set_session_name(&name).await {
                    self.report_extension_error(format!("setSessionName: {error}"));
                }
            }
            SessionCommand::SetLabel { entry_id, label } => {
                let result = self
                    .session_manager
                    .lock()
                    .await
                    .append_label_change(&entry_id, label.as_deref());
                if let Err(error) = result {
                    self.report_extension_error(format!("setLabel: {error}"));
                }
            }
            SessionCommand::SetActiveTools { tool_names } => {
                self.set_active_tools_by_name(tool_names);
            }
            SessionCommand::RefreshTools => {
                self.refresh_tool_registry(&RefreshToolRegistryOptions::default());
            }
            SessionCommand::SetThinkingLevel { level } => match parse_thinking_level(&level) {
                Some(level) => {
                    let _ = self.set_thinking_level(level).await;
                }
                None => {
                    self.report_extension_error(format!(
                        "setThinkingLevel: unknown level: {level}"
                    ));
                }
            },
            SessionCommand::Abort => self.abort().await,
            SessionCommand::Shutdown => self.invoke_extension_shutdown_handler(),
        }
    }
}

#[allow(clippy::result_large_err)]
fn install_bridge_replacement(
    host: &ExtensionRuntimeSet,
    prepared: PreparedReplacement,
) -> Result<(String, tokio::sync::oneshot::Receiver<()>), PreparedReplacement> {
    let token = host.next_replacement_token();
    let mut prepared = prepared;
    let Some(result) = prepared.result.take() else {
        return Err(prepared);
    };
    let reason = prepared.reason;
    let target_session_file = prepared.target_session_file.take();
    match host.install_pending(
        token.clone(),
        PendingReadyOp::Replacement {
            result,
            reason,
            target_session_file,
        },
    ) {
        Ok(ready_rx) => Ok((token, ready_rx)),
        Err(PendingReadyOp::Replacement {
            result,
            reason,
            target_session_file,
        }) => Err(PreparedReplacement {
            result: Some(result),
            reason,
            target_session_file,
        }),
        Err(PendingReadyOp::Reload { .. }) => {
            unreachable!("installed replacement returned a reload operation")
        }
    }
}

#[allow(clippy::result_large_err)]
fn pending_replacement(op: PendingReadyOp) -> Result<PreparedReplacement, PendingReadyOp> {
    match op {
        PendingReadyOp::Replacement {
            result,
            reason,
            target_session_file,
        } => Ok(PreparedReplacement {
            result: Some(result),
            reason,
            target_session_file,
        }),
        reload @ PendingReadyOp::Reload { .. } => Err(reload),
    }
}

async fn abort_bridge_pending(
    host: &ExtensionRuntimeSet,
    runtime: &AgentSessionRuntime,
    token: &str,
) {
    let Some(op) = host.abort_pending(token) else {
        return;
    };
    match pending_replacement(op) {
        Ok(prepared) => runtime.abort_prepared_replacement(prepared).await,
        Err(op) => drop(op),
    }
}

async fn dispatch_session_bridge(
    host: &Arc<ExtensionRuntimeSet>,
    item: SessionBridgeEvent,
    diagnostic_session: &std::sync::Weak<AgentSession>,
) {
    let route = host.route_session_bridge(&item);
    dispatch_session_bridge_route(host, item, route, diagnostic_session).await;
}

async fn dispatch_session_bridge_route(
    host: &Arc<ExtensionRuntimeSet>,
    item: SessionBridgeEvent,
    route: SessionBridgeRoute,
    diagnostic_session: &std::sync::Weak<AgentSession>,
) {
    match route {
        SessionBridgeRoute::Active { target, binding } => {
            target.apply_session_bridge_event(host, item).await;
            if !host.is_session_target_current(binding) {
                return;
            }
            let state = target.session_state_wire().await;
            let _ = host.push_session_state_for_binding(binding, &state).await;
        }
        SessionBridgeRoute::Candidate(target) => {
            target.apply_session_bridge_event(host, item).await;
        }
        SessionBridgeRoute::Operation => match item {
            SessionBridgeEvent::ReplacementReady { token, .. } => {
                let _ = host.complete_ready(&token);
            }
            SessionBridgeEvent::ReplacementAbort { token, origin } => {
                let _ = host.abort_waiting_ready(&token, origin);
            }
            _ => {
                if let Some(session) = diagnostic_session.upgrade() {
                    session.report_extension_error(
                        "non-replacement event unexpectedly routed as a bridge operation",
                    );
                }
            }
        },
        SessionBridgeRoute::Rejected => {
            answer_unclaimed_bridge_event(host, item).await;
        }
    }
}

/// Answer a dequeued bridge event whose target session is gone.
///
/// The bridge loop dequeues an item before checking for a live target. When
/// the target is `None` the item must still be answered — correlated
/// requests (`setModel`, `compact`, …) would otherwise hang the host.
/// Fire-and-forget commands need no response. Rejected replacement controls
/// must not mutate the pending operation.
async fn answer_unclaimed_bridge_event(host: &Arc<ExtensionRuntimeSet>, event: SessionBridgeEvent) {
    match event {
        SessionBridgeEvent::Command { .. }
        | SessionBridgeEvent::ReplacementReady { .. }
        | SessionBridgeEvent::ReplacementAbort { .. } => {}
        SessionBridgeEvent::SetModel { id, .. } => {
            let _ = host.respond_set_model(id, false).await;
        }
        SessionBridgeEvent::Compact { id, .. } => {
            let _ = host
                .respond_compact(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::NewSession { id, .. } => {
            let _ = host
                .respond_session_error(
                    id,
                    protocol::SESSION_NEW_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        SessionBridgeEvent::Fork { id, .. } => {
            let _ = host
                .respond_session_error(id, protocol::SESSION_FORK_METHOD, "no active session")
                .await;
        }
        SessionBridgeEvent::NavigateTree { id, .. } => {
            let _ = host
                .respond_navigate_tree(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::SwitchSession { id, .. } => {
            let _ = host
                .respond_session_error(
                    id,
                    protocol::SESSION_SWITCH_SESSION_METHOD,
                    "no active session",
                )
                .await;
        }
        SessionBridgeEvent::Reload { id } => {
            let _ = host
                .respond_reload(id, Err("no active session".to_owned()))
                .await;
        }
        SessionBridgeEvent::SetupEntries { id, .. } => {
            let _ = host
                .respond_setup_entries(id, Err("no active session".to_owned()))
                .await;
        }
    }
}

/// Wire string for a thinking level (serde `lowercase` discriminant).
fn thinking_level_wire(level: ModelThinkingLevel) -> String {
    serde_json::to_value(level)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "medium".to_owned())
}

/// Parse a wire thinking-level discriminant.
fn parse_thinking_level(raw: &str) -> Option<ModelThinkingLevel> {
    serde_json::from_value(Value::String(raw.to_owned())).ok()
}

/// Parse an upstream `deliverAs` discriminant.
fn parse_deliver_as(raw: &str) -> Option<DeliverAs> {
    match raw {
        "steer" => Some(DeliverAs::Steer),
        "followUp" => Some(DeliverAs::FollowUp),
        "nextTurn" => Some(DeliverAs::NextTurn),
        _ => None,
    }
}

/// Convert the wire `sendMessage` payload into a [`CustomMessageInput`].
fn custom_message_input(message: &Value) -> Result<CustomMessageInput, String> {
    let custom_type = message
        .get("customType")
        .and_then(Value::as_str)
        .ok_or("customType is required")?
        .to_owned();
    let content = message.get("content").cloned().map_or(
        Ok(CustomMessageContent::Text(String::new())),
        |value| {
            serde_json::from_value::<CustomMessageContent>(value)
                .map_err(|error| format!("invalid content: {error}"))
        },
    )?;
    let display = message
        .get("display")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let details = message.get("details").filter(|v| !v.is_null()).cloned();
    Ok(CustomMessageInput {
        custom_type,
        content,
        display,
        details,
    })
}

/// Split a wire `sendUserMessage` content (string or block array) into the
/// prompt text and image attachments.
fn user_message_parts(content: &Value) -> (String, Vec<ImageContent>) {
    match content {
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(blocks) => {
            let mut texts: Vec<&str> = Vec::new();
            let mut images = Vec::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text);
                        }
                    }
                    Some("image") => {
                        if let Ok(image) = serde_json::from_value::<ImageContent>(block.clone()) {
                            images.push(image);
                        }
                    }
                    _ => {}
                }
            }
            (texts.join("\n"), images)
        }
        _ => (String::new(), Vec::new()),
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

    /// Runner that records an ordered lifecycle log and supports toggling
    /// handler presence plus an optional emit delay (concurrency tests).
    struct TestRunner {
        has_start: AtomicBool,
        has_shutdown: AtomicBool,
        has_resources: AtomicBool,
        /// Unified ordered call log: `session_start:{reason}:{prev|-}`,
        /// `session_shutdown:{reason}:{target|-}`,
        /// `resources_discover:{reason}`.
        calls: Arc<Mutex<Vec<String>>>,
        emit_delay: Mutex<Option<std::time::Duration>>,
        flag_values: Arc<Mutex<HashMap<String, serde_json::Value>>>,
        resource_paths: Arc<Mutex<crate::core::resources::ResourceExtensionPaths>>,
    }

    impl TestRunner {
        fn new() -> Self {
            Self {
                has_start: AtomicBool::new(false),
                has_shutdown: AtomicBool::new(false),
                has_resources: AtomicBool::new(false),
                calls: Arc::new(Mutex::new(Vec::new())),
                emit_delay: Mutex::new(None),
                flag_values: Arc::new(Mutex::new(HashMap::new())),
                resource_paths: Arc::new(Mutex::new(
                    crate::core::resources::ResourceExtensionPaths::default(),
                )),
            }
        }

        fn record(&self, entry: String) {
            if let Ok(mut g) = self.calls.lock() {
                g.push(entry);
            }
        }

        fn lifecycle_label(event: &AgentSessionEvent) -> String {
            match event {
                AgentSessionEvent::SessionStart {
                    reason,
                    previous_session_file,
                } => format!(
                    "session_start:{}:{}",
                    reason.as_str(),
                    previous_session_file.as_deref().unwrap_or("-")
                ),
                AgentSessionEvent::SessionShutdown {
                    reason,
                    target_session_file,
                } => format!(
                    "session_shutdown:{}:{}",
                    reason.as_str(),
                    target_session_file.as_deref().unwrap_or("-")
                ),
                other => other.type_name().to_owned(),
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
            event: AgentSessionEvent,
        ) -> BoxFuture<
            '_,
            Result<
                Option<super::super::extension_runner::CancelResult>,
                super::super::extension_runner::ExtensionRunnerError,
            >,
        > {
            let delay = self
                .emit_delay
                .lock()
                .map(|guard| *guard)
                .unwrap_or_default();
            let label = Self::lifecycle_label(&event);
            Box::pin(async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                self.record(label);
                Ok(None)
            })
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
            self.record(format!("resources_discover:{reason}"));
            let _ = cwd;
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
            ..Default::default()
        };
        session.bind_extensions(bindings).await?;

        // Resource discovery invoked with "startup".
        let calls = locked_clone(&runner.calls, "calls")?;
        assert!(calls.iter().any(|c| c == "resources_discover:startup"));

        // Bindings recorded.
        assert_eq!(session.extension_mode(), Some(ExtensionMode::Rpc));

        // Error listener is routed.
        session.report_extension_error("boom");
        assert!(error_hit.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn bind_emits_stored_session_start_before_discovery() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_start.store(true, Ordering::SeqCst);
        runner.has_resources.store(true, Ordering::SeqCst);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;

        session
            .bind_extensions(ExtensionBindings::default())
            .await?;
        let calls = locked_clone(&runner.calls, "calls")?;
        assert_eq!(
            calls,
            vec![
                "session_start:startup:-".to_owned(),
                "resources_discover:startup".to_owned(),
            ],
            "first bind must emit session_start exactly once, before discovery"
        );

        // Second bind: no re-emission (take-guard), no second discovery.
        session
            .bind_extensions(ExtensionBindings::default())
            .await?;
        let calls = locked_clone(&runner.calls, "calls")?;
        assert_eq!(
            calls,
            vec![
                "session_start:startup:-".to_owned(),
                "resources_discover:startup".to_owned(),
            ],
            "second bind must not re-emit or rediscover"
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_binds_emit_start_once_before_any_discovery() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_start.store(true, Ordering::SeqCst);
        runner.has_resources.store(true, Ordering::SeqCst);
        *runner
            .emit_delay
            .lock()
            .map_err(|_| io::Error::other("emit delay mutex poisoned"))? =
            Some(std::time::Duration::from_millis(25));
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;

        let (first, second) = tokio::join!(
            session.bind_extensions(ExtensionBindings::default()),
            session.bind_extensions(ExtensionBindings::default()),
        );
        first?;
        second?;

        let calls = locked_clone(&runner.calls, "calls")?;
        assert_eq!(
            calls,
            vec![
                "session_start:startup:-".to_owned(),
                "resources_discover:startup".to_owned(),
            ],
            "concurrent binds must serialize: one start strictly before one discovery"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bind_emits_replacement_reason_and_previous_file() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_start.store(true, Ordering::SeqCst);
        runner.has_resources.store(true, Ordering::SeqCst);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        config.session_start_event = Some(SessionStartEvent {
            reason: SessionStartReason::New,
            previous_session_file: Some("prev.jsonl".into()),
        });
        let session = AgentSession::new(config)?;

        session
            .bind_extensions(ExtensionBindings::default())
            .await?;
        let calls = locked_clone(&runner.calls, "calls")?;
        assert_eq!(
            calls,
            vec![
                "session_start:new:prev.jsonl".to_owned(),
                // Non-reload replacement reasons map to "startup" for
                // resources_discover (wire contract: startup|reload only).
                "resources_discover:startup".to_owned(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn extension_resources_refresh_session_skills_and_system_prompt() -> TestResult {
        let temp = tempfile::tempdir()?;
        let cwd = temp.path().join("project");
        let agent_dir = temp.path().join("agent");
        let base_skill_dir = agent_dir.join("skills").join("base-skill");
        let extension_dir = temp.path().join("extension");
        let skill_dir = extension_dir.join("skills");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent_dir)?;
        std::fs::create_dir_all(&base_skill_dir)?;
        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(
            base_skill_dir.join("SKILL.md"),
            "---\nname: base-skill\ndescription: base\n---\nbody\n",
        )?;
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

        let mut loader = crate::core::resources::DefaultResourceLoader::new(
            crate::core::resources::DefaultResourceLoaderOptions {
                cwd: cwd.clone(),
                agent_dir,
                project_trusted: true,
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
        let mut baseline_skills = locked_clone(&session.skills, "skills")?;
        assert!(
            baseline_skills
                .iter()
                .any(|skill| skill.name == "extension-skill")
        );
        assert!(
            baseline_skills
                .iter()
                .any(|skill| skill.name == "base-skill")
        );
        baseline_skills.retain(|skill| skill.name != "extension-skill");
        baseline_skills.sort_unstable_by(|left, right| left.file_path.cmp(&right.file_path));

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
        let mut reloaded_skills = locked_clone(&session.skills, "skills")?;
        reloaded_skills.sort_unstable_by(|left, right| left.file_path.cmp(&right.file_path));
        assert_eq!(reloaded_skills, baseline_skills);
        Ok(())
    }

    #[tokio::test]
    async fn reload_refreshes_session_terminal_settings_from_disk() -> TestResult {
        let temp = tempfile::tempdir()?;
        let cwd = temp.path().join("project");
        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&agent_dir)?;
        let settings_path = agent_dir.join("settings.json");
        std::fs::write(&settings_path, r#"{ "terminal": { "hyperlinks": true } }"#)?;

        // One manager owns settings: the loader receives only the resolved
        // trust bit, and this manager moves into the session's slot. A stale
        // manager (or a dropped refresh) leaves the old overrides visible.
        let settings = crate::core::settings::SettingsManager::create(
            &cwd,
            Some(&agent_dir),
            crate::core::settings::SettingsManagerCreateOptions::new().project_trusted(true),
        );
        let loader = crate::core::resources::DefaultResourceLoader::new(
            crate::core::resources::DefaultResourceLoaderOptions {
                cwd: cwd.clone(),
                agent_dir: agent_dir.clone(),
                project_trusted: true,
                ..crate::core::resources::DefaultResourceLoaderOptions::default()
            },
        );

        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.cwd = cwd.to_string_lossy().into_owned();
        config.settings_manager = settings;
        config.resource_loader = Some(loader);
        let session = AgentSession::new(config)?;

        std::fs::write(
            &settings_path,
            r#"{ "terminal": { "hyperlinks": false, "trueColor": true } }"#,
        )?;
        session.reload().await?;

        let overrides = session.lock_settings().get_terminal_capability_overrides();
        assert_eq!(overrides.hyperlinks, Some(false));
        assert_eq!(overrides.true_color, Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn reload_emits_shutdown_start_discovery_in_order() -> TestResult {
        let runner = Arc::new(TestRunner::new());
        runner.has_shutdown.store(true, Ordering::SeqCst);
        runner.has_start.store(true, Ordering::SeqCst);
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
            .calls
            .lock()
            .map_err(|_| io::Error::other("calls mutex poisoned"))?
            .clear();

        session.reload().await?;

        let calls = locked_clone(&runner.calls, "calls")?;
        assert_eq!(
            calls,
            vec![
                "session_shutdown:reload:-".to_owned(),
                "session_start:reload:-".to_owned(),
                "resources_discover:reload".to_owned(),
            ],
            "reload must emit shutdown, then start, then rediscover"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_restart_prepare_keeps_old_runner_provider_and_transport_live() -> TestResult {
        let old_provider = serde_json::json!({
            "name": "old-provider",
            "baseUrl": "https://old.example/v1",
            "api": "openai-completions",
            "models": [{
                "id": "old-model",
                "name": "Old model",
                "api": "openai-completions",
                "baseUrl": "https://old.example/v1",
                "reasoning": false
            }]
        });
        let (runner, host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "providers": [old_provider],
                "handlers": ["input", "session_start", "session_shutdown"],
                "terminalInput": false
            }))
            .await?;
        let runtime_set = crate::core::extension_runtime_set::ExtensionRuntimeSet::bind(vec![(
            crate::core::extension_runtime_set::EndpointKind::TsCompat,
            runner,
        )]);
        let runtime = Arc::new(crate::core::model_runtime::ModelRuntime::create_in_memory().await?);
        assert!(
            runtime_set
                .register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, outcome)| outcome.is_ok())
        );
        let provider_epoch = runtime.provider_mutation_epoch();
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runtime_set.clone());
        config.host_extension_runner = Some(runtime_set.clone());
        config.model_runtime = Some(Arc::clone(&runtime));
        let session = AgentSession::new(config)?;

        assert!(matches!(
            session.reload().await,
            Err(ExtensionBindError::HostRestart(_))
        ));
        assert_eq!(host.request_count("session_shutdown"), 0);
        assert_eq!(host.request_count("session_start"), 0);
        assert_eq!(runtime_set.reload_generation(), 1);
        assert!(runtime_set.is_active());
        assert_eq!(runtime.provider_mutation_epoch(), provider_epoch);
        assert_eq!(runtime.get_registered_provider_ids(), ["old-provider"]);
        assert!(runtime.get_model("old-provider", "old-model").is_some());
        let result = runtime_set
            .emit_input("original", None, "user", None)
            .await?;
        assert!(!result.handled);
        host.wait_for_request("input").await?;

        runtime_set.shutdown_once().await;
        Ok(())
    }

    #[tokio::test]
    async fn reload_rejects_multiple_endpoints_before_old_lifecycle_shutdown() -> TestResult {
        let (first, first_host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "handlers": ["session_shutdown"],
                "terminalInput": false
            }))
            .await?;
        let (second, second_host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "handlers": ["session_shutdown"],
                "terminalInput": false
            }))
            .await?;
        let runtime_set = crate::core::extension_runtime_set::ExtensionRuntimeSet::bind(vec![
            (
                crate::core::extension_runtime_set::EndpointKind::TsCompat,
                first,
            ),
            (
                crate::core::extension_runtime_set::EndpointKind::Native,
                second,
            ),
        ]);
        let runtime = Arc::new(crate::core::model_runtime::ModelRuntime::create_in_memory().await?);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runtime_set.clone());
        config.host_extension_runner = Some(runtime_set.clone());
        config.model_runtime = Some(runtime);
        let session = AgentSession::new(config)?;

        assert!(matches!(
            session.reload().await,
            Err(ExtensionBindError::HostRestart(_))
        ));
        assert_eq!(first_host.request_count("session_shutdown"), 0);
        assert_eq!(second_host.request_count("session_shutdown"), 0);
        assert!(runtime_set.is_active());

        runtime_set.shutdown_once().await;
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
        let calls = locked_clone(&runner.calls, "calls")?;
        assert!(calls.iter().any(|c| c == "session_shutdown:reload:-"));
        assert!(
            calls.iter().any(|c| c == "resources_discover:reload"),
            "reload must refresh resources even without bindings, got {calls:?}"
        );
        Ok(())
    }

    /// Runner whose lifecycle `emit` fails (dead host simulation).
    struct FailingRunner;

    impl ExtensionRunner for FailingRunner {
        fn has_handlers(&self, _event: &str) -> bool {
            true
        }

        // Lifecycle emit fails; all other methods default to null-runner
        // behavior.
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
            Box::pin(async {
                Err(
                    super::super::extension_runner::ExtensionRunnerError::Failed(
                        "host gone".into(),
                    ),
                )
            })
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
    async fn reload_survives_lifecycle_emit_error() -> TestResult {
        // Lifecycle emit failures are isolated (host error reporting), never
        // fatal: reload must still complete resource rediscovery.
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(Arc::new(FailingRunner) as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;
        session.reload().await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn bridge_reload_ready_wait_has_no_hook_deadline() -> TestResult {
        // Drive the production await_bridge_reload path: the ready wait must
        // have no hook deadline, so advancing paused time past the old 30s
        // timeout must not abort the operation. complete_ready must then
        // finish it successfully.
        let session = make_session()?;
        let set = ExtensionRuntimeSet::bind(Vec::new());
        let runtime = Arc::new(crate::core::model_runtime::ModelRuntime::create_in_memory().await?);
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Reload {
                    prepared: crate::core::extension_runtime_set::PreparedReload::empty_for_test(),
                    model_runtime: Arc::clone(&runtime),
                },
            )
            .map_err(|_| "initial pending install was rejected")?;

        // Spawn the production waiter.
        let wait_session = Arc::clone(&session);
        let wait_set = Arc::clone(&set);
        let wait_token = token.clone();
        let wait_task = tokio::spawn(async move {
            wait_session
                .await_bridge_reload(wait_set, wait_token, ready_rx)
                .await;
        });

        // Let the waiter arm any production deadline before virtual time moves.
        tokio::task::yield_now().await;

        // Advance time well beyond the old 30-second deadline.
        tokio::time::advance(std::time::Duration::from_mins(1)).await;
        tokio::task::yield_now().await;

        // The operation must still be pending and the production task
        // unfinished — no deadline fired.
        assert!(
            set.is_pending_busy(),
            "operation must remain pending past the old deadline"
        );
        assert!(
            !wait_task.is_finished(),
            "production await_bridge_reload must not finish after advancing time past the old deadline"
        );

        // Now complete_ready — the production task must finish. A
        // PreparedReload with generation None causes commit_reload to
        // return uncommitted, but the waiter still completes.
        assert!(
            set.complete_ready(&token),
            "complete_ready must succeed for the matching token"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), wait_task)
            .await
            .map_err(|_| "production await_bridge_reload did not finish after complete_ready")??;
        assert!(
            !set.is_pending_busy(),
            "pending slot must be cleared after successful completion"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bind_survives_lifecycle_emit_error() -> TestResult {
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(Arc::new(FailingRunner) as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)?;
        session
            .bind_extensions(ExtensionBindings::default())
            .await?;
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
                on_error: Some(Arc::new(|_| {
                    // Deliberate unwind: proves listener panics are isolated.
                    std::panic::resume_unwind(Box::new("listener panic"));
                })),
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

    #[tokio::test]
    async fn endpoint_retirement_refreshes_the_session_tool_registry() -> TestResult {
        let (dead, dead_host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "tools": [
                {"name": "shared", "label": "dead", "description": "", "parameters": {}},
                {"name": "dead-only", "label": "dead", "description": "", "parameters": {}}
            ]
        }))
        .await?;
        let (live, _live_host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "tools": [
                {"name": "shared", "label": "live", "description": "", "parameters": {}}
            ]
        }))
        .await?;
        let runtime = ExtensionRuntimeSet::bind(vec![
            (
                crate::core::extension_runtime_set::EndpointKind::TsCompat,
                dead,
            ),
            (
                crate::core::extension_runtime_set::EndpointKind::Native,
                live,
            ),
        ]);
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.extension_runner = Some(runtime.clone());
        config.host_extension_runner = Some(runtime);
        let session = AgentSession::new(config)?;
        let initial_shared = session.get_tool("shared").ok_or("shared tool missing")?;
        assert!(session.get_tool("dead-only").is_some());

        dead_host.close().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let promoted = session
                    .get_tool("shared")
                    .is_some_and(|tool| !Arc::ptr_eq(&tool, &initial_shared));
                if session.get_tool("dead-only").is_none() && promoted {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        session.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn answer_unclaimed_bridge_event_answers_correlated_requests() -> TestResult {
        use crate::core::extension_host::SessionBridgeEvent;
        use crate::core::extension_runtime_set::{EndpointKind, ExtensionRuntimeSet};
        use pi_ext::protocol::{SessionCompactRequest, SessionSetModelRequest};

        let (runner, _host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "handlers": [],
                "terminalInput": false
            }))
            .await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);

        // Each correlated bridge event must be answered so the host does not
        // hang. The respond_* calls route through the set to the endpoint.
        // If answer_unclaimed_bridge_event dropped the item instead of
        // answering, the host's pending request would never resolve.
        answer_unclaimed_bridge_event(
            &set,
            SessionBridgeEvent::SetModel {
                id: 1,
                request: SessionSetModelRequest {
                    model: json!({"provider": "p", "id": "m"}),
                },
            },
        )
        .await;

        answer_unclaimed_bridge_event(
            &set,
            SessionBridgeEvent::Compact {
                id: 2,
                request: SessionCompactRequest {
                    custom_instructions: None,
                },
            },
        )
        .await;

        answer_unclaimed_bridge_event(&set, SessionBridgeEvent::Reload { id: 3 }).await;

        set.shutdown_once().await;
        Ok(())
    }

    // -- reload: model refresh + host hydration regressions -----------------

    /// Provider snapshot with an API key so `mark_configured_if_auth_present`
    /// marks the provider as auth-configured (required for `set_model`).
    fn provider_snapshot_with_auth(
        name: &str,
        base_url: &str,
        model_id: &str,
        context_window: u64,
    ) -> Value {
        json!({
            "name": name,
            "baseUrl": base_url,
            "api": "openai-completions",
            "apiKey": "sk-test-key",
            "models": [{
                "id": model_id,
                "name": model_id,
                "api": "openai-completions",
                "baseUrl": base_url,
                "reasoning": false,
                "contextWindow": context_window
            }]
        })
    }

    /// Build a session with a concrete host runner + model runtime, bind it,
    /// and register the initial provider. Returns the session, runtime, and
    /// host set so the test can drive a reload.
    async fn build_reload_session_with_provider(
        provider: Value,
    ) -> TestResult<(
        Arc<AgentSession>,
        Arc<crate::core::model_runtime::ModelRuntime>,
        Arc<ExtensionRuntimeSet>,
        crate::core::extension_runtime_set::tests::FakeHost,
    )> {
        let (runner, host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "providers": [provider],
            "handlers": ["session_start", "session_shutdown"],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![(
            crate::core::extension_runtime_set::EndpointKind::TsCompat,
            runner,
        )]);
        let runtime = Arc::new(crate::core::model_runtime::ModelRuntime::create_in_memory().await?);
        assert!(
            set.register_providers_on(&runtime)
                .into_iter()
                .all(|(_path, result)| result.is_ok()),
            "initial provider registration failed"
        );

        // Use the registered provider's model as the session model.
        let available = runtime.get_available_snapshot();
        let model = available
            .iter()
            .find(|m| m.provider == "reload-provider")
            .cloned()
            .ok_or("reload-provider model not found after registration")?;
        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), model)?;
        config.extension_runner = Some(set.clone());
        config.host_extension_runner = Some(set.clone());
        config.model_runtime = Some(Arc::clone(&runtime));
        let session = AgentSession::new(config)?;

        // Bind so the session bridge mirror is established.
        session
            .bind_extensions(ExtensionBindings {
                mode: Some(ExtensionMode::Rpc),
                ..Default::default()
            })
            .await?;
        Ok((session, runtime, set, host))
    }

    #[tokio::test]
    async fn reload_refreshes_selected_model_against_post_reload_registry() -> TestResult {
        // Initial provider has context_window 4096.
        let initial = provider_snapshot_with_auth(
            "reload-provider",
            "https://old.example/v1",
            "reload-model",
            4096,
        );
        let (session, _runtime, set, _old_host) =
            build_reload_session_with_provider(initial).await?;

        // The session model should have the initial context_window.
        let before = session.model();
        assert_eq!(before.provider, "reload-provider");
        assert_eq!(before.id, "reload-model");
        assert_eq!(before.context_window, 4096);

        // Capture the session entry count and subscribe to public events so
        // we can prove the reload produces no model-switch side effects.
        let entry_count_before = session.session_manager.lock().await.get_entries().len();
        let observed_events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&observed_events);
        let _unsub = session.subscribe(move |event| {
            if let Ok(mut log) = events_clone.lock() {
                log.push(event.type_name());
            }
        });

        // Prepare a replacement endpoint whose provider changed the model
        // definition (context_window 8192).
        let updated = provider_snapshot_with_auth(
            "reload-provider",
            "https://new.example/v1",
            "reload-model",
            8192,
        );
        let (replacement, replacement_host) =
            crate::core::extension_runtime_set::tests::make_runner(json!({
                "providers": [updated],
                "handlers": ["session_start", "session_shutdown"],
                "terminalInput": false
            }))
            .await?;
        replacement_host.set_response("flags.set", json!({"ok": true}));
        let (replacement_gen, pending) =
            crate::core::extension_runtime_set::generation_from_endpoints(
                2,
                vec![(
                    crate::core::extension_runtime_set::EndpointKind::TsCompat,
                    "<replacement>".to_owned(),
                    replacement,
                )],
            );
        set.inject_prepared_replacement_for_reload(replacement_gen, pending);

        // Drive the reload through the session — this commits the replacement,
        // re-registers providers, then calls refresh_selected_model_from_runtime
        // before emitting session_start{reload}.
        session.reload().await?;

        // The session model must now reflect the post-reload definition:
        // context_window changed from 4096 to 8192.
        let after = session.model();
        assert_eq!(after.provider, "reload-provider");
        assert_eq!(after.id, "reload-model");
        assert_eq!(
            after.context_window, 8192,
            "reload must re-resolve the selected model against the post-reload registry"
        );

        // The refresh is a provider-configuration update, not an explicit
        // model switch: no model_change entry is appended and no model_select
        // event is emitted.
        let entry_count_after = session.session_manager.lock().await.get_entries().len();
        assert_eq!(
            entry_count_after, entry_count_before,
            "reload model refresh must not append a model_change session entry"
        );
        let logged = observed_events
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        assert!(
            !logged.contains(&"model_select"),
            "reload model refresh must not emit model_select (events={logged:?})"
        );

        set.shutdown_once().await;
        replacement_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn live_provider_update_refreshes_selected_model_without_selection_side_effects()
    -> TestResult {
        let initial = provider_snapshot_with_auth(
            "reload-provider",
            "https://old.example/v1",
            "reload-model",
            4096,
        );
        let (session, _runtime, set, host) = build_reload_session_with_provider(initial).await?;
        let entry_count_before = session.session_manager.lock().await.get_entries().len();
        let observed_events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&observed_events);
        let _unsubscribe = session.subscribe(move |event| {
            if let Ok(mut events) = events_clone.lock() {
                events.push(event.type_name());
            }
        });

        host.emit(pi_ext::protocol::Frame {
            id: 0,
            kind: pi_ext::protocol::FrameKind::Event,
            method: pi_ext::protocol::PROVIDERS_UPDATE_METHOD.to_owned(),
            payload: json!({
                "providers": [provider_snapshot_with_auth(
                    "reload-provider",
                    "https://new.example/v1",
                    "reload-model",
                    8192,
                )]
            }),
        })
        .await;

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while session.model().context_window != 8192 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "live provider update did not refresh the selected model")?;
        assert_eq!(session.model().base_url, "https://new.example/v1");
        assert_eq!(
            session.session_manager.lock().await.get_entries().len(),
            entry_count_before,
            "provider refresh must not append a model_change session entry"
        );
        let logged = observed_events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default();
        assert!(
            !logged.contains(&"model_select"),
            "provider refresh must not emit model_select (events={logged:?})"
        );

        set.shutdown_once().await;
        host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn reload_hydrates_replacement_host_before_session_start_hooks() -> TestResult {
        let initial = provider_snapshot_with_auth(
            "reload-provider",
            "https://old.example/v1",
            "reload-model",
            4096,
        );
        let (session, _runtime, set, _old_host) =
            build_reload_session_with_provider(initial.clone()).await?;

        let (replacement, replacement_host) =
            crate::core::extension_runtime_set::tests::make_runner(json!({
                "providers": [initial.clone()],
                "handlers": ["session_start", "session_shutdown"],
                "terminalInput": false
            }))
            .await?;
        replacement_host.set_response("flags.set", json!({"ok": true}));
        let (replacement_gen, pending) =
            crate::core::extension_runtime_set::generation_from_endpoints(
                2,
                vec![(
                    crate::core::extension_runtime_set::EndpointKind::TsCompat,
                    "<replacement>".to_owned(),
                    replacement,
                )],
            );
        set.inject_prepared_replacement_for_reload(replacement_gen, pending);

        session.reload().await?;

        let methods = replacement_host.observed_methods();
        let update_index = methods
            .iter()
            .position(|method| method == "session.update")
            .ok_or("replacement host did not receive session.update")?;
        let start_index = methods
            .iter()
            .position(|method| method == "session_start")
            .ok_or("replacement host did not receive session_start")?;
        assert!(
            update_index < start_index,
            "session.update must precede session_start (methods={methods:?})"
        );

        let state = replacement_host
            .first_payload("session.update")
            .ok_or("replacement host did not retain session.update payload")?;
        assert_eq!(
            state.pointer("/model/provider"),
            Some(&serde_json::json!("reload-provider")),
            "hydrated session state must carry the selected model"
        );

        set.shutdown_once().await;
        replacement_host.wait_for_exit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn session_state_wire_produces_scoped_models() -> TestResult {
        use crate::core::agent_session::ScopedModel;
        use pi_ai::ModelThinkingLevel;

        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.scoped_models = vec![
            ScopedModel {
                model: test_model(),
                thinking_level: Some(ModelThinkingLevel::High),
            },
            ScopedModel {
                model: Model {
                    id: "other".to_owned(),
                    name: "other".to_owned(),
                    api: "test-api".to_owned(),
                    provider: "other-provider".to_owned(),
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
                },
                thinking_level: None,
            },
        ];
        let session = Arc::new(AgentSession::new(config)?);
        let state = session.session_state_wire().await;

        assert_eq!(state.scoped_models.len(), 2);
        assert_eq!(
            state.scoped_models[0].thinking_level,
            Some("high".to_owned())
        );
        assert!(state.scoped_models[1].thinking_level.is_none());
        assert_eq!(state.scoped_models[0].model["id"], "m");
        assert_eq!(state.scoped_models[1].model["provider"], "other-provider");
        Ok(())
    }

    fn session_name_command(name: &str, replacement_token: Option<&str>) -> SessionBridgeEvent {
        SessionBridgeEvent::Command {
            envelope: protocol::SessionCommandEnvelope {
                replacement_token: replacement_token.map(str::to_owned),
                command: SessionCommand::SetSessionName {
                    name: name.to_owned(),
                },
            },
            origin: None,
        }
    }

    #[tokio::test]
    async fn session_bridge_rebind_invalidates_committed_authority() -> TestResult {
        let first = make_session()?;
        let second = make_session()?;
        let set = ExtensionRuntimeSet::bind(Vec::new());
        let first_binding = set.bind_session_target(Arc::downgrade(&first)).await;
        let state = SessionStateWire::default();
        assert!(
            !set.push_session_state_for_binding(first_binding, &state)
                .await,
            "a staged target must not publish before its initial mirror"
        );
        assert!(set.activate_session_state(first_binding, &state).await);
        assert!(
            set.push_session_state_for_binding(first_binding, &state)
                .await
        );
        let ordinary = session_name_command("first", None);
        let SessionBridgeRoute::Active { target, binding } = set.route_session_bridge(&ordinary)
        else {
            return Err("tokenless command did not route to the active session".into());
        };
        assert!(Arc::ptr_eq(&target, &first));
        assert_eq!(binding, first_binding);

        let second_binding = set.bind_session_target(Arc::downgrade(&second)).await;
        assert!(!set.is_session_target_current(first_binding));
        assert!(set.is_session_target_current(second_binding));
        assert!(
            !set.push_session_state_for_binding(first_binding, &state)
                .await,
            "a stale publisher must not overwrite a replacement mirror"
        );
        assert!(
            !set.push_session_state_for_binding(second_binding, &state)
                .await,
            "a replacement must remain unpublished until finalization"
        );
        assert!(set.activate_session_state(second_binding, &state).await);
        let SessionBridgeRoute::Active { target, binding } = set.route_session_bridge(&ordinary)
        else {
            return Err("tokenless command did not follow the committed rebind".into());
        };
        assert!(Arc::ptr_eq(&target, &second));
        assert_eq!(binding, second_binding);

        first.dispose().await;
        second.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn losing_session_bridge_claim_does_not_rebind_target() -> TestResult {
        let first = make_session()?;
        let second = make_session()?;
        let set = ExtensionRuntimeSet::bind(Vec::new());
        first.set_host_extension_runner(Some(Arc::clone(&set)));
        second.set_host_extension_runner(Some(Arc::clone(&set)));

        first.bind_session_bridge().await;
        second.bind_session_bridge().await;

        let ordinary = session_name_command("first", None);
        let SessionBridgeRoute::Active { target, .. } = set.route_session_bridge(&ordinary) else {
            return Err("claimed bridge did not retain its target".into());
        };
        assert!(
            Arc::ptr_eq(&target, &first),
            "a session that lost the receiver claim rebound the global target"
        );

        first.dispose().await;
        second.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn session_bridge_routes_candidate_until_ready_finalizes() -> TestResult {
        use crate::core::agent_session_runtime::{
            AgentSessionRuntimeServices, CreateAgentSessionRuntimeResult,
        };
        use std::path::PathBuf;

        let candidate = make_session()?;
        let set = ExtensionRuntimeSet::bind(Vec::new());
        let token = set.next_replacement_token();
        let ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Replacement {
                    result: CreateAgentSessionRuntimeResult {
                        session: Arc::clone(&candidate),
                        services: AgentSessionRuntimeServices {
                            cwd: PathBuf::new(),
                            agent_dir: PathBuf::new(),
                        },
                        diagnostics: Vec::new(),
                        model_fallback_message: None,
                    },
                    reason: SessionShutdownReason::New,
                    target_session_file: None,
                },
            )
            .map_err(|_| "candidate pending install was rejected")?;
        let scoped = session_name_command("candidate", Some(&token));
        let SessionBridgeRoute::Candidate(target) = set.route_session_bridge(&scoped) else {
            return Err("scoped command did not route to the pending candidate".into());
        };
        assert!(Arc::ptr_eq(&target, &candidate));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::SetupEntries {
                id: 1,
                request: SessionSetupEntriesRequest {
                    replacement_token: token.clone(),
                },
                origin: None,
            }),
            SessionBridgeRoute::Candidate(_)
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementReady {
                token: token.clone(),
                origin: None,
            }),
            SessionBridgeRoute::Operation
        ));

        assert!(set.complete_ready(&token));
        ready_rx.await?;
        assert!(matches!(
            set.route_session_bridge(&scoped),
            SessionBridgeRoute::Candidate(_)
        ));
        assert!(matches!(
            set.route_session_bridge(&SessionBridgeEvent::ReplacementAbort {
                token: token.clone(),
                origin: None,
            }),
            SessionBridgeRoute::Rejected
        ));

        let (op, guard) = set
            .take_finalizing(&token)
            .ok_or("candidate operation was not finalizing")?;
        let staged_binding = set.bind_session_target(Arc::downgrade(&candidate)).await;
        drop(op);
        assert!(
            set.commit_session_replacement("stale-token")
                .await
                .is_none(),
            "a stale token committed the replacement target"
        );
        assert!(set.is_pending_busy());
        let (committed, binding) = set
            .commit_session_replacement(&token)
            .await
            .ok_or("matching token did not commit the replacement")?;
        assert!(Arc::ptr_eq(&committed, &candidate));
        assert_eq!(binding, staged_binding);
        assert!(!set.is_pending_busy());
        drop(guard);
        candidate.dispose().await;
        Ok(())
    }
    #[tokio::test]
    async fn candidate_command_does_not_publish_global_session_state() -> TestResult {
        use crate::core::agent_session_runtime::{
            AgentSessionRuntimeServices, CreateAgentSessionRuntimeResult,
        };
        use crate::core::extension_runtime_set::EndpointKind;
        use std::path::PathBuf;

        let (runner, fake_host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "handlers": ["input"],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let active = make_session()?;
        let candidate = make_session()?;
        let binding = set.bind_session_target(Arc::downgrade(&active)).await;
        let state = active.session_state_wire().await;
        assert!(set.activate_session_state(binding, &state).await);
        fake_host
            .wait_for_frame(protocol::SESSION_UPDATE_METHOD)
            .await?;
        let baseline = fake_host.frame_count(protocol::SESSION_UPDATE_METHOD);

        let token = set.next_replacement_token();
        let _ready_rx = set
            .install_pending(
                token.clone(),
                PendingReadyOp::Replacement {
                    result: CreateAgentSessionRuntimeResult {
                        session: Arc::clone(&candidate),
                        services: AgentSessionRuntimeServices {
                            cwd: PathBuf::new(),
                            agent_dir: PathBuf::new(),
                        },
                        diagnostics: Vec::new(),
                        model_fallback_message: None,
                    },
                    reason: SessionShutdownReason::New,
                    target_session_file: None,
                },
            )
            .map_err(|_| "candidate pending install was rejected")?;

        dispatch_session_bridge(
            &set,
            session_name_command("candidate-name", Some(&token)),
            &Arc::downgrade(&active),
        )
        .await;
        let _ = set.emit_input("barrier", None, "user", None).await?;
        fake_host.wait_for_request("input").await?;
        assert_eq!(
            fake_host.frame_count(protocol::SESSION_UPDATE_METHOD),
            baseline,
            "a candidate command must not publish the candidate mirror"
        );
        assert_eq!(
            candidate.session_state_wire().await.session_name.as_deref(),
            Some("candidate-name")
        );

        drop(set.abort_pending(&token));
        set.shutdown_once().await;
        active.dispose().await;
        candidate.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn unexpected_replacement_control_at_session_route_reports_and_continues() -> TestResult {
        use crate::core::extension_runtime_set::EndpointKind;

        let (runner, _fake_host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let session = make_session()?;
        let errors = Arc::new(Mutex::new(Vec::new()));
        let captured_errors = Arc::clone(&errors);
        session
            .bind_extensions(ExtensionBindings {
                on_error: Some(Arc::new(move |message| {
                    captured_errors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message.to_owned());
                })),
                ..Default::default()
            })
            .await?;
        let weak = Arc::downgrade(&session);
        let binding = set.bind_session_target(weak.clone()).await;

        dispatch_session_bridge_route(
            &set,
            SessionBridgeEvent::ReplacementReady {
                token: "unexpected".to_owned(),
                origin: None,
            },
            SessionBridgeRoute::Active {
                target: Arc::clone(&session),
                binding,
            },
            &weak,
        )
        .await;
        dispatch_session_bridge_route(
            &set,
            session_name_command("still-running", None),
            SessionBridgeRoute::Active {
                target: Arc::clone(&session),
                binding,
            },
            &weak,
        )
        .await;

        assert_eq!(
            errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["replacement control unexpectedly reached session bridge dispatch"]
        );
        assert_eq!(
            session.session_state_wire().await.session_name.as_deref(),
            Some("still-running")
        );
        set.shutdown_once().await;
        session.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn unexpected_operation_route_reports_and_continues() -> TestResult {
        use crate::core::extension_runtime_set::EndpointKind;

        let (runner, _fake_host) = crate::core::extension_runtime_set::tests::make_runner(json!({
            "handlers": [],
            "terminalInput": false
        }))
        .await?;
        let set = ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, runner)]);
        let session = make_session()?;
        let errors = Arc::new(Mutex::new(Vec::new()));
        let captured_errors = Arc::clone(&errors);
        session
            .bind_extensions(ExtensionBindings {
                on_error: Some(Arc::new(move |message| {
                    captured_errors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(message.to_owned());
                })),
                ..Default::default()
            })
            .await?;
        let weak = Arc::downgrade(&session);
        let binding = set.bind_session_target(weak.clone()).await;

        dispatch_session_bridge_route(
            &set,
            session_name_command("ignored", None),
            SessionBridgeRoute::Operation,
            &weak,
        )
        .await;
        dispatch_session_bridge_route(
            &set,
            session_name_command("still-running", None),
            SessionBridgeRoute::Active {
                target: Arc::clone(&session),
                binding,
            },
            &weak,
        )
        .await;

        assert_eq!(
            errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["non-replacement event unexpectedly routed as a bridge operation"]
        );
        assert_eq!(
            session.session_state_wire().await.session_name.as_deref(),
            Some("still-running")
        );
        set.shutdown_once().await;
        session.dispose().await;
        Ok(())
    }
}
