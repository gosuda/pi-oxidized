//! Extension runner seam and session-shared hook handle.
//!
//! `AgentSession` never depends on `pi-ext` directly. All extension interaction
//! goes through [`ExtensionRunner`]. [`NullExtensionRunner`] is the default so
//! the product layer compiles, unit-tests, and ships before the host exists.
//!
//! [`SessionHooks`] is captured by the `pi-agent` tool and next-turn closures at
//! Agent construction time. Reload swaps the runner under an `RwLock` without
//! reinstalling those closures. Sync locks are never held across `.await`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::core::resources::ResourceExtensionPaths;
use futures::future::BoxFuture;
use pi_agent::{
    AfterToolCallContext, AfterToolCallResult, AgentLoopError, AgentLoopTurnUpdate, AgentMessage,
    AgentTool, AgentToolResult, BeforeToolCallContext, BeforeToolCallResult,
    PrepareNextTurnContext,
};
use pi_ai::{AssistantMessageEvent, Model, ModelThinkingLevel, ToolCall, ToolResultContent};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::events::AgentSessionEvent;

/// Result of an extension `input` transform.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputTransformResult {
    /// When true the extension handled the input and `AgentSession` must not run.
    pub handled: bool,
    /// Replacement prompt text.
    pub text: Option<String>,
    /// Replacement image attachments (opaque JSON until image wiring lands).
    pub images: Option<Value>,
}

/// Optional system-prompt / custom-message injection from `before_agent_start`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeAgentStartResult {
    /// Custom messages to inject before the user prompt.
    pub messages: Vec<AgentMessage>,
    /// Per-turn system prompt override.
    pub system_prompt: Option<String>,
}

/// Result of a cancellable extension lifecycle event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CancelResult {
    /// When true the operation must abort.
    pub cancel: bool,
    /// Optional human-readable reason.
    pub reason: Option<String>,
}

/// Errors produced while dispatching extension hooks.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionRunnerError {
    /// Extension hook failed.
    #[error("extension error: {0}")]
    Failed(String),
    /// Extension host was invalidated after session replacement.
    #[error("extension context invalidated")]
    Invalidated,
}

/// Extension seam consumed by `AgentSession`.
///
/// Methods cover every hook this session layer calls. Implementations must be
/// cheap no-ops when no handlers are registered for a given event name.
pub trait ExtensionRunner: Send + Sync {
    /// Returns true when at least one handler is registered for `event`.
    fn has_handlers(&self, event: &str) -> bool;

    /// Emit a generic session lifecycle event (`agent_start`, turn_*, tool_*, etc.).
    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>>;

    /// Emit `message_end` and optionally return a replacement message.
    fn emit_message_end(
        &self,
        message: AgentMessage,
    ) -> BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>>;

    /// Emit `tool_call` (before execution). Returns an optional block result.
    fn emit_tool_call<'a>(
        &'a self,
        tool_name: &'a str,
        tool_call_id: &'a str,
        input: Map<String, Value>,
    ) -> BoxFuture<'a, Result<Option<BeforeToolCallResult>, ExtensionRunnerError>>;

    /// Emit `tool_result` (after execution). Returns an optional override.
    fn emit_tool_result<'a>(
        &'a self,
        tool_name: &'a str,
        tool_call_id: &'a str,
        input: Map<String, Value>,
        content: Vec<ToolResultContent>,
        details: Value,
        is_error: bool,
    ) -> BoxFuture<'a, Result<Option<AfterToolCallResult>, ExtensionRunnerError>>;

    /// Emit the `input` transform event.
    fn emit_input<'a>(
        &'a self,
        text: &'a str,
        images: Option<Value>,
        source: &'a str,
        streaming_behavior: Option<&'a str>,
    ) -> BoxFuture<'a, Result<InputTransformResult, ExtensionRunnerError>>;

    /// Emit `before_agent_start` and return optional message/system prompt injection.
    fn emit_before_agent_start<'a>(
        &'a self,
        prompt: &'a str,
        images: Option<Value>,
    ) -> BoxFuture<'a, Result<Option<BeforeAgentStartResult>, ExtensionRunnerError>>;

    /// Discover additional resource paths from extensions.
    fn emit_resources_discover<'a>(
        &'a self,
        cwd: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<ResourceExtensionPaths, ExtensionRunnerError>>;

    /// Registered slash-command names (extension source).
    fn get_registered_commands(&self) -> Vec<String>;

    /// Whether a slash command named `name` is registered.
    fn has_command(&self, name: &str) -> bool {
        self.get_registered_commands().iter().any(|c| c == name)
    }

    /// Execute an extension slash command by name.
    ///
    /// Returns `Ok(true)` when the command was found and dispatched,
    /// `Ok(false)` when no such command is registered. Errors during
    /// execution are reported via [`ExtensionRunner::emit_error`] and the
    /// return value stays `Ok(true)` (the command was still "handled").
    fn execute_command<'a>(
        &'a self,
        name: &'a str,
        args: &'a str,
    ) -> BoxFuture<'a, Result<bool, ExtensionRunnerError>>;

    /// Registered extension tools by name.
    fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>>;

    /// Flag values provided by extensions.
    fn get_flag_values(&self) -> HashMap<String, Value>;

    /// Mark the runner invalid after session replacement.
    fn invalidate(&self);

    /// Report an extension error to the host error listener.
    fn emit_error(&self, message: String);

    /// Shut down extension UI / handlers for this session.
    fn shutdown<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, Result<(), ExtensionRunnerError>>;
}

/// No-op extension runner used before `pi-ext` lands and in unit tests.
#[derive(Clone, Debug, Default)]
pub struct NullExtensionRunner;

impl ExtensionRunner for NullExtensionRunner {
    fn has_handlers(&self, _event: &str) -> bool {
        false
    }

    fn emit(
        &self,
        _event: AgentSessionEvent,
    ) -> BoxFuture<'_, Result<Option<CancelResult>, ExtensionRunnerError>> {
        Box::pin(async { Ok(None) })
    }

    fn emit_message_end(
        &self,
        _message: AgentMessage,
    ) -> BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>> {
        Box::pin(async { Ok(None) })
    }

    fn emit_tool_call(
        &self,
        _tool_name: &str,
        _tool_call_id: &str,
        _input: Map<String, Value>,
    ) -> BoxFuture<'_, Result<Option<BeforeToolCallResult>, ExtensionRunnerError>> {
        Box::pin(async { Ok(None) })
    }

    fn emit_tool_result(
        &self,
        _tool_name: &str,
        _tool_call_id: &str,
        _input: Map<String, Value>,
        _content: Vec<ToolResultContent>,
        _details: Value,
        _is_error: bool,
    ) -> BoxFuture<'_, Result<Option<AfterToolCallResult>, ExtensionRunnerError>> {
        Box::pin(async { Ok(None) })
    }

    fn emit_input(
        &self,
        _text: &str,
        _images: Option<Value>,
        _source: &str,
        _streaming_behavior: Option<&str>,
    ) -> BoxFuture<'_, Result<InputTransformResult, ExtensionRunnerError>> {
        Box::pin(async { Ok(InputTransformResult::default()) })
    }

    fn emit_before_agent_start(
        &self,
        _prompt: &str,
        _images: Option<Value>,
    ) -> BoxFuture<'_, Result<Option<BeforeAgentStartResult>, ExtensionRunnerError>> {
        Box::pin(async { Ok(None) })
    }

    fn emit_resources_discover(
        &self,
        _cwd: &str,
        _reason: &str,
    ) -> BoxFuture<'_, Result<ResourceExtensionPaths, ExtensionRunnerError>> {
        Box::pin(async { Ok(ResourceExtensionPaths::default()) })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        Vec::new()
    }

    fn execute_command(
        &self,
        _name: &str,
        _args: &str,
    ) -> BoxFuture<'_, Result<bool, ExtensionRunnerError>> {
        Box::pin(async { Ok(false) })
    }

    fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn AgentTool>> {
        HashMap::new()
    }

    fn get_flag_values(&self) -> HashMap<String, Value> {
        HashMap::new()
    }

    fn invalidate(&self) {}

    fn emit_error(&self, _message: String) {}

    fn shutdown(&self, _reason: &str) -> BoxFuture<'_, Result<(), ExtensionRunnerError>> {
        Box::pin(async { Ok(()) })
    }
}

/// System-prompt snapshot shared with the agent `prepare_next_turn` closure.
#[derive(Clone, Debug, Default)]
pub struct SystemPromptState {
    /// Base system prompt rebuilt from resources/tools.
    pub base: String,
    /// Per-turn override from `before_agent_start` (cleared after use by callers).
    pub override_prompt: Option<String>,
}

/// Shared handle captured by agent hook closures.
///
/// Lock order (never hold more than one at a time, never across `.await`):
/// 1. `runner` (`RwLock`)
/// 2. `system_prompt` (`RwLock`)
/// 3. `tools` (`RwLock`)
///
/// The agent event pump and public `AgentSession` methods coordinate through
/// [`super::AgentSessionInner`]; `SessionHooks` only exposes runner/prompt/tool
/// snapshots for the pi-agent hook closures.
#[derive(Clone)]
pub struct SessionHooks {
    runner: Arc<RwLock<Arc<dyn ExtensionRunner>>>,
    system_prompt: Arc<RwLock<SystemPromptState>>,
    tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
}

impl SessionHooks {
    /// Create hooks with the given extension runner.
    #[must_use]
    pub fn new(runner: Arc<dyn ExtensionRunner>) -> Self {
        Self {
            runner: Arc::new(RwLock::new(runner)),
            system_prompt: Arc::new(RwLock::new(SystemPromptState::default())),
            tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Create hooks with a null runner.
    #[must_use]
    pub fn null() -> Self {
        Self::new(Arc::new(NullExtensionRunner))
    }

    /// Swap the extension runner (reload path).
    pub fn set_runner(&self, runner: Arc<dyn ExtensionRunner>) {
        if let Ok(mut guard) = self.runner.write() {
            *guard = runner;
        }
    }

    /// Snapshot the current runner.
    #[must_use]
    pub fn runner(&self) -> Arc<dyn ExtensionRunner> {
        self.runner.read().map_or_else(
            |poisoned| Arc::clone(&*poisoned.into_inner()),
            |guard| Arc::clone(&*guard),
        )
    }

    /// Replace the base system prompt.
    pub fn set_base_system_prompt(&self, prompt: String) {
        if let Ok(mut guard) = self.system_prompt.write() {
            guard.base = prompt;
        }
    }

    /// Set or clear the per-turn system prompt override.
    pub fn set_system_prompt_override(&self, prompt: Option<String>) {
        if let Ok(mut guard) = self.system_prompt.write() {
            guard.override_prompt = prompt;
        }
    }

    /// Snapshot system-prompt state.
    #[must_use]
    pub fn system_prompt_snapshot(&self) -> SystemPromptState {
        self.system_prompt.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |guard| guard.clone(),
        )
    }

    /// Effective system prompt (`override` or `base`).
    #[must_use]
    pub fn effective_system_prompt(&self) -> String {
        let snap = self.system_prompt_snapshot();
        snap.override_prompt.unwrap_or(snap.base)
    }

    /// Replace the tools snapshot used by `prepare_next_turn`.
    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        if let Ok(mut guard) = self.tools.write() {
            *guard = tools;
        }
    }

    /// Clone the current tools snapshot.
    #[must_use]
    pub fn tools_snapshot(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |guard| guard.clone(),
        )
    }

    /// Build the `before_tool_call` closure for `AgentLoopConfig`.
    #[must_use]
    pub fn before_tool_call_hook(self: &Arc<Self>) -> pi_agent::BeforeToolCall {
        let hooks = Arc::clone(self);
        Arc::new(
            move |ctx: BeforeToolCallContext, cancel: CancellationToken| {
                let hooks = Arc::clone(&hooks);
                Box::pin(async move {
                    let runner = hooks.runner();
                    if !runner.has_handlers("tool_call") {
                        return Ok(None);
                    }
                    let tool_name = ctx.tool_call.name.clone();
                    let tool_call_id = ctx.tool_call.id.clone();
                    let hook_result = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ok(None),
                        result = runner.emit_tool_call(&tool_name, &tool_call_id, ctx.args) => result,
                    };
                    match hook_result {
                        Ok(result) => Ok(result),
                        Err(err) => Err(AgentLoopError::message(err.to_string())),
                    }
                })
            },
        )
    }

    /// Build the `after_tool_call` closure for `AgentLoopConfig`.
    #[must_use]
    pub fn after_tool_call_hook(self: &Arc<Self>) -> pi_agent::AfterToolCall {
        let hooks = Arc::clone(self);
        Arc::new(
            move |ctx: AfterToolCallContext, cancel: CancellationToken| {
                let hooks = Arc::clone(&hooks);
                Box::pin(async move {
                    let runner = hooks.runner();
                    if !runner.has_handlers("tool_result") {
                        return Ok(None);
                    }
                    let tool_name = ctx.tool_call.name.clone();
                    let tool_call_id = ctx.tool_call.id.clone();
                    let content = ctx.result.content.clone();
                    let details = ctx.result.details.clone();
                    let hook_result = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ok(None),
                        result = runner.emit_tool_result(
                            &tool_name,
                            &tool_call_id,
                            ctx.args,
                            content,
                            details,
                            ctx.is_error,
                        ) => result,
                    };
                    match hook_result {
                        Ok(result) => Ok(result),
                        Err(err) => Err(AgentLoopError::message(err.to_string())),
                    }
                })
            },
        )
    }

    /// Build the `prepare_next_turn` closure that refreshes system prompt + tools.
    #[must_use]
    pub fn prepare_next_turn_hook(self: &Arc<Self>) -> pi_agent::PrepareNextTurn {
        let hooks = Arc::clone(self);
        Arc::new(move |turn: PrepareNextTurnContext| {
            let hooks = Arc::clone(&hooks);
            Box::pin(async move {
                let system_prompt = hooks.effective_system_prompt();
                let tools = hooks.tools_snapshot();
                let mut context = turn.context;
                context.system_prompt = system_prompt;
                if !tools.is_empty() {
                    context.tools = tools;
                }
                Ok(Some(AgentLoopTurnUpdate {
                    context: Some(context),
                    model: None,
                    thinking_level: None,
                }))
            })
        })
    }
}

/// Helper: unused imports kept for sibling modules via re-exports.
#[allow(dead_code)]
fn _keep_types(
    _: ToolCall,
    _: AssistantMessageEvent,
    _: Model,
    _: ModelThinkingLevel,
    _: AgentToolResult,
) {
}
