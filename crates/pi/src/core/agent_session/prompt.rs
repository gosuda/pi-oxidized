//! Prompt lifecycle: preflight, steer/follow-up queues, run loop, and post-run
//! continuation (retry → compaction → queued messages).
//!
//! [`AgentSession::prompt`] runs the preflight then calls
//! [`AgentSession::run_agent_prompt`] which loops: `agent.prompt(messages)`,
//! then `while handle_post_agent_run` (retry → compaction → queued messages)
//! triggers another `agent.continue_run()`. `emit_agent_settled` fires
//! exactly once in the finally block.

use std::sync::Arc;

use pi_agent::{AgentMessage, user_text};
use pi_ai::{AssistantMessage, ImageContent};

use super::events::AgentSessionEvent;
use super::{AgentSession, BeforeAgentStartResult};
use crate::core::agent_session_services::{
    format_no_api_key_found_message, format_no_model_selected_message,
    format_oauth_auth_failed_message,
};
use crate::core::messages::CustomMessageContent;
use crate::core::model_runtime::ModelRuntime;
use crate::core::resources::frontmatter::strip_frontmatter;
use crate::core::resources::prompts::expand_prompt_template;

/// Upper bound on waiting for a failed run's already-queued `agent_end`.
///
/// Ordinary failed runs never wait this long: their `agent_end` is queued
/// before the run call returns, so the barrier resolves on the next pump
/// iteration. This bound only fires for a future pre-start rejection that
/// [`run_emits_agent_end`] does not classify, keeping worst-case added
/// latency well under the interactive p95 budget.
const FAILED_RUN_END_BARRIER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// How a streaming prompt is queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamingBehavior {
    /// Inject before the next LLM call (steering queue).
    Steer,
    /// Inject after the current run finishes (follow-up queue).
    FollowUp,
}

impl StreamingBehavior {
    /// Wire string matching TypeScript (`"steer"` / `"followUp"`).
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

/// Callback invoked once during `prompt()` preflight to signal accept/reject.
///
/// `true` is called when the prompt will be run (or queued), `false` on a
/// synchronous error. RPC consumers use this to emit exactly one response
/// per `prompt()` call.
pub type PreflightCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// Configuration for one [`AgentSession::prompt`] invocation.
#[derive(Clone)]
pub struct PromptOptions {
    /// Image attachments for the user message.
    pub images: Vec<ImageContent>,
    /// When the session is already streaming, routes the message to the
    /// steering or follow-up queue. Required while streaming.
    pub streaming_behavior: Option<StreamingBehavior>,
    /// Origin label forwarded to extension `input` events.
    pub source: Option<String>,
    /// Expand extension commands, `/skill:name`, and prompt templates.
    pub expand_prompt_templates: bool,
    /// Optional single-shot accept signal (see [`PreflightCallback`]).
    pub preflight_result: Option<PreflightCallback>,
}

impl Default for PromptOptions {
    fn default() -> Self {
        Self {
            images: Vec::new(),
            streaming_behavior: None,
            source: None,
            expand_prompt_templates: true,
            preflight_result: None,
        }
    }
}

impl PromptOptions {
    /// Build defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Delivery mode for [`AgentSession::send_custom_message`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliverAs {
    /// Steering queue (injected before the next LLM call).
    Steer,
    /// Follow-up queue (injected after the current run).
    FollowUp,
    /// Buffered; appended alongside the next user prompt.
    NextTurn,
}

/// Input for [`AgentSession::send_custom_message`].
#[derive(Clone, Debug)]
pub struct CustomMessageInput {
    /// Extension-defined discriminant.
    pub custom_type: String,
    /// User-visible / LLM content.
    pub content: CustomMessageContent,
    /// Whether the interactive UI should render this message.
    pub display: bool,
    /// Opaque details preserved on the transcript entry.
    pub details: Option<serde_json::Value>,
}

/// Errors produced by the prompt lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// Human-readable error (no model / no auth / queue / extension command).
    #[error("{0}")]
    Message(String),
    /// Underlying agent run failure.
    #[error(transparent)]
    Agent(#[from] pi_agent::AgentLoopError),
    /// Session persistence failure while preparing or settling the prompt.
    #[error(transparent)]
    Session(#[from] crate::core::sessions::SessionError),
}

impl PromptError {
    #[must_use]
    fn msg(s: impl Into<String>) -> Self {
        Self::Message(s.into())
    }
}

fn map_bash_flush_error(err: super::bash::BashExecError) -> PromptError {
    match err {
        super::bash::BashExecError::Session(err) => PromptError::Session(err),
        super::bash::BashExecError::Execution { message, .. } => PromptError::Message(message),
    }
}

enum PreflightOutcome {
    Run(Vec<AgentMessage>),
    Handled,
    Queued,
}

impl AgentSession {
    /// Send a prompt to the agent. Handles extension commands, input transforms,
    /// skill/template expansion, streaming-queue routing, model + auth validation,
    /// pre-prompt compaction, `before_agent_start` injection, and the run loop.
    ///
    /// # Errors
    ///
    /// - [`PromptError::Message`] for no-model, no-auth, concurrent-streaming
    ///   guard, or queue slash-command rejection.
    /// - [`PromptError::Agent`] when the underlying agent run fails.
    /// - [`PromptError::Session`] when transcript persistence fails.
    pub async fn prompt(
        self: &Arc<Self>,
        text: &str,
        options: PromptOptions,
    ) -> Result<(), PromptError> {
        self.prompt_inner(text, options).await
    }

    async fn prompt_inner(
        self: &Arc<Self>,
        text: &str,
        mut options: PromptOptions,
    ) -> Result<(), PromptError> {
        let expand = options.expand_prompt_templates;
        let preflight = options.preflight_result.take();
        let call_preflight = |ok: bool| {
            if let Some(cb) = &preflight {
                cb(ok);
            }
        };

        match self.prompt_preflight(text, &mut options, expand).await {
            Ok(PreflightOutcome::Run(messages)) => {
                call_preflight(true);
                self.run_agent_prompt(messages).await?;
                Ok(())
            }
            Ok(PreflightOutcome::Handled | PreflightOutcome::Queued) => {
                call_preflight(true);
                Ok(())
            }
            Err(err) => {
                call_preflight(false);
                Err(err)
            }
        }
    }

    /// Queue a steering message; errors when `text` is a registered extension
    /// command (commands cannot be queued).
    ///
    /// # Errors
    ///
    /// [`PromptError::Message`] for extension-command rejection.
    pub fn steer(&self, text: &str, images: Vec<ImageContent>) -> Result<(), PromptError> {
        self.check_not_extension_command(text)?;
        let expanded = self.expand_text(text);
        self.queue_steer(&expanded, images);
        Ok(())
    }

    /// Queue a follow-up message; errors when `text` is a registered extension
    /// command.
    ///
    /// # Errors
    ///
    /// [`PromptError::Message`] for extension-command rejection.
    pub fn follow_up(&self, text: &str, images: Vec<ImageContent>) -> Result<(), PromptError> {
        self.check_not_extension_command(text)?;
        let expanded = self.expand_text(text);
        self.queue_follow_up(&expanded, images);
        Ok(())
    }

    /// Send a custom message (extension-injected transcript entry). Delivery is
    /// selected by `deliver_as` and current streaming state.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError`] when starting a requested agent turn fails or
    /// when the idle-path durable session append fails (no live state or
    /// public event is published in that case).
    pub async fn send_custom_message(
        self: &Arc<Self>,
        message: CustomMessageInput,
        trigger_turn: bool,
        deliver_as: Option<DeliverAs>,
    ) -> Result<(), PromptError> {
        let app_message = build_custom_agent_message(&message);

        match deliver_as {
            Some(DeliverAs::NextTurn) => {
                self.lock_inner()
                    .pending_next_turn_messages
                    .push(app_message);
            }
            _ if self.is_session_streaming() => match deliver_as {
                Some(DeliverAs::FollowUp) => self.agent.follow_up(app_message),
                _ => self.agent.steer(app_message),
            },
            _ if trigger_turn => {
                self.run_agent_prompt(vec![app_message]).await?;
            }
            _ => {
                // Durable append first: live agent state and public events are
                // published only for entries the session file actually holds.
                {
                    let mut sm = self.session_manager.lock().await;
                    sm.append_custom_message_entry(
                        &message.custom_type,
                        &message.content,
                        message.display,
                        message.details.clone(),
                    )
                    .map_err(PromptError::Session)?;
                }
                self.agent.push_message(app_message.clone());
                self.emit_public(AgentSessionEvent::MessageStart {
                    message: app_message.clone(),
                });
                self.emit_public(AgentSessionEvent::MessageEnd {
                    message: app_message,
                });
            }
        }
        Ok(())
    }

    /// Send a user message. While idle this triggers a new turn; while
    /// streaming it queues per `deliver_as`.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError`] when prompt validation, extension handling, or
    /// the underlying agent run fails.
    pub async fn send_user_message(
        self: &Arc<Self>,
        text: &str,
        images: Vec<ImageContent>,
        deliver_as: Option<DeliverAs>,
    ) -> Result<(), PromptError> {
        let streaming_behavior = deliver_as.map(|d| match d {
            DeliverAs::Steer | DeliverAs::NextTurn => StreamingBehavior::Steer,
            DeliverAs::FollowUp => StreamingBehavior::FollowUp,
        });
        self.prompt(
            text,
            PromptOptions {
                images,
                streaming_behavior,
                source: Some("extension".to_owned()),
                expand_prompt_templates: false,
                preflight_result: None,
            },
        )
        .await
    }

    // -----------------------------------------------------------------
    // Preflight
    // -----------------------------------------------------------------

    async fn prompt_preflight(
        self: &Arc<Self>,
        text: &str,
        options: &mut PromptOptions,
        expand: bool,
    ) -> Result<PreflightOutcome, PromptError> {
        // 1. Extension command dispatch.
        if expand && text.starts_with('/') && self.try_execute_extension_command(text).await? {
            return Ok(PreflightOutcome::Handled);
        }

        // 2. Input event transform.
        let current_images = std::mem::take(&mut options.images);
        let (current_text, current_images, handled) = self
            .transform_input(text.to_owned(), current_images, options)
            .await?;
        if handled {
            options.images = current_images;
            return Ok(PreflightOutcome::Handled);
        }

        // 3. Skill / template expansion.
        let expanded_text = if expand {
            self.expand_text(&current_text)
        } else {
            current_text
        };

        // 4. Streaming: queue via steer/followUp.
        if self.is_session_streaming() {
            let behavior = options.streaming_behavior.ok_or_else(|| {
                PromptError::msg(
                    "Agent is already processing. Specify streamingBehavior \
                     ('steer' or 'followUp') to queue the message.",
                )
            })?;
            match behavior {
                StreamingBehavior::FollowUp => {
                    self.queue_follow_up(&expanded_text, current_images);
                }
                StreamingBehavior::Steer => {
                    self.queue_steer(&expanded_text, current_images);
                }
            }
            return Ok(PreflightOutcome::Queued);
        }

        // 5. Flush any pending bash messages before validation.
        self.flush_pending_bash_messages()
            .await
            .map_err(map_bash_flush_error)?;

        // 6. Validate model.
        let model = self.model();
        if is_no_model(&model) {
            return Err(PromptError::Message(format_no_model_selected_message()));
        }

        // 7. Validate auth.
        if let Some(runtime) = self.try_model_runtime() {
            let provider = &model.provider;
            let has_auth = runtime.has_configured_auth(provider)
                || runtime.check_auth(provider).await.is_some();
            if !has_auth {
                if runtime.is_using_oauth(provider) {
                    return Err(PromptError::Message(format_oauth_auth_failed_message(
                        provider,
                    )));
                }
                return Err(PromptError::Message(format_no_api_key_found_message(
                    provider,
                )));
            }
        }

        // 8. Pre-prompt compaction check (no agent.continue — sibling compaction
        //    slice owns this; here we trigger only the pre-prompt pass).
        if let Some(last_msg) = self.agent.last_assistant() {
            self.check_compaction(&last_msg, false).await;
        }

        // 9. Build messages: user message + pending nextTurn.
        let mut messages = Vec::new();
        messages.push(user_text(&expanded_text, current_images.iter().cloned()));
        {
            let mut inner = self.lock_inner();
            for msg in inner.pending_next_turn_messages.drain(..) {
                messages.push(msg);
            }
        }

        // 10. before_agent_start extension event.
        let runner = self.hooks.runner();
        let images_for_ext = if current_images.is_empty() {
            None
        } else {
            serde_json::to_value(&current_images).ok()
        };
        let result = runner
            .emit_before_agent_start(&expanded_text, images_for_ext)
            .await
            .map_err(|e| PromptError::msg(e.to_string()))?;
        drop(runner);

        self.apply_before_agent_start(result);

        Ok(PreflightOutcome::Run(messages))
    }

    async fn transform_input(
        &self,
        mut text: String,
        mut images: Vec<ImageContent>,
        options: &PromptOptions,
    ) -> Result<(String, Vec<ImageContent>, bool), PromptError> {
        let runner = self.hooks.runner();
        if !runner.has_handlers("input") {
            return Ok((text, images, false));
        }

        let streaming = if self.is_session_streaming() {
            options.streaming_behavior.map(|behavior| behavior.as_str())
        } else {
            None
        };
        let source = options.source.as_deref().unwrap_or("interactive");
        let images_value = if images.is_empty() {
            None
        } else {
            serde_json::to_value(&images).ok()
        };
        let result = runner
            .emit_input(&text, images_value, source, streaming)
            .await
            .map_err(|error| PromptError::msg(error.to_string()))?;

        if result.handled {
            return Ok((text, images, true));
        }

        if let Some(transformed_text) = result.text {
            text = transformed_text;
        }
        if let Some(transformed_images) = result.images
            && let Some(parsed) = parse_images_value(&transformed_images)
        {
            images = parsed;
        }

        Ok((text, images, false))
    }

    fn apply_before_agent_start(&self, result: Option<BeforeAgentStartResult>) {
        let Some(result) = result else {
            self.hooks.set_system_prompt_override(None);
            let base = self.lock_inner().base_system_prompt.clone();
            self.agent.set_system_prompt(base);
            return;
        };

        if !result.messages.is_empty() {
            let mut inner = self.lock_inner();
            inner
                .pending_next_turn_messages
                .splice(0..0, result.messages);
        }

        if let Some(system_prompt) = result.system_prompt {
            self.hooks
                .set_system_prompt_override(Some(system_prompt.clone()));
            self.agent.set_system_prompt(system_prompt);
        } else {
            self.hooks.set_system_prompt_override(None);
            let base = self.lock_inner().base_system_prompt.clone();
            self.agent.set_system_prompt(base);
        }
    }

    // -----------------------------------------------------------------
    // Run loop
    // -----------------------------------------------------------------

    async fn run_agent_prompt(
        self: &Arc<Self>,
        messages: Vec<AgentMessage>,
    ) -> Result<(), PromptError> {
        self.mark_agent_run_active();
        let result = self.run_agent_prompt_inner(messages).await;
        // Flush any bash messages that arrived during the run before settle.
        let flush_result = self
            .flush_pending_bash_messages()
            .await
            .map_err(map_bash_flush_error);
        self.hooks.set_system_prompt_override(None);
        self.emit_agent_settled().await;
        let pending_session_error = self.take_session_error();
        if let Some(error) = pending_session_error {
            return Err(PromptError::Session(error));
        }
        flush_result?;
        result
    }

    async fn run_agent_prompt_inner(
        self: &Arc<Self>,
        messages: Vec<AgentMessage>,
    ) -> Result<(), PromptError> {
        let mut messages = messages;
        {
            let mut inner = self.lock_inner();
            if !inner.pending_next_turn_messages.is_empty() {
                let pending: Vec<_> = inner.pending_next_turn_messages.drain(..).collect();
                messages.extend(pending);
            }
        }

        // Capture assistant count BEFORE the run so the first response is seen
        // as new and triggers retry/compaction/queued-message continuation.
        // After prepare_retry / overflow compaction pop the trailing assistant,
        // the count drops; re-baseline before continue_run so the next terminal
        // assistant is observed (TS tracks this via `_lastAssistantMessage`).
        let mut processed_count = self.assistant_count();
        let mut processed_agent_ends = self.processed_agent_end_count();
        let run = self.agent.prompt(messages).await;
        self.observe_run_agent_end(&run, processed_agent_ends)
            .await?;
        if let Some(error) = self.take_session_error() {
            return Err(PromptError::Session(error));
        }
        run?;
        processed_agent_ends = self.processed_agent_end_count();
        loop {
            let current_count = self.assistant_count();
            let new_assistant = if current_count > processed_count {
                self.agent.last_assistant()
            } else {
                None
            };
            if !self.handle_post_agent_run(new_assistant).await? {
                break;
            }
            // Re-baseline after pops from prepare_retry / overflow compaction.
            processed_count = self.assistant_count();
            let run = self.agent.continue_run().await;
            self.observe_run_agent_end(&run, processed_agent_ends)
                .await?;
            if let Some(error) = self.take_session_error() {
                return Err(PromptError::Session(error));
            }
            run?;
            processed_agent_ends = self.processed_agent_end_count();
        }
        Ok(())
    }

    /// Await the processed `agent_end` barrier for one run outcome.
    ///
    /// Successful runs require the barrier: a pump disconnect before the end
    /// event is a hard prompt error. Failed runs that started a lifecycle have
    /// already queued their synthesized `agent_end` before returning, so the
    /// barrier resolves immediately; the bounded timeout guards against a
    /// pre-start rejection this classifier does not know about, so a
    /// misclassification can never hang the prompt lifecycle.
    async fn observe_run_agent_end(
        &self,
        run: &Result<(), pi_agent::AgentLoopError>,
        processed_agent_ends: u64,
    ) -> Result<(), PromptError> {
        if run.is_ok() {
            if !self
                .wait_for_processed_agent_end(processed_agent_ends)
                .await
            {
                return Err(PromptError::msg(
                    "Agent event pump disconnected before agent_end",
                ));
            }
            return Ok(());
        }
        if run_emits_agent_end(run) {
            let _ = tokio::time::timeout(
                FAILED_RUN_END_BARRIER_TIMEOUT,
                self.wait_for_processed_agent_end(processed_agent_ends),
            )
            .await;
        }
        Ok(())
    }

    /// Post-run check: retry → compaction → queued messages.
    ///
    /// Returns `true` when the caller should `continue_run`.
    async fn handle_post_agent_run(
        self: &Arc<Self>,
        msg: Option<AssistantMessage>,
    ) -> Result<bool, PromptError> {
        let Some(msg) = msg else {
            return Ok(false);
        };

        if Self::is_retryable_error(&msg) && self.prepare_retry(&msg).await {
            return Ok(true);
        }

        self.emit_retry_exhausted(&msg);

        // Compaction check after retry handling. skip_aborted_check is `true`
        // here because we only run after a real agent message_end, not an
        // aborted user prompt.
        if self.check_compaction(&msg, true).await {
            return Ok(true);
        }

        Ok(self.agent.has_queued_messages())
    }

    // -----------------------------------------------------------------
    // Queue helpers
    // -----------------------------------------------------------------

    fn queue_steer(&self, text: &str, images: Vec<ImageContent>) {
        self.mirror_steering_push(text.to_owned());
        self.agent.steer(user_text(text, images));
    }

    fn queue_follow_up(&self, text: &str, images: Vec<ImageContent>) {
        self.mirror_follow_up_push(text.to_owned());
        self.agent.follow_up(user_text(text, images));
    }

    // -----------------------------------------------------------------
    // Extension commands + skill/template expansion
    // -----------------------------------------------------------------

    async fn try_execute_extension_command(&self, text: &str) -> Result<bool, PromptError> {
        let Some((name, args)) = parse_slash_command(text) else {
            return Ok(false);
        };
        let runner = self.hooks.runner();
        if !runner.has_command(name) {
            return Ok(false);
        }
        match runner.execute_command(name, args).await {
            Ok(handled) => {
                if !handled {
                    return Ok(false);
                }
            }
            Err(err) => {
                runner.emit_error(format!("command:{name}: {err}"));
            }
        }
        Ok(true)
    }

    fn check_not_extension_command(&self, text: &str) -> Result<(), PromptError> {
        if !text.starts_with('/') {
            return Ok(());
        }
        if let Some((name, _)) = parse_slash_command(text) {
            let runner = self.hooks.runner();
            if runner.has_command(name) {
                return Err(PromptError::msg(format!(
                    "Extension command \"/{name}\" cannot be queued. Use prompt() \
                     or execute the command when not streaming."
                )));
            }
        }
        Ok(())
    }

    fn expand_text(&self, text: &str) -> String {
        let expanded = self.expand_skill_command(text);
        let templates = self
            .prompt_templates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        expand_prompt_template(&expanded, &templates)
    }

    fn expand_skill_command(&self, text: &str) -> String {
        if !text.starts_with("/skill:") {
            return text.to_owned();
        }
        let rest = &text["/skill:".len()..];
        let (skill_name, args) = match rest.find(' ') {
            Some(idx) => (&rest[..idx], rest[idx + 1..].trim()),
            None => (rest, ""),
        };

        let skills = self
            .skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(skill) = skills.iter().find(|s| s.name == skill_name) else {
            return text.to_owned();
        };

        match std::fs::read_to_string(&skill.file_path) {
            Ok(content) => {
                let body = strip_frontmatter(&content)
                    .unwrap_or_else(|_| content.clone())
                    .trim()
                    .to_owned();
                let block = format!(
                    "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                    skill.name, skill.file_path, skill.base_dir, body
                );
                if args.is_empty() {
                    block
                } else {
                    format!("{block}\n\n{args}")
                }
            }
            Err(_) => text.to_owned(),
        }
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn is_session_streaming(&self) -> bool {
        self.lock_inner().is_agent_run_active
    }

    fn assistant_count(&self) -> usize {
        self.agent
            .transcript()
            .iter()
            .filter(|m| m.role() == "assistant")
            .count()
    }

    fn try_model_runtime(&self) -> Option<ModelRuntime> {
        self.model_runtime.as_deref().cloned()
    }
}

// -----------------------------------------------------------------------
// Free helpers
// -----------------------------------------------------------------------

fn parse_slash_command(text: &str) -> Option<(&str, &str)> {
    if !text.starts_with('/') {
        return None;
    }
    let body = &text[1..];
    match body.find(' ') {
        Some(idx) => Some((&body[..idx], &body[idx + 1..])),
        None => Some((body, "")),
    }
}

/// Whether this run outcome is followed by an `agent_end` agent event.
///
/// `Agent::prompt` / `Agent::continue_run` emit `agent_end` for every run that
/// actually starts — including failed runs, whose terminal sequence is
/// synthesized by the agent before the error returns. Only the pre-start
/// rejections below return without emitting anything; awaiting the processed
/// `agent_end` barrier for them would hang forever.
fn run_emits_agent_end(run: &Result<(), pi_agent::AgentLoopError>) -> bool {
    match run {
        Ok(()) => true,
        Err(pi_agent::AgentLoopError::Message(message)) => {
            message != "agent is already running"
                && message != "No messages to continue from"
                && !message.starts_with("Cannot continue from message role")
        }
    }
}

fn is_no_model(model: &pi_ai::Model) -> bool {
    model.provider == "unknown"
}

fn parse_images_value(value: &serde_json::Value) -> Option<Vec<ImageContent>> {
    serde_json::from_value::<Vec<ImageContent>>(value.clone()).ok()
}

fn build_custom_agent_message(message: &CustomMessageInput) -> AgentMessage {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "customType".to_owned(),
        serde_json::Value::String(message.custom_type.clone()),
    );
    payload.insert(
        "content".to_owned(),
        serde_json::to_value(&message.content).unwrap_or(serde_json::Value::Null),
    );
    payload.insert(
        "display".to_owned(),
        serde_json::Value::Bool(message.display),
    );
    if let Some(details) = &message.details {
        payload.insert("details".to_owned(), details.clone());
    }
    payload.insert(
        "timestamp".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(pi_agent::now_millis())),
    );
    AgentMessage::Custom(pi_agent::CustomAgentMessage::new("custom", payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_session::{
        AgentSessionConfig, AgentSessionEvent, ExtensionRunner, ExtensionRunnerError,
        NullExtensionRunner,
    };
    use futures::future::BoxFuture;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantContent, AssistantMessageEvent, Context, DoneReason, ErrorReason, ModelCost,
        ModelInput, Provider, ProviderError, StopReason, StreamOptions, TextContent,
    };
    use std::collections::HashMap;
    use std::fmt::Display;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{MutexGuard, PoisonError};
    use tokio::sync::{Notify, Semaphore};

    type ProviderEventResult = Result<AssistantMessageEvent, ProviderError>;
    type ProviderResponse = Vec<ProviderEventResult>;
    type ProviderResponses = Vec<ProviderResponse>;
    type TestResult<T = ()> = Result<T, String>;

    trait TestContext<T> {
        fn test_context(self, context: &str) -> TestResult<T>;
    }

    impl<T, E: Display> TestContext<T> for Result<T, E> {
        fn test_context(self, context: &str) -> TestResult<T> {
            self.map_err(|error| format!("{context}: {error}"))
        }
    }

    fn mutex_value<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn require_error<T, E>(result: Result<T, E>, context: &str) -> TestResult<E> {
        match result {
            Ok(_) => Err(format!("{context}: expected an error")),
            Err(error) => Ok(error),
        }
    }

    fn require_some<T>(value: Option<T>, context: &str) -> TestResult<T> {
        value.ok_or_else(|| format!("{context}: expected a value"))
    }

    fn test_model() -> pi_ai::Model {
        pi_ai::Model {
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

    fn assistant_text(text: &str) -> AssistantMessage {
        let mut message =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        message
            .content
            .push(AssistantContent::Text(TextContent::new(text)));
        message.stop_reason = StopReason::Stop;
        message
    }

    fn assistant_error(err: &str) -> AssistantMessage {
        let mut message =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        message.stop_reason = StopReason::Error;
        message.error_message = Some(err.to_owned());
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

    fn done_ok(msg: AssistantMessage) -> AssistantMessageEvent {
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: msg,
        }
    }

    fn done_err(msg: AssistantMessage) -> AssistantMessageEvent {
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: msg,
        }
    }

    #[derive(Clone)]
    struct SeqProvider {
        calls: Arc<AtomicUsize>,
        responses: Arc<StdMutex<ProviderResponses>>,
    }

    impl SeqProvider {
        fn new(responses: ProviderResponses) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                responses: Arc::new(StdMutex::new(responses)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Provider for SeqProvider {
        fn stream(
            &self,
            _model: &pi_ai::Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, ProviderEventResult> {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = mutex_value(&self.responses)
                .get(idx)
                .cloned()
                .unwrap_or_default();
            stream::iter(events).boxed()
        }
    }

    fn make_session(provider: Arc<dyn Provider>) -> TestResult<Arc<AgentSession>> {
        let config = AgentSessionConfig::test_config(provider, test_model())
            .test_context("test session config")?;
        AgentSession::new(config).test_context("test session creation")
    }

    /// Wait until the session-level run is idle (no wall-clock sleep).
    async fn drain(session: &Arc<AgentSession>) {
        session.wait_for_idle().await;
    }

    /// Wrap an event into `Result<_, ProviderError>` for `Vec<Vec<_>>`.
    fn ok_event(e: AssistantMessageEvent) -> AssistantMessageEvent {
        e
    }

    /// Build a response sequence where every event is wrapped in `Result::Ok`.
    fn sequence(events: Vec<AssistantMessageEvent>) -> ProviderResponse {
        events.into_iter().map(Ok).collect()
    }

    /// Single-event sequence.
    fn one(e: AssistantMessageEvent) -> ProviderResponses {
        vec![sequence(vec![e])]
    }

    /// Two-event sequence.
    fn two(a: AssistantMessageEvent, b: AssistantMessageEvent) -> ProviderResponses {
        vec![sequence(vec![a, b])]
    }

    /// Two-call sequence (split across provider.stream calls).
    fn split(
        first: Vec<AssistantMessageEvent>,
        second: Vec<AssistantMessageEvent>,
    ) -> ProviderResponses {
        vec![sequence(first), sequence(second)]
    }

    #[tokio::test]
    async fn single_prompt_records_messages() -> TestResult {
        let provider = Arc::new(SeqProvider::new(split(
            vec![
                ok_event(start_event()),
                ok_event(done_ok(assistant_text("hello"))),
            ],
            vec![],
        )));
        let session = make_session(provider)?;
        session
            .prompt("hi", PromptOptions::default())
            .await
            .test_context("single prompt")?;
        drain(&session).await;
        let messages = session.messages();
        let roles: Vec<&str> = messages.iter().map(AgentMessage::role).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_prompt_without_behavior_errors() -> TestResult {
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let session = make_session(provider)?;
        session.mark_agent_run_active();
        let result = session.prompt("second", PromptOptions::default()).await;
        let err = require_error(result, "concurrent prompt")?;
        assert!(
            err.to_string().contains("Agent is already processing"),
            "{err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_model_error() -> TestResult {
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.model = None;
        let session = AgentSession::new(config).test_context("session")?;
        let result = session.prompt("hi", PromptOptions::default()).await;
        let err = require_error(result, "no-model prompt")?;
        assert!(err.to_string().contains("No model selected"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn retry_transient_then_success() -> TestResult {
        let provider = Arc::new(SeqProvider::new(split(
            vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ],
            vec![
                ok_event(start_event()),
                ok_event(done_ok(assistant_text("recovered"))),
            ],
        )));
        let session = make_session(provider.clone())?;
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let settled = Arc::new(AtomicUsize::new(0));
        let ev = events.clone();
        let s = settled.clone();
        let _u = session.subscribe(move |event| match event {
            AgentSessionEvent::AutoRetryStart { attempt, .. } => {
                mutex_value(&ev).push(format!("start:{attempt}"));
            }
            AgentSessionEvent::AutoRetryEnd { success, .. } => {
                mutex_value(&ev).push(format!("end:{success}"));
            }
            AgentSessionEvent::AgentSettled => {
                s.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let ev = mutex_value(&events).clone();
        assert_eq!(
            provider.call_count(),
            2,
            "one failure + one recovery stream"
        );
        assert_eq!(
            ev,
            vec!["start:1".to_owned(), "end:true".to_owned()],
            "retry lifecycle order: {ev:?}"
        );
        assert_eq!(settled.load(Ordering::SeqCst), 1);
        assert_eq!(session.retry_attempt(), 0);
        assert!(!session.agent.state().is_streaming);
        Ok(())
    }

    #[tokio::test]
    async fn retry_disabled_no_retry() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_err(assistant_error("overloaded_error")),
        )));
        let session = make_session(provider.clone())?;
        session.set_auto_retry_enabled(false);
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let settled = Arc::new(AtomicUsize::new(0));
        let ev = events.clone();
        let s = settled.clone();
        let _u = session.subscribe(move |e| match e {
            AgentSessionEvent::AutoRetryStart { .. } => {
                mutex_value(&ev).push("start".to_owned());
            }
            AgentSessionEvent::AutoRetryEnd { .. } => {
                mutex_value(&ev).push("end".to_owned());
            }
            AgentSessionEvent::AgentSettled => {
                s.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let ev = mutex_value(&events).clone();
        assert!(
            ev.is_empty(),
            "disabled retry must not emit auto_retry events: {ev:?}"
        );
        assert_eq!(
            provider.call_count(),
            1,
            "disabled retry must not re-invoke the provider"
        );
        assert_eq!(settled.load(Ordering::SeqCst), 1);
        assert_eq!(session.retry_attempt(), 0);
        assert!(!session.auto_retry_enabled());
        Ok(())
    }

    #[tokio::test]
    async fn non_retryable_error_no_retry() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_err(assistant_error("invalid_api_key")),
        )));
        let session = make_session(provider.clone())?;
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let settled = Arc::new(AtomicUsize::new(0));
        let ev = events.clone();
        let s = settled.clone();
        let _u = session.subscribe(move |e| match e {
            AgentSessionEvent::AutoRetryStart { .. } => {
                mutex_value(&ev).push("start".to_owned());
            }
            AgentSessionEvent::AutoRetryEnd { .. } => {
                mutex_value(&ev).push("end".to_owned());
            }
            AgentSessionEvent::AgentSettled => {
                s.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let ev = mutex_value(&events).clone();
        assert!(
            ev.is_empty(),
            "auth error must not emit auto_retry events: {ev:?}"
        );
        assert_eq!(provider.call_count(), 1);
        assert_eq!(settled.load(Ordering::SeqCst), 1);
        assert_eq!(session.retry_attempt(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn single_settled_after_prompt() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("ok")),
        )));
        let session = make_session(provider)?;
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _u = session.subscribe(move |e| {
            if matches!(e, AgentSessionEvent::AgentSettled) {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        session
            .prompt("hi", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn settled_waits_for_agent_end_extension_processing() -> TestResult {
        let gate = Arc::new(Semaphore::new(0));
        let entered = Arc::new(Notify::new());
        let runner = Arc::new(TestRunner {
            agent_end_gate: Some(Arc::clone(&gate)),
            agent_end_entered: Some(Arc::clone(&entered)),
            ..TestRunner::default()
        });
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("ok")),
        )));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.extension_runner = Some(runner as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config).test_context("session")?;
        let settled = Arc::new(AtomicUsize::new(0));
        let settled_for_listener = Arc::clone(&settled);
        let _unsubscribe = session.subscribe(move |event| {
            if matches!(event, AgentSessionEvent::AgentSettled) {
                settled_for_listener.fetch_add(1, Ordering::SeqCst);
            }
        });

        let entered_wait = entered.notified();
        let session_for_prompt = Arc::clone(&session);
        let prompt = tokio::spawn(async move {
            session_for_prompt
                .prompt("hi", PromptOptions::default())
                .await
        });
        entered_wait.await;
        assert_eq!(settled.load(Ordering::SeqCst), 0);

        gate.add_permits(1);
        prompt
            .await
            .test_context("joining gated prompt")?
            .test_context("gated prompt")?;
        assert_eq!(settled.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn disconnect_cancels_agent_end_barrier() -> TestResult {
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let session = make_session(provider)?;
        let before = session.processed_agent_end_count();
        let session_for_wait = Arc::clone(&session);
        let waiter =
            tokio::spawn(
                async move { session_for_wait.wait_for_processed_agent_end(before).await },
            );
        tokio::task::yield_now().await;

        session.disconnect_from_agent();
        let wait_completed = waiter.await.test_context("joining agent-end waiter")?;
        assert!(!wait_completed);
        session.reconnect_to_agent();
        session.dispose().await;
        Ok(())
    }

    #[tokio::test]
    async fn queue_steer_and_follow_up() -> TestResult {
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let session = make_session(provider)?;
        session.queue_steer("a", Vec::new());
        assert_eq!(session.pending_message_count(), 1);
        session.queue_follow_up("b", Vec::new());
        assert_eq!(session.pending_message_count(), 2);
        session.clear_queue();
        assert_eq!(session.pending_message_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn prompt_preflight_flushes_bash_before_validation() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("unused")),
        )));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.model = None;
        let session = AgentSession::new(config).test_context("session")?;
        session.lock_inner().pending_bash_messages.push(
            crate::core::messages::BashExecutionMessage::from_fields(
                crate::core::messages::BashExecutionFields {
                    command: "printf pending".to_owned(),
                    output: "pending".to_owned(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp: 1,
                    exclude_from_context: None,
                },
            ),
        );

        assert!(session.has_pending_bash_messages());
        let result = session.prompt("x", PromptOptions::default()).await;

        assert!(matches!(result, Err(PromptError::Message(_))));
        assert!(!session.has_pending_bash_messages());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_flush_failure_retains_bash_message_for_retry() -> TestResult {
        let dir = tempfile::tempdir().test_context("tempdir")?;
        let mut manager = crate::core::sessions::SessionManager::create(
            dir.path().to_string_lossy().as_ref(),
            Some(dir.path().to_string_lossy().as_ref()),
            None,
        )
        .test_context("session manager")?;
        manager
            .append_message(&pi_agent::user_text("hi", std::iter::empty()))
            .test_context("append user")?;
        let assistant = AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(assistant_text(
            "answer",
        ))));
        manager
            .append_message(&assistant)
            .test_context("append assistant")?;
        let session_file = std::path::PathBuf::from(
            manager
                .get_session_file()
                .ok_or_else(|| "missing session file".to_owned())?,
        );
        let backup = dir.path().join("session-backup.jsonl");
        std::fs::rename(&session_file, &backup).test_context("move session aside")?;
        std::fs::create_dir(&session_file).test_context("block append path")?;

        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.model = None;
        config.session_manager = manager;
        let session = AgentSession::new(config).test_context("session")?;
        let bash_message = |command: &str, timestamp: i64| {
            crate::core::messages::BashExecutionMessage::from_fields(
                crate::core::messages::BashExecutionFields {
                    command: command.to_owned(),
                    output: command.to_owned(),
                    exit_code: Some(0),
                    cancelled: false,
                    truncated: false,
                    full_output_path: None,
                    timestamp,
                    exclude_from_context: None,
                },
            )
        };
        session.lock_inner().pending_bash_messages.extend([
            bash_message("printf first", 1),
            bash_message("printf second", 2),
        ]);

        let err = require_error(
            session.prompt("x", PromptOptions::default()).await,
            "persistence failure",
        )?;
        assert!(matches!(err, PromptError::Session(_)));
        assert_eq!(session.lock_inner().pending_bash_messages.len(), 2);

        std::fs::remove_dir(&session_file).test_context("remove append blocker")?;
        std::fs::rename(&backup, &session_file).test_context("restore session")?;
        session
            .flush_pending_bash_messages()
            .await
            .test_context("retry flush")?;
        assert!(!session.has_pending_bash_messages());
        let persisted =
            std::fs::read_to_string(&session_file).test_context("read persisted session")?;
        let first = persisted
            .find("printf first")
            .ok_or_else(|| "missing first bash".to_owned())?;
        let second = persisted
            .find("printf second")
            .ok_or_else(|| "missing second bash".to_owned())?;
        assert!(
            first < second,
            "retried bash messages must preserve queue order"
        );
        assert_eq!(persisted.matches("\"role\":\"bashExecution\"").count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn message_end_disk_failure_returns_typed_prompt_error() -> TestResult {
        let dir = tempfile::tempdir().test_context("tempdir")?;
        let mut manager = crate::core::sessions::SessionManager::create(
            dir.path().to_string_lossy().as_ref(),
            Some(dir.path().to_string_lossy().as_ref()),
            None,
        )
        .test_context("session manager")?;
        manager
            .append_message(&pi_agent::user_text("existing", std::iter::empty()))
            .test_context("append existing user")?;
        manager
            .append_message(&AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(
                assistant_text("existing answer"),
            ))))
            .test_context("append existing assistant")?;
        let before_count = manager.get_entries().len();
        let session_file = std::path::PathBuf::from(
            manager
                .get_session_file()
                .ok_or_else(|| "missing session file".to_owned())?,
        );
        let backup = dir.path().join("message-end-backup.jsonl");
        std::fs::rename(&session_file, &backup).test_context("move session aside")?;
        std::fs::create_dir(&session_file).test_context("block append path")?;

        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("new answer")),
        )));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.session_manager = manager;
        let session = AgentSession::new(config).test_context("session")?;

        let err = require_error(
            session
                .prompt("new question", PromptOptions::default())
                .await,
            "message-end persistence failure",
        )?;
        assert!(matches!(err, PromptError::Session(_)));
        assert_eq!(
            session.session_manager.lock().await.get_entries().len(),
            before_count,
            "failed append must not advance the in-memory tree"
        );

        std::fs::remove_dir(&session_file).test_context("remove append blocker")?;
        std::fs::rename(&backup, &session_file).test_context("restore session")?;
        Ok(())
    }

    #[tokio::test]
    async fn handle_post_settles_after_retry_path_without_queue() -> TestResult {
        // Observable post-run contract: retry → (no queue) → exactly one settle.
        // A successful recovery leaves retry_attempt at 0 and no pending queue.
        let provider = Arc::new(SeqProvider::new(split(
            vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ],
            vec![
                ok_event(start_event()),
                ok_event(done_ok(assistant_text("ok"))),
            ],
        )));
        let session = make_session(provider.clone())?;
        let order = Arc::new(StdMutex::new(Vec::<String>::new()));
        let o = order.clone();
        let _u = session.subscribe(move |e| match e {
            AgentSessionEvent::AutoRetryStart { .. } => {
                mutex_value(&o).push("retry".to_owned());
            }
            AgentSessionEvent::AutoRetryEnd { success, .. } => {
                mutex_value(&o).push(format!("retry_end:{success}"));
            }
            AgentSessionEvent::AgentSettled => {
                mutex_value(&o).push("settled".to_owned());
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let order = mutex_value(&order).clone();
        assert_eq!(
            order,
            vec![
                "retry".to_owned(),
                "retry_end:true".to_owned(),
                "settled".to_owned()
            ],
            "post-run order: {order:?}"
        );
        assert_eq!(provider.call_count(), 2);
        assert_eq!(session.pending_message_count(), 0);
        assert_eq!(session.retry_attempt(), 0);
        let last = require_some(session.agent.last_assistant(), "last assistant after retry")?;
        assert_eq!(last.stop_reason, StopReason::Stop);
        Ok(())
    }

    #[tokio::test]
    async fn run_agent_prompt_flushes_bash_before_settled() -> TestResult {
        // Smoke: completion settles exactly once after the prompt loop.
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("ok")),
        )));
        let session = make_session(provider)?;
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _u = session.subscribe(move |e| {
            if matches!(e, AgentSessionEvent::AgentSettled) {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        session
            .prompt("hi", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn null_runner_command_passes_through() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("ok")),
        )));
        let session = make_session(provider)?;
        session
            .prompt("/nonexistent hello", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let messages = session.messages();
        let roles: Vec<&str> = messages.iter().map(AgentMessage::role).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        Ok(())
    }

    #[derive(Default)]
    struct TestRunner {
        commands: Vec<String>,
        runs: Arc<StdMutex<Vec<String>>>,
        agent_end_gate: Option<Arc<Semaphore>>,
        agent_end_entered: Option<Arc<Notify>>,
    }

    impl ExtensionRunner for TestRunner {
        fn has_handlers(&self, event: &str) -> bool {
            event == "agent_end"
                && (self.agent_end_gate.is_some() || self.agent_end_entered.is_some())
        }
        fn emit(
            &self,
            event: AgentSessionEvent,
        ) -> BoxFuture<
            '_,
            Result<Option<crate::core::agent_session::CancelResult>, ExtensionRunnerError>,
        > {
            let gate = self.agent_end_gate.clone();
            let entered = self.agent_end_entered.clone();
            Box::pin(async move {
                if matches!(event, AgentSessionEvent::AgentEnd { .. }) {
                    if let Some(entered) = entered {
                        entered.notify_one();
                    }
                    if let Some(gate) = gate
                        && let Ok(permit) = gate.acquire_owned().await
                    {
                        permit.forget();
                    }
                }
                Ok(None)
            })
        }
        fn emit_message_end(
            &self,
            _m: AgentMessage,
        ) -> BoxFuture<'_, Result<Option<AgentMessage>, ExtensionRunnerError>> {
            Box::pin(async { Ok(None) })
        }
        fn emit_tool_call(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Map<String, serde_json::Value>,
        ) -> BoxFuture<'_, Result<Option<pi_agent::BeforeToolCallResult>, ExtensionRunnerError>>
        {
            Box::pin(async { Ok(None) })
        }
        fn emit_tool_result(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Map<String, serde_json::Value>,
            _: Vec<pi_ai::ToolResultContent>,
            _: serde_json::Value,
            _: bool,
        ) -> BoxFuture<'_, Result<Option<pi_agent::AfterToolCallResult>, ExtensionRunnerError>>
        {
            Box::pin(async { Ok(None) })
        }
        fn emit_input(
            &self,
            _: &str,
            _: Option<serde_json::Value>,
            _: &str,
            _: Option<&str>,
        ) -> BoxFuture<
            '_,
            Result<crate::core::agent_session::InputTransformResult, ExtensionRunnerError>,
        > {
            Box::pin(async { Ok(crate::core::agent_session::InputTransformResult::default()) })
        }
        fn emit_before_agent_start(
            &self,
            _: &str,
            _: Option<serde_json::Value>,
        ) -> BoxFuture<
            '_,
            Result<
                Option<crate::core::agent_session::BeforeAgentStartResult>,
                ExtensionRunnerError,
            >,
        > {
            Box::pin(async { Ok(None) })
        }
        fn emit_resources_discover(
            &self,
            _: &str,
            _: &str,
        ) -> BoxFuture<
            '_,
            Result<crate::core::resources::ResourceExtensionPaths, ExtensionRunnerError>,
        > {
            Box::pin(async { Ok(crate::core::resources::ResourceExtensionPaths::default()) })
        }
        fn get_registered_commands(&self) -> Vec<String> {
            self.commands.clone()
        }
        fn execute_command<'a>(
            &'a self,
            name: &'a str,
            args: &'a str,
        ) -> BoxFuture<'a, Result<bool, ExtensionRunnerError>> {
            mutex_value(&self.runs).push(format!("{name}:{args}"));
            Box::pin(async { Ok(true) })
        }
        fn get_all_registered_tools(&self) -> HashMap<String, Arc<dyn pi_agent::AgentTool>> {
            HashMap::new()
        }
        fn get_flag_values(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn invalidate(&self) {}
        fn emit_error(&self, _: String) {}
        fn shutdown(&self, _: &str) -> BoxFuture<'_, Result<(), ExtensionRunnerError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn extension_command_dispatched_idle() -> TestResult {
        let runner = Arc::new(TestRunner {
            commands: vec!["testcmd".to_owned()],
            runs: Arc::new(StdMutex::new(Vec::new())),
            ..TestRunner::default()
        });
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("queued")),
        )));
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.extension_runner = Some(runner.clone() as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config).test_context("session")?;

        session
            .prompt("/testcmd hello world", PromptOptions::default())
            .await
            .test_context("prompt")?;

        let runs = mutex_value(&runner.runs).clone();
        assert_eq!(runs, vec!["testcmd:hello world"]);
        assert!(session.messages().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn steer_extension_command_rejected() -> TestResult {
        let runner = Arc::new(TestRunner {
            commands: vec!["testcmd".to_owned()],
            runs: Arc::new(StdMutex::new(Vec::new())),
            ..TestRunner::default()
        });
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let mut config = AgentSessionConfig::test_config(provider, test_model())
            .map_err(|error| format!("test config failed: {error}"))?;
        config.extension_runner = Some(runner as Arc<dyn ExtensionRunner>);
        let session = AgentSession::new(config)
            .map_err(|error| format!("session creation failed: {error}"))?;

        let err = match session.steer("/testcmd x", Vec::new()) {
            Ok(()) => return Err("extension command unexpectedly queued".to_owned()),
            Err(error) => error,
        };
        assert!(
            err.to_string()
                .contains("Extension command \"/testcmd\" cannot be queued"),
            "{err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn retry_exhaust_emits_failure() -> TestResult {
        // max_retries default 3 → 4 provider streams (initial + 3 retries), then end:false.
        let provider = Arc::new(SeqProvider::new(vec![
            sequence(vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ]),
            sequence(vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ]),
            sequence(vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ]),
            sequence(vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ]),
        ]));
        let session = make_session(provider.clone())?;
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let settled = Arc::new(AtomicUsize::new(0));
        let ev = events.clone();
        let s = settled.clone();
        let _u = session.subscribe(move |event| match event {
            AgentSessionEvent::AutoRetryStart { attempt, .. } => {
                mutex_value(&ev).push(format!("start:{attempt}"));
            }
            AgentSessionEvent::AutoRetryEnd {
                success, attempt, ..
            } => {
                mutex_value(&ev).push(format!("end:{success}:{attempt}"));
            }
            AgentSessionEvent::AgentSettled => {
                s.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let ev = mutex_value(&events).clone();
        assert_eq!(
            provider.call_count(),
            4,
            "initial + max_retries=3 must exhaust without phantom calls"
        );
        assert_eq!(
            ev,
            vec![
                "start:1".to_owned(),
                "start:2".to_owned(),
                "start:3".to_owned(),
                "end:false:3".to_owned(),
            ],
            "exhaust lifecycle: {ev:?}"
        );
        assert_eq!(settled.load(Ordering::SeqCst), 1);
        assert_eq!(session.retry_attempt(), 0);
        assert!(!session.agent.state().is_streaming);
        Ok(())
    }

    #[tokio::test]
    async fn abort_retry_during_sleep() -> TestResult {
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_err(assistant_error("overloaded_error")),
        )));
        let session = make_session(provider.clone())?;
        {
            let mut inner = session.lock_inner();
            inner.max_retries = 3;
        }
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let ev = events.clone();
        let session_for_abort = Arc::clone(&session);
        let _u = session.subscribe(move |event| match event {
            AgentSessionEvent::AutoRetryStart { attempt, .. } => {
                mutex_value(&ev).push(format!("start:{attempt}"));
                let session = Arc::clone(&session_for_abort);
                tokio::spawn(async move {
                    tokio::task::yield_now().await;
                    session.abort_retry();
                });
            }
            AgentSessionEvent::AutoRetryEnd {
                success,
                final_error,
                ..
            } => {
                mutex_value(&ev).push(format!(
                    "end:{success}:{}",
                    final_error.as_deref().unwrap_or("")
                ));
            }
            AgentSessionEvent::AgentSettled => {
                mutex_value(&ev).push("settled".to_owned());
            }
            _ => {}
        });
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        let ev = mutex_value(&events).clone();
        assert_eq!(
            ev,
            vec![
                "start:1".to_owned(),
                "end:false:Retry cancelled".to_owned(),
                "settled".to_owned(),
            ],
            "abort lifecycle: {ev:?}"
        );
        assert_eq!(
            provider.call_count(),
            1,
            "abort during sleep must not start another provider stream"
        );
        assert_eq!(session.retry_attempt(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn retry_then_tool_loop_keeps_prompt_open() -> TestResult {
        let provider = Arc::new(SeqProvider::new(split(
            vec![
                ok_event(start_event()),
                ok_event(done_err(assistant_error("overloaded_error"))),
            ],
            vec![
                ok_event(start_event()),
                ok_event(done_ok(assistant_text("recovered"))),
            ],
        )));
        let session = make_session(provider)?;
        session
            .prompt("test", PromptOptions::default())
            .await
            .test_context("prompt")?;
        drain(&session).await;
        assert!(!session.agent.state().is_streaming);
        Ok(())
    }

    /// Session whose next append fails (session file path blocked by a dir).
    fn blocked_append_session(
        provider: Arc<dyn Provider>,
        dir: &tempfile::TempDir,
    ) -> TestResult<Arc<AgentSession>> {
        let mut manager = crate::core::sessions::SessionManager::create(
            dir.path().to_string_lossy().as_ref(),
            Some(dir.path().to_string_lossy().as_ref()),
            None,
        )
        .test_context("session manager")?;
        manager
            .append_message(&pi_agent::user_text("seed", std::iter::empty()))
            .test_context("seed user append")?;
        // Persistence is lazy until an assistant entry exists; materialize the
        // file so the directory blocker makes every later append fail.
        manager
            .append_message(&AgentMessage::Llm(Box::new(pi_ai::Message::Assistant(
                assistant_text("seed answer"),
            ))))
            .test_context("seed assistant append")?;
        let session_file = std::path::PathBuf::from(
            manager
                .get_session_file()
                .ok_or_else(|| "missing session file".to_owned())?,
        );
        std::fs::remove_file(&session_file).test_context("remove session file")?;
        std::fs::create_dir(&session_file).test_context("block append path")?;
        let mut config =
            AgentSessionConfig::test_config(provider, test_model()).test_context("test config")?;
        config.session_manager = manager;
        AgentSession::new(config).test_context("session")
    }

    #[tokio::test]
    async fn provider_error_run_observes_agent_end_before_settled() -> TestResult {
        let provider = Arc::new(SeqProvider::new(vec![vec![Err(
            pi_ai::ProviderError::new("stream exploded"),
        )]]));
        let session = make_session(provider)?;
        let order = Arc::new(StdMutex::new(Vec::new()));
        let order_clone = Arc::clone(&order);
        let _unsub = session.subscribe(move |event| {
            mutex_value(&order_clone).push(event.type_name().to_owned());
        });

        // The provider failure surfaces as an error assistant terminal; the
        // regression under test is event ordering, not the prompt result.
        let _ = session.prompt("hi", PromptOptions::default()).await;

        let observed = mutex_value(&order).clone();
        let end = require_some(
            observed.iter().position(|name| name == "agent_end"),
            "public agent_end for the failed run",
        )?;
        let settled = require_some(
            observed.iter().position(|name| name == "agent_settled"),
            "agent_settled after the failed run",
        )?;
        assert!(
            end < settled,
            "agent_end must be observed before agent_settled: {observed:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn idle_custom_message_append_failure_publishes_nothing() -> TestResult {
        let dir = tempfile::tempdir().test_context("tempdir")?;
        let provider = Arc::new(SeqProvider::new(one(start_event())));
        let session = blocked_append_session(provider, &dir)?;
        let events = Arc::new(StdMutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let _unsub = session.subscribe(move |event| {
            mutex_value(&events_clone).push(event.type_name().to_owned());
        });
        let transcript_before = session.messages().len();

        let err = require_error(
            session
                .send_custom_message(
                    CustomMessageInput {
                        custom_type: "note".to_owned(),
                        content: CustomMessageContent::Text("hello".to_owned()),
                        display: true,
                        details: None,
                    },
                    false,
                    None,
                )
                .await,
            "idle custom append",
        )?;
        assert!(matches!(err, PromptError::Session(_)), "{err}");
        assert_eq!(
            session.messages().len(),
            transcript_before,
            "failed durable append must not mutate live transcript"
        );
        assert!(
            mutex_value(&events).is_empty(),
            "failed durable append must not publish message events: {:?}",
            mutex_value(&events)
        );
        Ok(())
    }

    #[tokio::test]
    async fn message_end_disk_failure_settles_only_after_agent_end() -> TestResult {
        let dir = tempfile::tempdir().test_context("tempdir")?;
        let provider = Arc::new(SeqProvider::new(two(
            start_event(),
            done_ok(assistant_text("answer")),
        )));
        let session = blocked_append_session(provider, &dir)?;
        let order = Arc::new(StdMutex::new(Vec::new()));
        let order_clone = Arc::clone(&order);
        let _unsub = session.subscribe(move |event| {
            mutex_value(&order_clone).push(event.type_name().to_owned());
        });

        let err = require_error(
            session.prompt("question", PromptOptions::default()).await,
            "disk-failure run",
        )?;
        assert!(matches!(err, PromptError::Session(_)), "{err}");

        let observed = mutex_value(&order).clone();
        let message_end = require_some(
            observed.iter().position(|name| name == "message_end"),
            "public message_end for the failed persistence",
        )?;
        let agent_end = require_some(
            observed.iter().position(|name| name == "agent_end"),
            "public agent_end after the persistence failure",
        )?;
        let settled = require_some(
            observed.iter().position(|name| name == "agent_settled"),
            "final agent_settled",
        )?;
        assert!(
            message_end < agent_end && agent_end < settled,
            "persistence failure must not settle before agent_end: {observed:?}"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|name| *name == "agent_settled")
                .count(),
            1,
            "exactly one settle per run: {observed:?}"
        );
        Ok(())
    }

    #[allow(dead_code)]
    fn _ensure_null_runner_send_sync(_: NullExtensionRunner) {}

    #[allow(dead_code)]
    fn _ensure_ok_event_ok(e: AssistantMessageEvent) {
        let _ = ok_event(e);
    }
}
