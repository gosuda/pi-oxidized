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
//! - reload (emits `session_shutdown{reload}` on the old host, preserves flag
//!   values, restarts the host, re-emits `session_start{reload}` on the new
//!   host, then re-discovers resources)
//! - the replaced-session context handed to `withSession` after runtime swap
//! - extension error isolation (host errors never abort the session)
//!
//! Divergence from upstream: reload emission is not gated on recorded
//! bindings (`hasBindings` in `agent-session.ts`). All Rust modes bind, and
//! `emit` self-gates on handler presence, so the gate would only suppress
//! correct emissions.

use crate::core::extension_host::{HostExtensionRunner, SessionBridgeEvent};
use crate::core::messages::CustomMessageContent;
use crate::core::resources::{
    ResourceLoader, SlashCommandInfo, SlashCommandSource, SyntheticSourceInfoOptions,
    create_synthetic_source_info,
};
use pi_ai::{ImageContent, Model, ModelThinkingLevel};
use pi_ext::protocol::{SessionCommand, SessionCommandInfoWire, SessionStateWire, SessionToolWire};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use super::prompt::{CustomMessageInput, DeliverAs};
use super::tools::RefreshToolRegistryOptions;

use super::AgentSession;
use super::events::{
    AgentSessionEvent, SessionShutdownReason, SessionStartEvent, SessionStartReason,
};

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
    /// 1. Capture previous flag values (preserved across the swap).
    /// 2. Emit `session_shutdown{reload}` on the old runner (self-gated on
    ///    handler presence; host errors isolated).
    /// 3. When a concrete host is present: sequential restart-and-rewire
    ///    (await old transport reap exactly once, re-register providers,
    ///    restore flags, swap runner, refresh tools).
    /// 4. Emit `session_start{reload}` on the post-swap runner.
    /// 5. Reload base resources and re-discover extension resources.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionBindError`] on host restart or resource-discovery
    /// failure.
    pub async fn reload(&self) -> Result<(), ExtensionBindError> {
        let runner = self.hooks.runner();
        let previous_flag_values = runner.get_flag_values();

        // Lifecycle event on the old host. Emit self-gates on handler
        // presence; host transport reaping is handled below regardless.
        let _ = runner
            .emit(AgentSessionEvent::SessionShutdown {
                reason: SessionShutdownReason::Reload,
                target_session_file: None,
            })
            .await;

        if let Some(host) = self.host_extension_runner() {
            let Some(runtime) = self.model_runtime() else {
                // No runtime to re-register providers against: still reap the
                // old host so dispose paths stay single-reap clean.
                host.shutdown_once().await;
                self.set_host_extension_runner(None);
                self.hooks
                    .set_runner(Arc::new(super::extension_runner::NullExtensionRunner));
                self.emit_session_start_reload().await;
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
            // Re-claim the fresh host's session-action bridge.
            self.bind_session_bridge().await;
            self.emit_session_start_reload().await;
            self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
                .await?;
            return Ok(());
        }

        // Trait-only / test path (no concrete host).
        self.emit_session_start_reload().await;
        self.extend_resources_from_extensions(SessionStartReason::Reload.as_str())
            .await?;
        Ok(())
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
                // Upstream commands carry the loading extension's SourceInfo:
                // `inline` for built-in inline factories, `cli` for
                // `--extension`-loaded paths. The host snapshot only carries
                // the extension path, so derive the source from its shape.
                let source = if path.starts_with("<inline:") {
                    "inline"
                } else {
                    "cli"
                };
                commands.push(SlashCommandInfo {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    source: SlashCommandSource::Extension,
                    source_info: create_synthetic_source_info(
                        path,
                        SyntheticSourceInfoOptions {
                            source: source.to_owned(),
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
        let Some(mut bridge_rx) = host.take_session_bridge() else {
            return;
        };
        let Some(weak) = self.upgrade_self() else {
            return;
        };
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
        // Awaited: the mirror must be warm before session_start is emitted.
        let state = self.session_state_wire().await;
        host.push_session_state(&state).await;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    item = bridge_rx.recv() => {
                        let Some(item) = item else { break };
                        let Some(session) = weak.upgrade() else { break };
                        session.apply_session_bridge_event(&host, item).await;
                        let state = session.session_state_wire().await;
                        host.push_session_state(&state).await;
                    }
                    ping = dirty_rx.recv() => {
                        if ping.is_none() {
                            break;
                        }
                        // Coalesce bursts into one push.
                        while dirty_rx.try_recv().is_ok() {}
                        if !host.is_active() {
                            break;
                        }
                        let Some(session) = weak.upgrade() else { break };
                        let state = session.session_state_wire().await;
                        host.push_session_state(&state).await;
                    }
                }
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
        host: &Arc<HostExtensionRunner>,
        event: SessionBridgeEvent,
    ) {
        match event {
            SessionBridgeEvent::Command(command) => self.apply_session_command(command).await,
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
                // Compaction can be slow; run it off the bridge loop so other
                // commands (abort included) stay responsive.
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
}
