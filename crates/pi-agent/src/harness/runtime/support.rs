//! Shared runtime state, durable helpers, and conversion boundaries.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::context::Context;
use crate::message::{AgentMessage, default_convert_to_llm, user_text};
use crate::queue::QueueMode;
use crate::session::address::{
    branch_tip, entry_label, lane_config, lane_state, operation_meta as operation_meta_value,
    operation_preparation, operation_result, operation_state, pending_assistant_frames,
    pending_entry,
};
use crate::session::configuration::{CompactionSettings, HarnessRetryPolicy, HarnessStreamOptions};
use crate::session::operation::{
    Control, NormalizedRetryPolicy, Operation, OperationIntent, OperationKind, OperationMeta,
    OperationResultRecord, OperationScope, RunSettings,
};
use crate::session::traits::{Branch, Session, SessionReader, SessionReaderExt};
use crate::session::{
    DurableStructuralPreparation, Entry, EntryId, InboxItem, InboxItemKind, LaneConfiguration,
    LaneName, LaneState, ModelIdentity, NewEntry, NewEntryBody, OperationId, PendingEntry,
    SessionError, UsageId, Write,
};
use crate::tool::ToolExecutionMode;

use crate::harness::api::{HarnessResources, PromptInput, QueueInput};
use crate::harness::result::{HarnessError, HarnessFault, SharedFault};

/// Runtime configuration kept outside durable lane state.
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) model: pi_ai::Model,
    pub(crate) thinking_level: pi_ai::ModelThinkingLevel,
    pub(crate) active_tool_names: Vec<String>,
    pub(crate) tools: Vec<Arc<dyn crate::harness::tool::HarnessTool>>,
    pub(crate) resources: HarnessResources,
    pub(crate) stream_options: HarnessStreamOptions,
    pub(crate) retry: HarnessRetryPolicy,
    pub(crate) compaction: CompactionSettings,
    pub(crate) steering_mode: QueueMode,
    pub(crate) follow_up_mode: QueueMode,
    pub(crate) tool_execution: ToolExecutionMode,
    pub(crate) tool_context: Option<crate::harness::tool::ToolContextSource>,
    pub(crate) system_prompt: Option<crate::harness::api::SystemPromptSource>,
    pub(crate) to_provider_messages: crate::harness::stream::ToProviderMessages,
    pub(crate) entry_projectors: HashMap<String, crate::session::EntryProjector>,
}

/// In-memory copy of the durable fields needed to make a lane decision.
#[derive(Clone)]
pub(crate) struct LaneData {
    pub(crate) tip: Option<EntryId>,
    pub(crate) config: LaneConfiguration,
    pub(crate) state: LaneState,
    pub(crate) operation: Option<Operation>,
    pub(crate) last_result: Option<OperationResultRecord>,
}

impl LaneData {
    pub(crate) fn new(config: LaneConfiguration, tip: Option<EntryId>, state: LaneState) -> Self {
        Self {
            tip,
            config,
            state,
            operation: None,
            last_result: None,
        }
    }
}

/// Convert a storage failure into the public closed/error boundary.
///
/// Storage errors are infrastructure faults, not caller-invalid requests. The
/// lane is sealed by its owner before this value is returned, so a later call
/// cannot accidentally continue from a partially committed view.
/// Takes the error by value so callers map with the fn pointer
/// (`.map_err(map_session_error)`); a reference would force a closure
/// at every call site for no behavior change.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn map_session_error(error: SessionError) -> HarnessError {
    HarnessError::Closed {
        message: format!("session operation failed: {error}"),
    }
}
/// Rejection for an operation against a sealed lane: shares the sealing
/// fault object so every rejection chain stays inspectable to one cause.
pub(crate) fn sealed_rejection(fault: &Arc<HarnessFault>) -> HarnessError {
    HarnessError::FaultSealed {
        message: fault.message.clone(),
        fault: SharedFault(Arc::clone(fault)),
    }
}

pub(crate) fn new_entry_id(session: &dyn Session) -> Result<EntryId, SessionError> {
    session.id_generator().next(None).map(EntryId::from)
}

pub(crate) fn new_operation_id(session: &dyn Session) -> Result<OperationId, SessionError> {
    session.id_generator().next(None).map(OperationId::from)
}

pub(crate) fn new_usage_id(session: &dyn Session) -> Result<UsageId, SessionError> {
    session.id_generator().next(None).map(UsageId::from)
}

pub(crate) fn invalid_message(reason: &str, message: &str) -> HarnessError {
    HarnessError::InvalidMessage {
        lane: LaneName::new(""),
        reason: reason.to_owned(),
        message: message.to_owned(),
    }
}

pub(crate) fn prompt_messages(input: PromptInput) -> Result<Vec<AgentMessage>, HarnessError> {
    match input {
        PromptInput::Text { text, images } => {
            if text.trim().is_empty() && images.is_empty() {
                return Err(invalid_message(
                    "empty_prompt",
                    "prompt text and images cannot both be empty",
                ));
            }
            Ok(vec![user_text(text, images)])
        }
        PromptInput::Messages(messages) => {
            if messages.is_empty() {
                return Err(invalid_message(
                    "empty_prompt",
                    "prompt message list cannot be empty",
                ));
            }
            if messages
                .iter()
                .any(|message| message.role() == "assistant" && assistant_pending(message))
            {
                return Err(invalid_message(
                    "pending_assistant",
                    "pending assistant messages cannot be admitted",
                ));
            }
            Ok(messages)
        }
    }
}

pub(crate) fn queue_message(input: QueueInput) -> Result<AgentMessage, String> {
    match input {
        QueueInput::Text { text, images } => {
            if text.trim().is_empty() && images.is_empty() {
                return Err("queue message cannot be empty".to_owned());
            }
            Ok(user_text(text, images))
        }
        QueueInput::Message(message) => {
            if message.role() == "assistant" && assistant_pending(&message) {
                return Err("pending assistant messages cannot be queued".to_owned());
            }
            Ok(message)
        }
    }
}

pub(crate) fn assistant_pending(message: &AgentMessage) -> bool {
    if message.role() != "assistant" {
        return false;
    }
    serde_json::to_value(message)
        .ok()
        .and_then(|value| {
            value
                .get("stopReason")
                .and_then(Value::as_str)
                .map(|reason| reason == "pending")
        })
        .unwrap_or(false)
}

pub(crate) fn assistant_message(message: &AgentMessage) -> Option<pi_ai::AssistantMessage> {
    if message.role() != "assistant" {
        return None;
    }
    serde_json::to_value(message)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn assistant_agent_message(
    message: pi_ai::AssistantMessage,
) -> Result<AgentMessage, HarnessError> {
    serde_json::to_value(message)
        .map_err(|error| HarnessError::Closed {
            message: format!("assistant message serialization failed: {error}"),
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| HarnessError::Closed {
                message: format!("assistant message conversion failed: {error}"),
            })
        })
}

pub(crate) fn tool_result_agent_message(
    message: &pi_ai::ToolResultMessage,
) -> Result<AgentMessage, HarnessError> {
    serde_json::to_value(message)
        .map_err(|error| HarnessError::Closed {
            message: format!("tool result serialization failed: {error}"),
        })
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| HarnessError::Closed {
                message: format!("tool result conversion failed: {error}"),
            })
        })
}

pub(crate) fn operation_scope(config: &RuntimeConfig) -> OperationScope {
    OperationScope {
        control: Control::Running,
        settings: RunSettings {
            compaction: config.compaction,
            steering_mode: config.steering_mode,
            follow_up_mode: config.follow_up_mode,
            tool_execution: config.tool_execution,
        },
        latest_assistant_entry_id: None,
    }
}

pub(crate) fn captured_configuration(config: &RuntimeConfig) -> LaneConfiguration {
    LaneConfiguration {
        model: ModelIdentity {
            provider: config.model.provider.clone(),
            model_id: config.model.id.clone(),
            api: Some(config.model.api.clone()),
        },
        thinking_level: config.thinking_level,
        active_tool_names: config.active_tool_names.clone(),
    }
}

pub(crate) fn normalized_retry(
    policy: HarnessRetryPolicy,
) -> Result<NormalizedRetryPolicy, HarnessError> {
    NormalizedRetryPolicy::try_from(policy).map_err(|_| HarnessError::InvalidRetryPolicy {
        max_retries: policy.max_retries,
        base_delay_ms: policy.base_delay_ms,
        message: "retry policy cannot be represented safely".to_owned(),
    })
}

pub(crate) fn pending_write(entry: EntryId, kind: InboxItemKind) -> InboxItem {
    InboxItem {
        entry_id: entry,
        kind,
    }
}

pub(crate) fn pending_entry_write(
    entry_id: &EntryId,
    payload: &PendingEntry,
) -> Result<Write, SessionError> {
    let address = pending_entry(entry_id);
    crate::session::set_value(&address, payload)
}

pub(crate) fn entry_write(
    id: EntryId,
    parent_id: Option<EntryId>,
    message: AgentMessage,
    terminate: bool,
) -> Write {
    Write::Entry {
        entry: NewEntry {
            id,
            parent_id,
            body: NewEntryBody::Message { message, terminate },
        },
    }
}

pub(crate) fn custom_entry_write(
    id: EntryId,
    parent_id: Option<EntryId>,
    custom_type: String,
    data: Option<Value>,
) -> Write {
    Write::Entry {
        entry: NewEntry {
            id,
            parent_id,
            body: NewEntryBody::Custom { custom_type, data },
        },
    }
}

pub(crate) fn set_json<T: Serialize>(
    address: &crate::session::Value<T>,
    value: &T,
) -> Result<Write, SessionError> {
    crate::session::set_value(address, value)
}

pub(crate) fn op_cleanup_writes(op: &OperationId, response: Option<&EntryId>) -> Vec<Write> {
    let mut writes = vec![
        crate::session::delete_value(&operation_meta_value(op)),
        crate::session::delete_value(&operation_state(op)),
    ];
    if let Some(response) = response {
        writes.push(crate::session::delete_list(&pending_assistant_frames(
            op, response,
        )));
    }
    writes
}

pub(crate) async fn branch_entries(
    branch: &dyn Branch,
    cx: &Context,
) -> Result<Vec<Entry>, SessionError> {
    let mut entries = branch.find_entries(None, cx).await?;
    entries.reverse();
    Ok(entries)
}

pub(crate) async fn context_messages(
    entries: &[Entry],
    entry_projectors: &HashMap<String, crate::session::EntryProjector>,
    cx: &Context,
) -> Result<Vec<AgentMessage>, SessionError> {
    let mut messages = Vec::new();
    for entry in crate::harness::compaction::build_context_entries(entries) {
        if let Entry::Custom { custom_type, .. } = entry {
            if let Some(projector) = entry_projectors.get(custom_type) {
                messages.extend(
                    projector(entry.clone(), cx.clone())
                        .await?
                        .unwrap_or_default(),
                );
            }
        } else {
            messages.extend(crate::harness::compaction::session_entry_to_context_messages(entry));
        }
    }
    Ok(messages)
}

pub(crate) async fn read_pending(
    reader: &dyn SessionReader,
    items: &[InboxItem],
    cx: &Context,
) -> Result<Vec<(InboxItem, PendingEntry)>, SessionError> {
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        let stored = reader.get_value(&pending_entry(&item.entry_id), cx).await?;
        let Some(stored) = stored else {
            return Err(SessionError::Invariant(format!(
                "missing pending entry {}",
                item.entry_id
            )));
        };
        values.push((item.clone(), stored.value));
    }
    Ok(values)
}

pub(crate) fn lane_branch_tip_address(lane: &LaneName) -> crate::session::Value<Option<EntryId>> {
    branch_tip(lane.as_str())
}

pub(crate) fn default_provider_conversion() -> crate::harness::stream::ToProviderMessages {
    Arc::new(|messages, _cx| Box::pin(async move { Ok(default_convert_to_llm(&messages)) }))
}

pub(crate) fn ensure_lane_name(name: &LaneName) -> Result<(), HarnessError> {
    if name.as_str().is_empty() || name.as_str().contains('\0') {
        return Err(HarnessError::InvalidLane {
            lane: name.clone(),
            reason: "invalid_name".to_owned(),
            message: "lane name must be non-empty and contain no NUL".to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn operation_meta(
    op: &OperationId,
    lane: LaneName,
    source_tip_id: Option<EntryId>,
    started_at: i64,
    intent: OperationIntent,
) -> OperationMeta {
    OperationMeta {
        operation_id: op.clone(),
        lane,
        source_tip_id,
        started_at,
        intent,
    }
}

pub(crate) fn operation_kind(intent: &OperationIntent) -> OperationKind {
    match intent {
        OperationIntent::Run { .. } => OperationKind::Run,
        OperationIntent::Compaction { .. } => OperationKind::Compaction,
        OperationIntent::Navigation { .. } => OperationKind::Navigation,
    }
}

pub(crate) fn response_limit(model: &pi_ai::Model) -> u32 {
    u32::try_from(model.max_tokens).unwrap_or(u32::MAX)
}

pub(crate) fn context_window(model: &pi_ai::Model) -> u32 {
    u32::try_from(model.context_window).unwrap_or(u32::MAX)
}

pub(crate) fn lane_config_address(lane: &LaneName) -> crate::session::Value<LaneConfiguration> {
    lane_config(lane)
}

pub(crate) fn lane_state_address(lane: &LaneName) -> crate::session::Value<LaneState> {
    lane_state(lane)
}

pub(crate) fn result_address(op: &OperationId) -> crate::session::Value<OperationResultRecord> {
    operation_result(op)
}

pub(crate) fn label_address(entry: &EntryId) -> crate::session::Value<String> {
    entry_label(entry)
}

pub(crate) fn preparation_address(
    op: &OperationId,
    task: &str,
) -> crate::session::Value<DurableStructuralPreparation> {
    operation_preparation(op, task)
}

#[cfg(test)]
mod tests {
    use futures::future::BoxFuture;

    use super::*;
    use crate::session::{EntryBase, EntryProjector};

    fn base(id: &str) -> EntryBase {
        EntryBase {
            id: EntryId::new(id),
            parent_id: None,
            seq: 0,
            timestamp: 1,
            custom_type: None,
        }
    }

    fn llm_user_text(message: &AgentMessage) -> Option<&str> {
        let AgentMessage::Llm(message) = message else {
            return None;
        };
        let pi_ai::Message::User(user) = message.as_ref() else {
            return None;
        };
        let pi_ai::UserMessageContent::Text(text) = &user.content else {
            return None;
        };
        Some(text.as_str())
    }

    /// A registered projector runs for custom entries inside the retained
    /// context, contributes its messages in source order, and its failure
    /// aborts the context build.
    #[tokio::test]
    async fn context_messages_projects_registered_custom_entries_and_propagates_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let projector: EntryProjector = Arc::new(|entry: Entry, _cx: Context| {
            let id = entry.id().as_str().to_owned();
            let future: BoxFuture<'static, Result<Option<Vec<AgentMessage>>, SessionError>> =
                Box::pin(async move {
                    Ok(Some(vec![
                        user_text(format!("projected-{id}-first"), []),
                        user_text(format!("projected-{id}-second"), []),
                    ]))
                });
            future
        });
        let mut projectors = HashMap::new();
        projectors.insert("recap".to_owned(), projector);
        let entries = vec![
            Entry::Custom {
                base: base("dropped"),
                custom_type: "recap".to_owned(),
                data: None,
            },
            Entry::Compaction {
                base: base("c"),
                summary: "sum".to_owned(),
                retained_tail: Vec::new(),
                tokens_before: 10,
                details: None,
                usage: None,
                from_hook: false,
            },
            Entry::Custom {
                base: base("kept"),
                custom_type: "recap".to_owned(),
                data: None,
            },
            Entry::Message {
                base: base("tail"),
                message: user_text("after", []),
                terminate: false,
            },
        ];
        let messages = context_messages(&entries, &projectors, &Context::background()).await?;
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), "compactionSummary");
        assert_eq!(llm_user_text(&messages[1]), Some("projected-kept-first"));
        assert_eq!(llm_user_text(&messages[2]), Some("projected-kept-second"));
        assert_eq!(llm_user_text(&messages[3]), Some("after"));

        let failing: EntryProjector = Arc::new(|_entry: Entry, _cx: Context| {
            let future: BoxFuture<'static, Result<Option<Vec<AgentMessage>>, SessionError>> =
                Box::pin(
                    async move { Err(SessionError::Invariant("projector failed".to_owned())) },
                );
            future
        });
        let mut projectors = HashMap::new();
        projectors.insert("recap".to_owned(), failing);
        match context_messages(&entries, &projectors, &Context::background()).await {
            Err(SessionError::Invariant(message)) => {
                assert_eq!(message, "projector failed");
            }
            other => {
                return Err(format!("expected projector failure, got {other:?}").into());
            }
        }
        Ok(())
    }
}
