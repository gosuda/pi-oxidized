//! Product bash execution impls.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/agent-session.ts`
//! `executeBash`, `recordBashResult`, `abortBash`, `isBashRunning`,
//! `hasPendingBashMessages`, and `_flushPendingBashMessages`.
//!
//! Behaviour preserved from the TypeScript contract:
//! - `execute_bash` prepends `settings.shellCommandPrefix` (when set), runs
//!   through the configured `BashOperations`, streams output chunks through
//!   `on_chunk`, and records a `BashExecutionMessage` entry on the session.
//! - `exclude_from_context: true` (`!!` prefix) records the entry with the
//!   flag set so [`crate::core::messages`] drops it from LLM context.
//! - While the agent is streaming, recorded bash messages are queued and
//!   flushed on `agent_end` so `tool_use` / `tool_result` ordering is preserved.
//! - `abort_bash` cancels the in-flight token; the next call can start a new
//!   command.
//!
//! Lock order: never hold `AgentSessionInner` across `.await`. The session
//! manager async mutex is acquired for append-only persistence.

use std::path::PathBuf;
use std::sync::Arc;

use pi_agent::{AgentMessage, AgentTool};
use tokio_util::sync::CancellationToken;

use crate::core::messages::{BashExecutionFields, BashExecutionMessage};

use crate::core::tools::bash::{BashOperations, BashTool, BashToolOptions};

use super::AgentSession;
/// Result of a product bash execution (TypeScript `BashResult`).
///
/// Mirrors the wire shape used by the `bash` RPC response and by
/// `BashExecutionMessage` persistence. Defined here (rather than re-exported
/// from `modes/rpc/types.rs`) so the agent-session slice owns the canonical
/// product shape without creating a `modes → core` dependency.
#[derive(Clone, Debug, PartialEq)]
pub struct BashResult {
    /// Combined stdout + stderr (sanitized, possibly truncated).
    pub output: String,
    /// Process exit code (`None` when killed/cancelled).
    pub exit_code: Option<i32>,
    /// Whether the command was aborted before completion.
    pub cancelled: bool,
    /// Whether `output` is a truncated view of the full stream.
    pub truncated: bool,
    /// Spill-file path captured when `truncated` is true.
    pub full_output_path: Option<String>,
}

/// Errors produced by [`AgentSession::execute_bash`].
#[derive(Debug, thiserror::Error)]
pub enum BashExecError {
    /// Underlying bash execution failure (non-zero exit, abort, or timeout).
    /// Carries the formatted output and parsed result so callers can record
    /// the attempt just like the TypeScript reference does.
    #[error("{message}")]
    Execution {
        /// Human-readable error text (mirrors `ToolError::message`).
        message: String,
        /// Parsed bash result for session persistence.
        result: BashResult,
    },
    /// Session persistence failure.
    #[error(transparent)]
    Session(#[from] crate::core::sessions::SessionError),
}

/// Options accepted by [`AgentSession::execute_bash`].
#[derive(Clone, Default)]
pub struct ExecuteBashOptions {
    /// When `true`, the recorded message is excluded from LLM context
    /// (TypeScript `!!` prefix).
    pub exclude_from_context: bool,
    /// Custom operations backend (TypeScript `BashOperations`). Defaults to
    /// local shell execution using the settings-configured `shellPath`.
    pub operations: Option<Arc<dyn BashOperations>>,
}

impl std::fmt::Debug for ExecuteBashOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteBashOptions")
            .field("exclude_from_context", &self.exclude_from_context)
            .field("operations", &self.operations.as_ref().map(|_| "Some(..)"))
            .finish()
    }
}

impl AgentSession {
    /// Execute a bash command, stream output, and persist the result.
    ///
    /// The command runs in [`AgentSession::cwd`] resolved against the
    /// settings-configured shell path and command prefix. `on_chunk` receives
    /// merged stdout/stderr chunks as UTF-8 strings as they arrive.
    ///
    /// On non-zero exit / abort / timeout the returned `Err` carries the
    /// parsed [`BashResult`] so the caller can still inspect the output.
    ///
    /// # Errors
    ///
    /// Returns [`BashExecError::Execution`] when the command fails, or
    /// [`BashExecError::Session`] when persistence fails.
    pub async fn execute_bash<F>(
        &self,
        command: &str,
        on_chunk: Option<F>,
        options: ExecuteBashOptions,
    ) -> Result<BashResult, BashExecError>
    where
        F: FnMut(&str) + Send + 'static,
    {
        let token = self.begin_bash_abort();
        let (prefix, shell_path) = {
            let settings = self.lock_settings();
            (
                settings.get_shell_command_prefix(),
                settings.get_shell_path(),
            )
        };
        let resolved = match prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}\n{command}"),
            _ => command.to_owned(),
        };

        let result = run_bash(
            self.cwd.clone(),
            resolved.clone(),
            shell_path,
            options.operations.clone(),
            on_chunk,
            token.clone(),
        )
        .await;
        self.clear_bash_abort();

        let parsed = match result {
            Ok(parsed) => parsed,
            Err(err) => {
                let message = err.to_string();
                let parsed = parse_bash_result_from_error(&message, &resolved);
                let err = BashExecError::Execution {
                    message,
                    result: parsed,
                };
                return Err(err);
            }
        };

        // Persist + record (may queue if agent is streaming).
        self.record_bash_result(command, parsed.clone(), &options)
            .await?;
        Ok(parsed)
    }

    /// Record a bash result in session history.
    ///
    /// Used by [`Self::execute_bash`] and by extensions that handle bash
    /// execution themselves. While the agent is streaming, the entry is
    /// queued and flushed on `agent_end` to preserve `tool_use` / `tool_result`
    /// ordering.
    ///
    /// # Errors
    ///
    /// Returns [`BashExecError::Session`] when persistence fails on the
    /// immediate (non-deferred) path.
    pub async fn record_bash_result(
        &self,
        command: &str,
        result: BashResult,
        options: &ExecuteBashOptions,
    ) -> Result<(), BashExecError> {
        let message = BashExecutionMessage::from_fields(BashExecutionFields {
            command: command.to_owned(),
            output: result.output,
            exit_code: result.exit_code.map(i64::from),
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path,
            timestamp: pi_agent::now_millis(),
            exclude_from_context: if options.exclude_from_context {
                Some(true)
            } else {
                None
            },
        });
        // Serialize a bash-execution custom message (role="bashExecution").
        let agent_message: AgentMessage = AgentMessage::Custom(pi_agent::CustomAgentMessage::new(
            "bashExecution",
            bash_execution_payload(&message),
        ));

        if self.is_streaming() {
            // Defer to agent_end.
            let mut inner = self.lock_inner();
            inner.pending_bash_messages.push(message);
            return Ok(());
        }

        // Persist immediately.
        let mut manager = self.session_manager.lock().await;
        let id = manager.append_message(&agent_message)?;
        drop(manager);
        self.agent.push_message(agent_message);
        if let Some(entry) = self.session_manager.lock().await.get_entry(&id).cloned() {
            self.emit_public(super::events::AgentSessionEvent::EntryAppended { entry });
        }
        Ok(())
    }

    /// Whether there are pending bash messages waiting to be flushed.
    #[must_use]
    pub fn has_pending_bash_messages(&self) -> bool {
        !self.lock_inner().pending_bash_messages.is_empty()
    }

    /// Append pending bash messages in order, removing each only after its
    /// session append succeeds.
    ///
    /// Called by the pump after `agent_end` (TypeScript `_flushPendingBashMessages`).
    ///
    /// # Errors
    ///
    /// Returns [`BashExecError::Session`] on the first persistence failure.
    /// The failed message and every unattempted message remain queued in their
    /// original order so a later flush can retry without loss.
    pub async fn flush_pending_bash_messages(&self) -> Result<(), BashExecError> {
        // Serialize flush attempts without using the session-manager lock as the
        // serializer: lock order forbids taking `inner` while manager is held.
        let _flush_guard = self.bash_flush_lock.lock().await;
        loop {
            let message = {
                let inner = self.lock_inner();
                inner.pending_bash_messages.first().cloned()
            };
            let Some(message) = message else {
                return Ok(());
            };
            let agent_message: AgentMessage =
                AgentMessage::Custom(pi_agent::CustomAgentMessage::new(
                    "bashExecution",
                    bash_execution_payload(&message),
                ));
            let entry = {
                let mut manager = self.session_manager.lock().await;
                let id = manager.append_message(&agent_message)?;
                manager.get_entry(&id).cloned()
            };
            self.agent.push_message(agent_message);
            {
                let mut inner = self.lock_inner();
                if inner.pending_bash_messages.first() == Some(&message) {
                    inner.pending_bash_messages.remove(0);
                }
            }
            if let Some(entry) = entry {
                self.emit_public(super::events::AgentSessionEvent::EntryAppended { entry });
            }
        }
    }
}

/// Run a bash command through a `BashTool`, capturing the parsed result.
///
/// The tool wires `on_chunk` through `ToolUpdates` partial snapshots and
/// returns the formatted output / exit code / cancellation / spill metadata.
async fn run_bash<F>(
    cwd: String,
    command: String,
    shell_path: Option<String>,
    operations: Option<Arc<dyn BashOperations>>,
    on_chunk: Option<F>,
    cancel: CancellationToken,
) -> Result<BashResult, pi_agent::ToolError>
where
    F: FnMut(&str) + Send + 'static,
{
    let mut options = BashToolOptions::new(PathBuf::from(&cwd));
    if let Some(shell) = shell_path.filter(|s| !s.is_empty()) {
        options.shell_path = Some(PathBuf::from(shell));
    }
    if let Some(operations) = operations {
        options.operations = Some(operations);
    }
    let tool = BashTool::with_options(options);

    // Build the args map the BashTool expects (`BashToolInput`).
    let mut args = serde_json::Map::new();
    args.insert("command".to_owned(), serde_json::Value::String(command));

    // ToolUpdates sink: forward partial text to on_chunk. The BashTool emits
    // snapshots on a 100ms throttle, so this is naturally backpressured.
    let (updates, mut rx) = make_chunk_channel(on_chunk);

    let result = tool
        .execute("agent-session-bash", args, cancel, updates)
        .await;

    // Drain any straggling partial snapshots before computing the final result.
    while rx.recv().await.is_some() {}

    let agent_result = result?;
    Ok(parse_bash_result_from_agent_result(agent_result))
}

/// Convert a successful `AgentToolResult` into a `BashResult`.
fn parse_bash_result_from_agent_result(result: pi_agent::AgentToolResult) -> BashResult {
    let output = result
        .content
        .into_iter()
        .find_map(|block| match block {
            pi_ai::ToolResultContent::Text(text) => Some(text.text),
            pi_ai::ToolResultContent::Image(_) => None,
        })
        .unwrap_or_default();
    let details: serde_json::Value = result.details;
    let truncation = details.get("truncation").cloned();
    let full_output_path = details
        .get("fullOutputPath")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let truncated = truncation
        .as_ref()
        .and_then(|value| value.get("truncated"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // Success implies exit code 0; non-zero exits propagate as ToolError.
    BashResult {
        output,
        exit_code: Some(0),
        cancelled: false,
        truncated,
        full_output_path,
    }
}

/// Convert a `ToolError` message into a `BashResult`.
///
/// Recognizes `"Command exited with code N"`, the `"aborted"` sentinel, and
/// the `"timeout:N"` sentinel produced by the local `BashOperations`.
fn parse_bash_result_from_error(message: &str, _command: &str) -> BashResult {
    let exit_code = message.find("Command exited with code ").and_then(|idx| {
        let rest = &message[idx + "Command exited with code ".len()..];
        rest.split_whitespace()
            .next()
            .and_then(|token| token.trim().parse::<i32>().ok())
    });
    let cancelled = message.contains("Command aborted");
    let timed_out = message.contains("Command timed out");
    BashResult {
        output: strip_status_suffix(message),
        exit_code,
        cancelled,
        truncated: timed_out,
        full_output_path: None,
    }
}

/// Strip the trailing `Command ...` status line that the `BashTool` appends so
/// the recorded `output` field matches the raw stream.
fn strip_status_suffix(message: &str) -> String {
    if let Some(idx) = message.rfind("\n\nCommand exited with code ") {
        return message[..idx].to_owned();
    }
    if let Some(idx) = message.rfind("\n\nCommand aborted") {
        return message[..idx].to_owned();
    }
    if let Some(idx) = message.rfind("\n\nCommand timed out") {
        return message[..idx].to_owned();
    }
    message.to_owned()
}

/// Serialize a [`BashExecutionMessage`] into a custom-agent-message payload.
fn bash_execution_payload(
    message: &BashExecutionMessage,
) -> serde_json::Map<String, serde_json::Value> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "command".to_owned(),
        serde_json::Value::String(message.command.clone()),
    );
    payload.insert(
        "output".to_owned(),
        serde_json::Value::String(message.output.clone()),
    );
    if let Some(exit_code) = message.exit_code {
        payload.insert("exitCode".to_owned(), serde_json::Value::from(exit_code));
    }
    payload.insert(
        "cancelled".to_owned(),
        serde_json::Value::Bool(message.cancelled),
    );
    payload.insert(
        "truncated".to_owned(),
        serde_json::Value::Bool(message.truncated),
    );
    if let Some(path) = message.full_output_path.clone() {
        payload.insert("fullOutputPath".to_owned(), serde_json::Value::String(path));
    }
    payload.insert(
        "timestamp".to_owned(),
        serde_json::Value::from(message.timestamp),
    );
    if message.exclude_from_context.unwrap_or(false) {
        payload.insert(
            "excludeFromContext".to_owned(),
            serde_json::Value::Bool(true),
        );
    }
    payload
}

/// Build a `ToolUpdates` channel that forwards partial text to `on_chunk`.
fn make_chunk_channel<F>(
    on_chunk: Option<F>,
) -> (pi_agent::ToolUpdates, tokio::sync::mpsc::Receiver<()>)
where
    F: FnMut(&str) + Send + 'static,
{
    use std::sync::Mutex;
    let callback: Arc<Mutex<Option<F>>> = Arc::new(Mutex::new(on_chunk));
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(64);
    let tx = Arc::new(Mutex::new(Some(tx)));
    let updates = pi_agent::ToolUpdates::new(move |result| {
        let text = result
            .content
            .iter()
            .find_map(|block| match block {
                pi_ai::ToolResultContent::Text(text) => Some(text.text.as_str()),
                pi_ai::ToolResultContent::Image(_) => None,
            })
            .unwrap_or("");
        if text.is_empty() {
            return;
        }
        let Some(mut cb) = callback.lock().ok().and_then(|mut g| g.take()) else {
            return;
        };
        cb(text);
        // Re-store the callback so future partials can use it.
        if let Ok(mut guard) = callback.lock() {
            *guard = Some(cb);
        }
        // Notify the receiver that a partial arrived. Best-effort send: if the
        // channel is full we drop the signal because the receiver only needs
        // to know *that* activity happened, not how many chunks.
        if let Ok(tx_guard) = tx.lock()
            && let Some(tx) = tx_guard.as_ref()
        {
            let _ = tx.try_send(());
        }
    });
    (updates, rx)
}
