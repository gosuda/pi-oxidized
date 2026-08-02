//! Tree navigation, export, and naming.
//!
//! Implements the tree-navigation surface from
//! `coding-agent/src/core/agent-session.ts` (`navigateTree`,
//! `getUserMessagesForForking`, `getLastAssistantText`, `setSessionName`,
//! `exportToHtml`, `exportToJsonl`). Session statistics / context usage live
//! in `stats.rs` (sibling module).
//!
//! All persistence flows through the shared session-manager async mutex;
//! public listeners are invoked without holding the inner mutex.

use std::pin::Pin;
use std::sync::Arc;

use pi_agent::AgentMessage;
use pi_ai::{AssistantContent, Message, StopReason};
use tokio_util::sync::CancellationToken;

use super::AgentSession;
use super::events::{AgentSessionEvent, SummarizationRetrySource};
use crate::core::compaction::{
    GenerateBranchSummaryOptions, SummarizeStreamFn, collect_entries_for_branch_summary,
    generate_branch_summary,
};
use crate::core::export_html::{
    ExportError, ExportOptions, RenderedResult, RenderedToolHtml, SessionExportState,
    ToolHtmlRenderer, export_session_to_html, resolve_export_theme,
};
use crate::core::session_transfer::{SessionTransferError, export_branch_to_jsonl};
use crate::core::sessions::{SessionEntry, SessionError};

// ---------------------------------------------------------------------------
// Errors / options
// ---------------------------------------------------------------------------

/// Errors raised by tree-level session operations.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// Target entry does not exist.
    #[error("Entry {0} not found")]
    EntryNotFound(String),
    /// Summarization requested but no model is selected.
    #[error("No model available for summarization")]
    NoModel,
    /// Summarization failed or was cancelled with an error.
    #[error("Branch summarization failed: {0}")]
    Summarization(String),
    /// Session JSONL export failed.
    #[error(transparent)]
    Export(#[from] SessionTransferError),
    /// HTML export failed.
    #[error(transparent)]
    HtmlExport(#[from] ExportError),
    /// Session persistence error.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Options accepted by [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default)]
pub struct NavigateTreeOptions {
    /// When true, generate (or accept from extensions) a branch summary.
    pub summarize: bool,
    /// Custom summarization instructions appended to the prompt.
    pub custom_instructions: Option<String>,
    /// When true, `custom_instructions` replaces the default prompt.
    pub replace_instructions: bool,
    /// Optional label attached to the new branch-summary entry (or target).
    pub label: Option<String>,
}

/// Outcome of [`AgentSession::navigate_tree`].
#[derive(Clone, Debug, Default)]
pub struct NavigateTreeResult {
    /// User-message text to place in the editor when navigating to a user message.
    pub editor_text: Option<String>,
    /// True when an extension cancelled the navigation.
    pub cancelled: bool,
    /// True when the summarization step was aborted by cancellation.
    pub aborted: bool,
    /// Created branch-summary entry, when a summary was produced.
    pub summary_entry: Option<SessionEntry>,
}

/// A user-message entry selectable for forking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkableUserMessage {
    /// Entry id.
    pub entry_id: String,
    /// Concatenated text content of the user message.
    pub text: String,
}

/// Auth headers / env passed to the branch summarizer.
///
/// Resolved by the caller (model-runtime slice); the tree module is agnostic
/// to the source so it can be tested with a fake.
#[derive(Clone, Debug, Default)]
pub struct SummarizationAuth {
    /// Explicit API key.
    pub api_key: Option<String>,
    /// Optional request headers.
    pub headers: Option<std::collections::BTreeMap<String, Option<String>>>,
    /// Provider-scoped environment overrides.
    pub env: Option<std::collections::BTreeMap<String, String>>,
}

/// Inputs collected before running the summarizer (mirrors TS `TreePreparation`).
#[derive(Clone, Debug, Default)]
pub struct TreePreparation {
    /// Target entry id.
    pub target_id: String,
    /// Previous leaf id (before navigation).
    pub old_leaf_id: Option<String>,
    /// Common ancestor of the old leaf and target paths.
    pub common_ancestor_id: Option<String>,
    /// Entries on the abandoned path, leaf→ancestor order.
    pub entries_to_summarize: Vec<SessionEntry>,
    /// Whether the user requested summarization.
    pub user_wants_summary: bool,
    /// Custom summarization instructions (mutable by extensions).
    pub custom_instructions: Option<String>,
    /// Whether to replace the default summarization prompt.
    pub replace_instructions: bool,
    /// Label to attach.
    pub label: Option<String>,
}

/// Async closure that pre-renders a tool call or result to HTML fragments.
///
/// Used by [`AgentSession::export_to_html`] to bridge the async extension
/// runner into the sync [`ToolHtmlRenderer`] expected by the exporter. The
/// closure receives `(tool_call_id, tool_name, payload_json)` and returns
/// optional pre-rendered HTML. When `None`, no extension tool rendering is
/// applied and the built-in generic viewer is used.
pub type ToolHtmlPreRenderer = Arc<
    dyn Fn(
            String,
            String,
            serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Option<RenderedToolHtml>> + Send>>
        + Send
        + Sync,
>;

/// Sync wrapper over a pre-rendered `HashMap<tool_call_id, RenderedToolHtml>`.
///
/// Implements [`ToolHtmlRenderer`] by looking up pre-rendered fragments,
/// bridging the async extension runner into the sync export pipeline.
struct MapToolHtmlRenderer {
    calls: std::collections::HashMap<String, RenderedToolHtml>,
}

impl ToolHtmlRenderer for MapToolHtmlRenderer {
    fn render_call(
        &self,
        tool_call_id: &str,
        _tool_name: &str,
        _arguments: &serde_json::Value,
    ) -> Option<String> {
        self.calls
            .get(tool_call_id)
            .and_then(|r| r.call_html.clone())
    }

    fn render_result(
        &self,
        tool_call_id: &str,
        _tool_name: &str,
        _result: &[pi_ai::ToolResultContent],
        _details: Option<&serde_json::Value>,
        _is_error: bool,
    ) -> Option<RenderedResult> {
        self.calls.get(tool_call_id).map(|r| RenderedResult {
            collapsed: r.result_html_collapsed.clone(),
            expanded: r.result_html_expanded.clone(),
        })
    }
}

impl AgentSession {
    /// Navigate the session tree to `target_id`.
    ///
    /// Ordering matches `navigateTree` in TS:
    /// 1. No-op when already at target.
    /// 2. Validate target entry exists.
    /// 3. Collect entries on the abandoned path.
    /// 4. Query `session_before_tree` handler presence (cancellable when the
    ///    typed variant lands).
    /// 5. Run summarizer when requested.
    /// 6. Position the leaf (branch / reset / branch-with-summary).
    /// 7. Attach label to summary or target entry.
    /// 8. Rebuild agent messages from session context.
    /// 9. Signal `session_tree` handler presence.
    ///
    /// # Errors
    ///
    /// See [`TreeError`].
    pub async fn navigate_tree(
        self: &Arc<Self>,
        target_id: &str,
        options: NavigateTreeOptions,
        auth: SummarizationAuth,
        summarizer: Option<&SummarizeStreamFn>,
    ) -> Result<NavigateTreeResult, TreeError> {
        let old_leaf_id = {
            let sm = self.session_manager.lock().await;
            sm.get_leaf_id().map(str::to_owned)
        };

        if Some(target_id) == old_leaf_id.as_deref() {
            // Navigation itself is a no-op, but a requested label is still a
            // persisted action. As in the non-summary TypeScript path, attach
            // it to the navigation target rather than to the label entry.
            if let Some(label) = options.label.as_deref() {
                self.session_manager
                    .lock()
                    .await
                    .append_label_change(target_id, Some(label))?;
            }
            return Ok(NavigateTreeResult::default());
        }

        if options.summarize {
            let model = self.model();
            if model.id.is_empty() {
                return Err(TreeError::NoModel);
            }
        }

        // Validate + collect under lock, then release before any async work.
        let (target_entry, preparation) = {
            let sm = self.session_manager.lock().await;
            let target_entry = sm
                .get_entry(target_id)
                .cloned()
                .ok_or_else(|| TreeError::EntryNotFound(target_id.to_owned()))?;

            let collected =
                collect_entries_for_branch_summary(&sm, old_leaf_id.as_deref(), target_id);
            let prep = TreePreparation {
                target_id: target_id.to_owned(),
                old_leaf_id: old_leaf_id.clone(),
                common_ancestor_id: collected.common_ancestor_id.clone(),
                entries_to_summarize: collected.entries.clone(),
                user_wants_summary: options.summarize,
                custom_instructions: options.custom_instructions.clone(),
                replace_instructions: options.replace_instructions,
                label: options.label.clone(),
            };
            (target_entry, prep)
        };

        // Set up cancellation slot.
        let token = self.begin_branch_summary_abort();

        // Extension before_tree hook gate.
        let _before_handlers = self.has_extension_handlers("session_before_tree");

        let result = self
            .navigate_tree_inner(target_entry, preparation, auth, summarizer, &token)
            .await;
        self.clear_branch_summary_abort();
        result
    }

    async fn navigate_tree_inner(
        self: &Arc<Self>,
        target_entry: SessionEntry,
        preparation: TreePreparation,
        auth: SummarizationAuth,
        summarizer: Option<&SummarizeStreamFn>,
        token: &CancellationToken,
    ) -> Result<NavigateTreeResult, TreeError> {
        // Run default summarizer when requested.
        let mut summary_text: Option<String> = None;
        let mut summary_details: Option<serde_json::Value> = None;
        let mut summary_usage: Option<pi_ai::Usage> = None;
        let mut from_extension = false;
        if preparation.user_wants_summary
            && !preparation.entries_to_summarize.is_empty()
            && let Some(stream_fn) = summarizer
        {
            let model = self.model();
            let reserve_tokens = self
                .lock_settings()
                .get_branch_summary_settings()
                .reserve_tokens;
            let opts = GenerateBranchSummaryOptions {
                model: model.clone(),
                api_key: auth.api_key.clone(),
                headers: auth.headers.clone(),
                env: auth.env.clone(),
                signal: token.clone(),
                custom_instructions: preparation.custom_instructions.clone(),
                replace_instructions: preparation.replace_instructions,
                reserve_tokens: Some(reserve_tokens),
                stream_fn: Arc::clone(stream_fn),
                retry: Some(self.summarization_retry_policy()),
                retry_callbacks: Some(
                    self.summarization_retry_callbacks(SummarizationRetrySource::BranchSummary),
                ),
            };
            let result = generate_branch_summary(&preparation.entries_to_summarize, opts)
                .await
                .map_err(|e| TreeError::Summarization(e.to_string()))?;
            if result.aborted.unwrap_or(false) {
                return Ok(NavigateTreeResult {
                    cancelled: true,
                    aborted: true,
                    ..Default::default()
                });
            }
            if let Some(err) = result.error.clone() {
                return Err(TreeError::Summarization(err));
            }
            summary_text = result.summary;
            summary_usage = result.usage;
            summary_details = Some(serde_json::json!({
                "readFiles": result.read_files.unwrap_or_default(),
                "modifiedFiles": result.modified_files.unwrap_or_default(),
            }));
            from_extension = false;
        }
        let _ = from_extension;

        // Determine new leaf id + editor text from the target entry shape.
        let (new_leaf_id, editor_text) = compute_new_leaf_and_editor_text(&target_entry);

        // Persist leaf change + optional summary under the lock.
        let summary_entry = {
            let mut sm = self.session_manager.lock().await;
            if let Some(text) = summary_text.as_deref() {
                let id = sm.branch_with_summary(
                    new_leaf_id.as_deref(),
                    text,
                    summary_details.clone(),
                    from_extension.then_some(true),
                    summary_usage.clone(),
                )?;
                if let Some(l) = preparation.label.as_deref() {
                    let _ = sm.append_label_change(&id, Some(l));
                }
                sm.get_entry(&id).cloned()
            } else if new_leaf_id.is_none() {
                sm.reset_leaf();
                if let Some(l) = preparation.label.as_deref() {
                    let _ = sm.append_label_change(&preparation.target_id, Some(l));
                }
                None
            } else {
                sm.branch(new_leaf_id.as_deref().unwrap_or(""))?;
                if let Some(l) = preparation.label.as_deref() {
                    let _ = sm.append_label_change(&preparation.target_id, Some(l));
                }
                None
            }
        };

        // Rebuild agent messages from new session context.
        let session_context = {
            let sm = self.session_manager.lock().await;
            sm.build_session_context()
                .map_err(|e| TreeError::Summarization(e.to_string()))?
        };
        self.agent.replace_messages(session_context.messages);

        // Signal session_tree handler presence.
        let _ = self.has_extension_handlers("session_tree");

        Ok(NavigateTreeResult {
            editor_text,
            cancelled: false,
            aborted: false,
            summary_entry,
        })
    }

    /// Begin the branch-summary cancellation slot.
    fn begin_branch_summary_abort(&self) -> CancellationToken {
        let token = CancellationToken::new();
        let mut inner = self.lock_inner();
        if let Some(prev) = inner.branch_summary_abort.take() {
            prev.cancel();
        }
        inner.branch_summary_abort = Some(token.clone());
        token
    }

    /// Clear the branch-summary cancellation slot.
    fn clear_branch_summary_abort(&self) {
        self.lock_inner().branch_summary_abort = None;
    }

    /// Abort in-flight branch summarization.
    pub fn abort_branch_summary(&self) {
        let mut inner = self.lock_inner();
        if let Some(token) = inner.branch_summary_abort.take() {
            token.cancel();
        }
    }

    /// Collect user messages on the current branch available for forking.
    ///
    /// Order matches TS: walk all entries, keep message entries with role
    /// `user` and non-empty text.
    pub async fn get_user_messages_for_forking(&self) -> Vec<ForkableUserMessage> {
        let entries: Vec<SessionEntry> = {
            let sm = self.session_manager.lock().await;
            sm.get_entries().into_iter().cloned().collect()
        };
        let mut out = Vec::new();
        for entry in entries {
            if let SessionEntry::Message(m) = &entry {
                if m.message.role() != "user" {
                    continue;
                }
                let text = extract_user_message_text(&m.message);
                if !text.is_empty() {
                    out.push(ForkableUserMessage {
                        entry_id: m.id.clone(),
                        text,
                    });
                }
            }
        }
        out
    }

    /// Export the session to a self-contained HTML file.
    ///
    /// Captures the live agent state (system prompt + active tools), resolves
    /// the configured theme name, and optionally pre-renders extension tool
    /// calls / results into HTML fragments via `tool_pre_renderer`.
    ///
    /// # Errors
    ///
    /// See [`ExportError`].
    pub async fn export_to_html(
        &self,
        output_path: Option<&str>,
        tool_pre_renderer: Option<ToolHtmlPreRenderer>,
    ) -> Result<String, ExportError> {
        // Capture export state (systemPrompt + tools).
        let snapshot = self.agent.state();
        let state = SessionExportState::from_agent_snapshot(&snapshot);

        // Headless resolution: Auto/pairs pick the dark member (TerminalTheme::Dark).
        let theme = {
            let settings = self.lock_settings();
            resolve_export_theme(settings.get_theme().as_deref(), settings.get_theme_mode())
        };

        // Pre-render extension tool HTML (async → sync bridge).
        let map_renderer = if let Some(renderer) = tool_pre_renderer {
            let entries = {
                let sm = self.session_manager.lock().await;
                sm.get_branch(None).into_iter().cloned().collect::<Vec<_>>()
            };
            let mut calls: std::collections::HashMap<String, RenderedToolHtml> =
                std::collections::HashMap::new();
            for entry in &entries {
                if let SessionEntry::Message(m) = entry {
                    if let Some(Message::Assistant(assistant)) = m.message.as_llm() {
                        for block in &assistant.content {
                            if let AssistantContent::ToolCall(call) = block {
                                let args = serde_json::Value::Object(call.arguments.clone());
                                if let Some(rendered) =
                                    renderer(call.id.clone(), call.name.clone(), args).await
                                {
                                    calls.insert(call.id.clone(), rendered);
                                }
                            }
                        }
                    }
                    if let Some(Message::ToolResult(result)) = m.message.as_llm() {
                        let payload = serde_json::to_value(&result.content).unwrap_or_default();
                        if let Some(rendered) = renderer(
                            result.tool_call_id.clone(),
                            result.tool_name.clone(),
                            payload,
                        )
                        .await
                        {
                            let entry = calls.entry(result.tool_call_id.clone()).or_default();
                            entry.result_html_collapsed = rendered.result_html_collapsed;
                            entry.result_html_expanded = rendered.result_html_expanded;
                        }
                    }
                }
            }
            Some(MapToolHtmlRenderer { calls })
        } else {
            None
        };

        let sm = self.session_manager.lock().await;
        let opts = ExportOptions {
            output_path: output_path.map(std::path::PathBuf::from),
            theme_name: None,
            theme: Some(theme),
            tool_renderer: map_renderer.as_ref().map(|r| r as &dyn ToolHtmlRenderer),
        };
        export_session_to_html(&sm, Some(&state), opts)
    }

    /// Export the current branch to a linearized JSONL file.
    ///
    /// # Errors
    ///
    /// See [`SessionTransferError`].
    pub async fn export_to_jsonl(
        &self,
        output_path: Option<&str>,
    ) -> Result<String, SessionTransferError> {
        let sm = self.session_manager.lock().await;
        export_branch_to_jsonl(&sm, output_path)
    }

    /// Last assistant message text (skipping aborted empty messages).
    ///
    /// Returns `None` when no usable assistant message exists.
    #[must_use]
    pub fn get_last_assistant_text(&self) -> Option<String> {
        let messages = self.agent.transcript();
        for message in messages.into_iter().rev() {
            if message.role() != "assistant" {
                continue;
            }
            let Some(Message::Assistant(assistant)) = message.as_llm() else {
                continue;
            };
            if matches!(assistant.stop_reason, StopReason::Aborted) && assistant.content.is_empty()
            {
                continue;
            }
            let text: String = assistant
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return None;
            }
            return Some(trimmed.to_owned());
        }
        None
    }

    /// Set the session display name (sanitized: newlines collapse to spaces).
    ///
    /// Persists a `session_info` entry and emits `session_info_changed`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] on persistence failure.
    pub async fn set_session_name(&self, name: &str) -> Result<(), SessionError> {
        let resolved_name = {
            let mut sm = self.session_manager.lock().await;
            sm.append_session_info(name)?;
            sm.get_session_name()
        };
        self.emit_public(AgentSessionEvent::SessionInfoChanged {
            name: resolved_name.clone(),
        });
        if self.has_extension_handlers("session_info_changed") {
            let runner = self.hooks.runner();
            // Fire-and-forget: extension errors must not break the rename.
            tokio::spawn(async move {
                let _ = runner
                    .emit(AgentSessionEvent::SessionInfoChanged {
                        name: resolved_name,
                    })
                    .await;
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute (`new_leaf_id`, `editor_text`) for a target entry.
fn compute_new_leaf_and_editor_text(
    target_entry: &SessionEntry,
) -> (Option<String>, Option<String>) {
    match target_entry {
        SessionEntry::Message(m) if m.message.role() == "user" => {
            let text = extract_user_message_text(&m.message);
            let text = if text.is_empty() { None } else { Some(text) };
            (m.parent_id.clone(), text)
        }
        SessionEntry::CustomMessage(m) => {
            let text = extract_custom_message_text(&m.content);
            (m.parent_id.clone(), text)
        }
        _ => (target_entry.id().map(str::to_owned), None),
    }
}

/// Extract concatenated text from a user message (mirrors TS).
pub(super) fn extract_user_message_text(message: &AgentMessage) -> String {
    let Some(Message::User(user)) = message.as_llm() else {
        return String::new();
    };
    match &user.content {
        pi_ai::UserMessageContent::Text(s) => s.clone(),
        pi_ai::UserMessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let pi_ai::UserContent::Text(t) = block {
                    out.push_str(&t.text);
                }
            }
            out
        }
    }
}

/// Public wrapper for cross-module access (e.g. `agent_session_runtime::fork`).
#[must_use]
pub fn extract_user_message_text_pub(message: &AgentMessage) -> String {
    extract_user_message_text(message)
}

fn extract_custom_message_text(
    content: &crate::core::messages::CustomMessageContent,
) -> Option<String> {
    use crate::core::messages::CustomMessageContent;
    let text = match content {
        CustomMessageContent::Text(s) => s.clone(),
        CustomMessageContent::Blocks(blocks) => {
            let mut out = String::new();
            for block in blocks {
                if let pi_ai::UserContent::Text(t) = block {
                    out.push_str(&t.text);
                }
            }
            out
        }
    };
    if text.is_empty() { None } else { Some(text) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_session::{AgentSession, AgentSessionConfig};
    use crate::core::sessions::SessionManager;
    use crate::core::settings::SettingsManager;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider,
        ProviderError, StreamOptions, Usage,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn missing(context: &'static str) -> std::io::Error {
        std::io::Error::other(context)
    }

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

    fn assistant_with_usage(text: &str, usage: Usage) -> AssistantMessage {
        let mut message =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        message
            .content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(text)));
        message.stop_reason = pi_ai::StopReason::Stop;
        message.usage = usage;
        message
    }

    fn make_session() -> TestResult<Arc<AgentSession>> {
        let config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        AgentSession::new(config).map_err(Into::into)
    }

    #[tokio::test]
    async fn get_user_messages_for_forking_returns_user_text() -> TestResult {
        let session = make_session()?;
        {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::User(
                pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("hello".into()), 0),
            ))))?;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("hi back", Usage::default()),
            ))))?;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::User(
                pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("second".into()), 1),
            ))))?;
        }
        let messages = session.get_user_messages_for_forking().await;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "hello");
        assert_eq!(messages[1].text, "second");
        Ok(())
    }

    #[tokio::test]
    async fn get_last_assistant_text_skips_aborted_empty() -> TestResult {
        let session = make_session()?;
        let mut aborted = AssistantMessage::new("test-api", "test-provider", "m", 0);
        aborted.stop_reason = pi_ai::StopReason::Aborted;
        let mut good = AssistantMessage::new("test-api", "test-provider", "m", 1);
        good.content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(
                "real text",
            )));
        good.stop_reason = pi_ai::StopReason::Stop;

        session
            .agent
            .push_message(AgentMessage::Llm(Box::new(Message::Assistant(aborted))));
        session
            .agent
            .push_message(AgentMessage::Llm(Box::new(Message::Assistant(good))));

        let text = session.get_last_assistant_text();
        assert_eq!(text.as_deref(), Some("real text"));
        Ok(())
    }

    #[tokio::test]
    async fn get_last_assistant_text_returns_none_when_only_aborted() -> TestResult {
        let session = make_session()?;
        let mut aborted = AssistantMessage::new("test-api", "test-provider", "m", 0);
        aborted.stop_reason = pi_ai::StopReason::Aborted;
        session
            .agent
            .push_message(AgentMessage::Llm(Box::new(Message::Assistant(aborted))));
        let text = session.get_last_assistant_text();
        assert_eq!(text, None);
        Ok(())
    }

    #[tokio::test]
    async fn set_session_name_emits_event_and_persists() -> TestResult {
        let session = make_session()?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
        let _unsub = session.subscribe(move |event| {
            if let AgentSessionEvent::SessionInfoChanged { name } = event {
                let _ = tx.send(name.clone());
            }
        });
        session.set_session_name("My Session").await?;
        let name = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await?;
        let name = name.ok_or_else(|| missing("session name receiver closed"))?;
        assert_eq!(name.as_deref(), Some("My Session"));
        let persisted = session.session_name().await;
        assert_eq!(persisted.as_deref(), Some("My Session"));
        Ok(())
    }

    #[tokio::test]
    async fn set_session_name_collapses_newlines_to_single_spaces() -> TestResult {
        let session = make_session()?;
        session.set_session_name("line1\n\nline2").await?;
        let persisted = session.session_name().await;
        assert_eq!(persisted.as_deref(), Some("line1 line2"));
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_noop_when_already_at_target() -> TestResult {
        let session = make_session()?;
        let id = {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::User(
                pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("x".into()), 0),
            ))))?
        };
        let result = session
            .navigate_tree(
                &id,
                NavigateTreeOptions::default(),
                SummarizationAuth::default(),
                None,
            )
            .await?;
        assert!(!result.cancelled);
        assert!(result.summary_entry.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_unknown_target_errors() -> TestResult {
        let session = make_session()?;
        let result = session
            .navigate_tree(
                "missing",
                NavigateTreeOptions::default(),
                SummarizationAuth::default(),
                None,
            )
            .await;
        assert!(matches!(result, Err(TreeError::EntryNotFound(_))));
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_to_user_message_sets_editor_text_and_leaf_to_parent() -> TestResult {
        let session = make_session()?;
        let user_id = {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::User(
                pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("hello".into()), 0),
            ))))?
        };
        {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("reply", Usage::default()),
            ))))?;
        }
        let result = session
            .navigate_tree(
                &user_id,
                NavigateTreeOptions::default(),
                SummarizationAuth::default(),
                None,
            )
            .await?;
        assert_eq!(result.editor_text.as_deref(), Some("hello"));
        let leaf = {
            let sm = session.session_manager.lock().await;
            sm.get_leaf_id().map(str::to_owned)
        };
        assert!(
            leaf.is_none(),
            "expected null leaf after navigating to root user, got {leaf:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_to_assistant_sets_leaf_to_target() -> TestResult {
        let session = make_session()?;
        let (id1, _id2) = {
            let mut sm = session.session_manager.lock().await;
            let a = sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("first", Usage::default()),
            ))))?;
            let b = sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("second", Usage::default()),
            ))))?;
            (a, b)
        };
        let result = session
            .navigate_tree(
                &id1,
                NavigateTreeOptions::default(),
                SummarizationAuth::default(),
                None,
            )
            .await?;
        assert!(result.editor_text.is_none());
        let leaf = {
            let sm = session.session_manager.lock().await;
            sm.get_leaf_id().map(str::to_owned)
        };
        assert_eq!(leaf.as_deref(), Some(id1.as_str()));
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_attaches_label_to_target_when_no_summary() -> TestResult {
        let session = make_session()?;
        let id = {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("hi", Usage::default()),
            ))))?
        };
        session
            .navigate_tree(
                &id,
                NavigateTreeOptions {
                    label: Some("bookmark".into()),
                    ..Default::default()
                },
                SummarizationAuth::default(),
                None,
            )
            .await?;
        let label = {
            let sm = session.session_manager.lock().await;
            sm.get_label(&id).map(str::to_owned)
        };
        assert_eq!(label.as_deref(), Some("bookmark"));
        Ok(())
    }

    #[tokio::test]
    async fn navigate_tree_summarize_requires_model() -> TestResult {
        let session = make_session()?;
        let target = {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("first", Usage::default()),
            ))))?
        };
        {
            let mut sm = session.session_manager.lock().await;
            sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("second", Usage::default()),
            ))))?;
        }
        let mut empty_model = test_model();
        empty_model.id = String::new();
        session.agent.set_model(empty_model);
        let result = session
            .navigate_tree(
                &target,
                NavigateTreeOptions {
                    summarize: true,
                    ..Default::default()
                },
                SummarizationAuth::default(),
                None,
            )
            .await;
        assert!(matches!(result, Err(TreeError::NoModel)), "got {result:?}");
        Ok(())
    }

    #[tokio::test]
    async fn session_abort_cancels_branch_summary() -> TestResult {
        let session = make_session()?;
        let target = {
            let mut manager = session.session_manager.lock().await;
            let target = manager.append_message(&AgentMessage::Llm(Box::new(
                Message::Assistant(assistant_with_usage("first", Usage::default())),
            )))?;
            manager.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(
                assistant_with_usage("second", Usage::default()),
            ))))?;
            target
        };
        let summarizer: SummarizeStreamFn = Arc::new(move |_model, _context, options| {
            let signal = options.signal.clone();
            Box::pin(async move {
                if let Some(signal) = signal {
                    signal.cancelled().await;
                }
                let mut message = AssistantMessage::new("test-api", "test-provider", "m", 1);
                message.stop_reason = pi_ai::StopReason::Stop;
                let events = stream::iter(vec![Ok(AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::Stop,
                    message,
                })]);
                Box::pin(events)
                    as std::pin::Pin<
                        Box<
                            dyn futures::Stream<Item = Result<AssistantMessageEvent, ProviderError>>
                                + Send,
                        >,
                    >
            })
        });
        let navigation = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .navigate_tree(
                        &target,
                        NavigateTreeOptions {
                            summarize: true,
                            ..NavigateTreeOptions::default()
                        },
                        SummarizationAuth::default(),
                        Some(&summarizer),
                    )
                    .await
            }
        });
        for _ in 0..100 {
            if session.is_summarizing() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(session.is_summarizing());

        session.abort().await;
        let joined = tokio::time::timeout(std::time::Duration::from_secs(2), navigation).await?;
        let result = joined?;
        assert!(
            matches!(
                &result,
                Err(TreeError::Summarization(message))
                    if message.eq_ignore_ascii_case("summarization cancelled")
            ),
            "expected cancelled summarization, got {result:?}"
        );
        assert!(!session.is_summarizing());
        Ok(())
    }

    /// Minimal `AgentTool` for export tests.
    struct ExportTestTool;

    impl pi_agent::AgentTool for ExportTestTool {
        fn name(&self) -> &'static str {
            "exportTestTool"
        }
        fn label(&self) -> &'static str {
            "Export Test Tool"
        }
        fn description(&self) -> &'static str {
            "A test tool for export assertions."
        }
        fn parameters(&self) -> &serde_json::Value {
            static EMPTY: std::sync::LazyLock<serde_json::Value> =
                std::sync::LazyLock::new(|| serde_json::Value::Object(serde_json::Map::new()));
            &EMPTY
        }
        fn validate_arguments(
            &self,
            args: &serde_json::Map<String, serde_json::Value>,
        ) -> std::result::Result<serde_json::Map<String, serde_json::Value>, pi_agent::ToolError>
        {
            Ok(args.clone())
        }
        fn execute(
            &self,
            _tool_call_id: &str,
            _args: serde_json::Map<String, serde_json::Value>,
            _cancel: tokio_util::sync::CancellationToken,
            _updates: pi_agent::ToolUpdates,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = std::result::Result<
                            pi_agent::AgentToolResult,
                            pi_agent::ToolError,
                        >,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(pi_agent::AgentToolResult::default()) })
        }
    }

    fn export_test_session(cwd: &str) -> TestResult<Arc<AgentSession>> {
        use crate::core::settings::{Settings, SettingsManagerCreateOptions};

        let session_manager = SessionManager::create(cwd, None, None)?;
        let mut settings_manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions {
                project_trusted: true,
            },
        );
        settings_manager.set_theme("dark");

        let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
        config.session_manager = session_manager;
        config.settings_manager = settings_manager;
        config.system_prompt = "EXPORTED SYSTEM PROMPT".into();
        config.cwd = cwd.to_owned();
        let session = AgentSession::new(config)?;
        session.agent.set_tools(vec![
            Arc::new(ExportTestTool) as Arc<dyn pi_agent::AgentTool>
        ]);
        session
            .agent
            .set_system_prompt("EXPORTED SYSTEM PROMPT".into());
        Ok(session)
    }

    async fn append_export_messages(session: &AgentSession) -> TestResult {
        let mut sm = session.session_manager.lock().await;
        sm.append_message(&AgentMessage::Llm(Box::new(Message::User(
            pi_ai::UserMessage::new(pi_ai::UserMessageContent::Text("hi".into()), 0),
        ))))?;
        let mut assistant =
            AssistantMessage::new("test-api", "test-provider", "m", pi_agent::now_millis());
        assistant
            .content
            .push(AssistantContent::ToolCall(pi_ai::ToolCall::new(
                "tc-1",
                "customTool",
                serde_json::Map::new(),
            )));
        assistant.stop_reason = pi_ai::StopReason::Stop;
        sm.append_message(&AgentMessage::Llm(Box::new(Message::Assistant(assistant))))?;
        Ok(())
    }

    fn export_tool_renderer() -> ToolHtmlPreRenderer {
        use crate::core::export_html::RenderedToolHtml;

        Arc::new(|_id: String, name: String, _args: serde_json::Value| {
            Box::pin(async move {
                if name == "customTool" {
                    Some(RenderedToolHtml {
                        call_html: Some("<div class='custom-tool'>RENDERED_HTML</div>".into()),
                        result_html_collapsed: None,
                        result_html_expanded: None,
                    })
                } else {
                    None
                }
            })
        })
    }

    fn decode_export_data(html: &str) -> TestResult<serde_json::Value> {
        use base64::Engine as _;

        let marker = "<script id=\"session-data\" type=\"application/json\">";
        let start = html
            .find(marker)
            .ok_or_else(|| missing("session-data script marker"))?
            + marker.len();
        let end = html[start..]
            .find("</script>")
            .ok_or_else(|| missing("session-data terminator"))?
            + start;
        let decoded = base64::engine::general_purpose::STANDARD.decode(html[start..end].trim())?;
        serde_json::from_slice(&decoded).map_err(Into::into)
    }

    fn assert_export_data(data: &serde_json::Value) -> TestResult {
        assert_eq!(
            data["systemPrompt"].as_str(),
            Some("EXPORTED SYSTEM PROMPT"),
            "systemPrompt should be embedded from agent state"
        );
        let tools = data
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| missing("tools array should be present"))?;
        assert!(
            !tools.is_empty(),
            "tools should be non-empty from agent state"
        );
        assert!(
            tools.iter().any(|tool| tool["name"] == "exportTestTool"),
            "tools should contain exportTestTool"
        );
        let rendered = data
            .get("renderedTools")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| missing("renderedTools should be present"))?;
        assert!(
            rendered.contains_key("tc-1"),
            "renderedTools should contain tc-1, got keys: {:?}",
            rendered.keys().collect::<Vec<_>>()
        );
        let call_html = rendered["tc-1"]["callHtml"]
            .as_str()
            .ok_or_else(|| missing("callHtml should be present"))?;
        assert!(
            call_html.contains("RENDERED_HTML"),
            "callHtml should contain rendered content"
        );
        Ok(())
    }

    #[tokio::test]
    async fn export_to_html_embeds_state_tools_theme_rendered_tools() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cwd = tmp.path().to_string_lossy().into_owned();
        let session = export_test_session(&cwd)?;
        append_export_messages(&session).await?;
        let output_path = tmp.path().join("session.html");
        let output_path = output_path
            .to_str()
            .ok_or_else(|| missing("temporary export path should be UTF-8"))?;

        let output = session
            .export_to_html(Some(output_path), Some(export_tool_renderer()))
            .await?;
        let html = std::fs::read_to_string(output)?;
        assert!(
            !html.contains("{{SESSION_DATA}}"),
            "template placeholder unfilled"
        );
        let data = decode_export_data(&html)?;
        assert_export_data(&data)?;
        Ok(())
    }

    #[tokio::test]
    async fn export_to_html_uses_m3_light_and_auto_dark_settings() -> TestResult {
        use crate::core::settings::ThemeMode;

        let tmp = tempfile::tempdir()?;
        let cwd = tmp.path().to_string_lossy().into_owned();

        // m3-light pin
        {
            let session = export_test_session(&cwd)?;
            {
                let mut settings = session.lock_settings();
                settings.set_theme("m3-light");
                settings.set_theme_mode(ThemeMode::Light);
            }
            append_export_messages(&session).await?;
            let output_path = tmp.path().join("m3.html");
            let output_path = output_path
                .to_str()
                .ok_or_else(|| missing("temporary export path should be UTF-8"))?;
            let output = session.export_to_html(Some(output_path), None).await?;
            let html = std::fs::read_to_string(output)?;
            assert!(
                html.contains("--accent: #6750a4;"),
                "settings theme m3-light should export m3-light CSS vars"
            );
        }

        // dark + auto headless → default dark
        {
            let session = export_test_session(&cwd)?;
            {
                let mut settings = session.lock_settings();
                settings.set_theme("dark");
                settings.set_theme_mode(ThemeMode::Auto);
            }
            append_export_messages(&session).await?;
            let output_path = tmp.path().join("auto-dark.html");
            let output_path = output_path
                .to_str()
                .ok_or_else(|| missing("temporary export path should be UTF-8"))?;
            let output = session.export_to_html(Some(output_path), None).await?;
            let html = std::fs::read_to_string(output)?;
            assert!(
                html.contains("--accent: #50a8ff;"),
                "theme=dark themeMode=auto headless should export default dark"
            );
            assert!(
                html.contains("--exportPageBg: #18181e;"),
                "default dark export page background should remain explicit"
            );
        }

        Ok(())
    }
}
