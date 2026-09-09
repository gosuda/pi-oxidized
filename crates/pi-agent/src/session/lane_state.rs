use serde::{Deserialize, Serialize};

use super::{EntryId, OperationId};
use crate::message::AgentMessage;

/// Model identity persisted in lane configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelIdentity {
    /// Provider key that resolves the model, not a display name.
    pub provider: String,
    /// Provider-scoped model identifier.
    #[serde(rename = "modelId")]
    pub model_id: String,
    /// API shape used for the request, when captured by a newer operation.
    ///
    /// `None` is retained for records written before API capture was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// Lane settings captured at reservation time so a resumed step replays under
/// the model and tool set it started with, not the lane's current ones.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaneConfiguration {
    /// Model this lane's generations were issued against.
    pub model: ModelIdentity,
    /// Reasoning effort requested from the provider.
    pub thinking_level: pi_ai::ModelThinkingLevel,
    /// Names of the tools offered to the model. An empty list is a normal
    /// tool-less lane, distinct from a configuration that was never captured.
    #[serde(rename = "activeToolNames")]
    pub active_tool_names: Vec<String>,
}

/// Durable per-lane bookkeeping: which operation owns the lane and what is
/// queued for it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct LaneState {
    /// Operation currently driving this lane. `None` is the ordinary idle lane,
    /// not a lost reservation.
    #[serde(rename = "currentOperationId")]
    pub current_operation_id: Option<OperationId>,
    /// Most recently terminated operation, kept for reporting. `None` before
    /// the lane's first operation has finished.
    #[serde(rename = "lastOperationId")]
    pub last_operation_id: Option<OperationId>,
    /// Queued messages awaiting a drain point, in arrival order. Empty is the
    /// normal state.
    pub inbox: Vec<InboxItem>,
}

/// One queued message: its committed entry plus how the driver should treat it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InboxItem {
    /// Entry holding the queued payload.
    #[serde(rename = "entryId")]
    pub entry_id: EntryId,
    /// Drain class of the queued entry.
    pub kind: InboxItemKind,
}

/// How a queued entry is allowed to enter the running conversation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InboxItemKind {
    /// Serialized literal `"steer"` — injected at the next drain point of the
    /// current run.
    Steer,
    /// Serialized literal `"followUp"` — injected only when nothing else
    /// triggers a turn.
    FollowUp,
    /// Serialized literal `"nextRun"` — held back until a later run starts.
    NextRun,
    /// Serialized literal `"write"` — a custom (non-message) payload queued for
    /// durable writing rather than for model context.
    Write,
}

/// Entry payload staged under its reserved id before it joins a branch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PendingEntry {
    /// Tag `"message"` — a message payload not yet appended to the branch.
    Message {
        /// Staged message.
        payload: AgentMessage,
    },
    /// Tag `"custom"` — an application-defined payload not yet appended.
    Custom {
        /// Application discriminator the eventual custom entry will carry.
        custom_type: String,
        /// Staged payload; `None` is a marker with no data.
        payload: Option<serde_json::Value>,
    },
}
