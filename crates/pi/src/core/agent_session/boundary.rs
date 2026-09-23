use std::sync::Arc;

use pi_agent::{AgentLoopError, AgentTurnContext, AgentTurnDecision};
use pi_ai::{Message, StopReason};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AgentSession;
use super::events::AgentSessionEvent;
use super::extension_runner::{BoundaryPreview, ExtensionRunnerError};
use crate::core::compaction::estimate_context_tokens;
use crate::core::messages::{CustomMessageContent, convert_to_llm};
use crate::core::sessions::{ContextEditReplacement, SessionEntry, SessionError, SessionManager};

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum BoundaryDraft {
    Custom {
        custom_type: String,
        #[serde(default, deserialize_with = "present_value")]
        data: Option<Value>,
    },
    CustomMessage {
        custom_type: String,
        content: CustomMessageContent,
        display: bool,
        #[serde(default, deserialize_with = "present_value")]
        details: Option<Value>,
    },
    ContextEdit {
        target_id: String,
        #[serde(deserialize_with = "Option::deserialize")]
        replacement: Option<ContextEditReplacement>,
    },
    Compaction {
        summary: String,
        #[serde(deserialize_with = "Option::deserialize")]
        first_kept_entry_id: Option<String>,
        #[serde(default, deserialize_with = "present_value")]
        details: Option<Value>,
        #[serde(default)]
        usage: Option<pi_ai::Usage>,
    },
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn append_draft(
    manager: &mut SessionManager,
    draft: BoundaryDraft,
) -> Result<String, SessionError> {
    match draft {
        BoundaryDraft::Custom { custom_type, data } => {
            manager.append_custom_entry(&custom_type, data)
        }
        BoundaryDraft::CustomMessage {
            custom_type,
            content,
            display,
            details,
        } => manager.append_custom_message_entry(&custom_type, &content, display, details),
        BoundaryDraft::ContextEdit {
            target_id,
            replacement,
        } => manager.append_context_edit(&target_id, replacement),
        BoundaryDraft::Compaction {
            summary,
            first_kept_entry_id,
            details,
            usage,
        } => {
            let context = manager.build_session_context()?;
            let tokens = estimate_context_tokens(&context.messages).tokens;
            manager.append_compaction(
                &summary,
                first_kept_entry_id.as_deref(),
                i64::try_from(tokens).unwrap_or(i64::MAX),
                details,
                Some(true),
                usage,
            )
        }
    }
}

fn preview_drafts(
    manager: &SessionManager,
    entries: &[Value],
) -> Result<SessionManager, SessionError> {
    let mut preview = manager.preview_branch()?;
    for entry in entries {
        append_draft(&mut preview, serde_json::from_value(entry.clone())?)?;
    }
    preview.build_session_context()?;
    Ok(preview)
}

impl AgentSession {
    pub(crate) async fn preview_boundary(
        self: &Arc<Self>,
        boundary: &str,
        entries: Vec<Value>,
    ) -> Result<Value, SessionError> {
        if !matches!(boundary, "turn_end" | "agent_before_settle") {
            return Err(SessionError::ContextEditInvalid(format!(
                "Unknown boundary: {boundary}"
            )));
        }
        let preview = {
            let manager = self.session_manager.lock().await;
            preview_drafts(&manager, &entries)?
        };
        let projection = preview.build_session_projection()?;
        let context_entries = serde_json::to_value(&projection.entries)?;
        let messages = projection.into_context().messages;
        let llm_messages = convert_to_llm(&messages)?;
        let assistant_tail = matches!(llm_messages.last(), Some(Message::Assistant(_)));
        let context_can_continue = llm_messages
            .iter()
            .any(|message| !matches!(message, Message::System(_)))
            && !assistant_tail;
        let pending_custom = self.lock_inner().pending_custom_messages.clone();
        let has_pending_custom = !pending_custom.is_empty();
        let mut pending_messages = self.agent.peek_queued_messages();
        pending_messages.extend(pending_custom);
        let can_continue = context_can_continue
            || has_pending_custom
            || (self.agent.has_queued_messages() && (boundary == "turn_end" || assistant_tail));
        Ok(json!({
            "contextEntries": context_entries,
            "contextMessages": messages,
            "llmMessages": llm_messages,
            "pendingMessages": pending_messages,
            "canContinue": can_continue,
        }))
    }

    async fn commit_boundary_drafts(&self, entries: Vec<Value>) -> Result<(), SessionError> {
        let mut manager = Arc::clone(&self.session_manager).lock_owned().await;
        let (appended, messages) = tokio::task::spawn_blocking(move || {
            preview_drafts(&manager, &entries)?;
            let mut appended = Vec::with_capacity(entries.len());
            for entry in entries {
                let id = append_draft(&mut manager, serde_json::from_value(entry)?)?;
                let entry = manager
                    .get_entry(&id)
                    .ok_or(SessionError::EntryNotFound(id))?;
                appended.push(entry.clone());
            }
            Ok::<_, SessionError>((appended, manager.build_session_context()?.messages))
        })
        .await
        .map_err(|error| SessionError::Io {
            path: "boundary persistence worker".to_owned(),
            source: std::io::Error::other(error),
        })??;
        self.agent.replace_messages(messages);
        for entry in appended {
            self.emit_public(AgentSessionEvent::EntryAppended { entry });
        }
        Ok(())
    }

    async fn dispatch_boundary(
        self: &Arc<Self>,
        boundary: &'static str,
        mut payload: Value,
    ) -> Result<bool, SessionError> {
        let runner = self.hooks.runner();
        payload["entries"] = json!([]);
        payload["continue"] = Value::Bool(false);
        payload["context"] = self.preview_boundary(boundary, Vec::new()).await?;
        let session = Arc::clone(self);
        let preview: BoundaryPreview = Arc::new(move |entries| {
            let session = Arc::clone(&session);
            Box::pin(async move {
                session
                    .preview_boundary(boundary, entries)
                    .await
                    .map_err(|error| ExtensionRunnerError::Failed(error.to_string()))
            })
        });
        let result = match runner.emit_boundary(boundary, payload, preview).await {
            Ok(result) => result.unwrap_or_default(),
            Err(error) => {
                runner.emit_error(error.to_string());
                return Ok(false);
            }
        };
        let entries = result.entries.unwrap_or_default();
        if let Err(error) = self.preview_boundary(boundary, entries.clone()).await {
            runner.emit_error(format!("Invalid boundary entries: {error}"));
            return Ok(false);
        }
        self.commit_boundary_drafts(entries).await?;
        if result.continue_after != Some(true) {
            return Ok(false);
        }
        let context = self.preview_boundary(boundary, Vec::new()).await?;
        if context["canContinue"] != true {
            runner.emit_error(format!(
                "{boundary} requested continuation without runnable model context"
            ));
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn finish_turn_boundary(
        self: &Arc<Self>,
        turn: AgentTurnContext,
    ) -> Result<Option<AgentTurnDecision>, AgentLoopError> {
        let (outcome, turn_index) = {
            let mut inner = self.lock_inner();
            inner.last_activity_outcome = match turn.message.stop_reason {
                StopReason::Aborted => "aborted",
                StopReason::Error => "error",
                _ => "completed",
            };
            let index = inner.boundary_turn_index;
            inner.boundary_turn_index += 1;
            (inner.last_activity_outcome, index)
        };
        let runner = self.hooks.runner();
        if !runner.has_handlers("turn_end") {
            return Ok(None);
        }
        if !self
            .wait_for_processed_messages(turn.new_messages.len())
            .await
        {
            return Ok(None);
        }
        let (message_entry_id, tool_result_entry_ids) = {
            let manager = self.session_manager.lock().await;
            persisted_turn_ids(&manager, &turn.tool_results)
        };
        let Some(message_entry_id) = message_entry_id else {
            runner.emit_error(
                "turn_end could not resolve the persisted assistant entry ID".to_owned(),
            );
            return Ok(None);
        };
        let payload = json!({
            "type": "turn_end",
            "turnIndex": turn_index,
            "message": turn.message,
            "toolResults": turn.tool_results,
            "messageEntryId": message_entry_id,
            "toolResultEntryIds": tool_result_entry_ids,
            "outcome": outcome,
        });
        match self.dispatch_boundary("turn_end", payload).await {
            Ok(should_continue) => Ok(should_continue.then_some(AgentTurnDecision::Continue)),
            Err(error) => {
                let message = error.to_string();
                self.record_session_error(error);
                self.agent.abort();
                Err(AgentLoopError::message(message))
            }
        }
    }

    pub(super) async fn before_settle_boundary(self: &Arc<Self>) -> Result<bool, SessionError> {
        if self.lock_inner().boundary_abort_requested {
            return Ok(false);
        }
        let runner = self.hooks.runner();
        if !runner.has_handlers("agent_before_settle") {
            return Ok(self.agent.has_queued_messages());
        }
        let outcome = self.lock_inner().last_activity_outcome;
        let requested = self
            .dispatch_boundary(
                "agent_before_settle",
                json!({ "type": "agent_before_settle", "outcome": outcome }),
            )
            .await?;
        self.flush_pending_custom_messages().await?;
        if self.lock_inner().boundary_abort_requested {
            return Ok(false);
        }
        let context = self
            .preview_boundary("agent_before_settle", Vec::new())
            .await?;
        Ok((requested || self.agent.has_queued_messages()) && context["canContinue"] == true)
    }

    pub(super) async fn flush_pending_custom_messages(&self) -> Result<(), SessionError> {
        let pending = std::mem::take(&mut self.lock_inner().pending_custom_messages);
        for message in pending {
            self.persist_message_end(&message).await?;
            self.agent.push_message(message.clone());
            self.emit_public(AgentSessionEvent::MessageStart {
                message: message.clone(),
            });
            self.emit_public(AgentSessionEvent::MessageEnd { message });
        }
        Ok(())
    }

    pub(super) fn begin_agent_run(&self) {
        let mut inner = self.lock_inner();
        inner.run_message_baseline = inner.processed_message_ends;
    }

    async fn wait_for_processed_messages(&self, count: usize) -> bool {
        loop {
            let (notified, cancelled) = {
                let inner = self.lock_inner();
                if inner.pending_session_error.is_some() {
                    return false;
                }
                if inner.processed_message_ends - inner.run_message_baseline >= count {
                    return true;
                }
                (
                    Arc::clone(&inner.message_end_notify).notified_owned(),
                    inner.agent_end_wait_cancel.clone(),
                )
            };
            tokio::select! {
                biased;
                () = cancelled.cancelled() => return false,
                () = notified => {}
            }
        }
    }
}

fn persisted_turn_ids(
    manager: &SessionManager,
    results: &[pi_ai::ToolResultMessage],
) -> (Option<String>, Vec<String>) {
    let branch = manager.get_branch(None);
    let mut assistant_id = None;
    let mut tool_ids = std::collections::HashMap::with_capacity(results.len());
    for entry in branch.into_iter().rev() {
        let SessionEntry::Message(entry) = entry else {
            continue;
        };
        match entry.message.as_llm() {
            Some(Message::Assistant(_)) => {
                assistant_id = Some(entry.id.clone());
                break;
            }
            Some(Message::ToolResult(result)) => {
                tool_ids
                    .entry(result.tool_call_id.as_str())
                    .or_insert(entry.id.as_str());
            }
            _ => {}
        }
    }
    let ids = results
        .iter()
        .filter_map(|result| {
            tool_ids
                .get(result.tool_call_id.as_str())
                .map(|id| (*id).to_owned())
        })
        .collect();
    (assistant_id, ids)
}
