use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::{EntryId, PendingAssistantMessage, SessionError};
use crate::context::Context;
use crate::message::AgentMessage;

/// Identity and ordering header shared by every durable entry kind.
///
/// These are the only fields every backend indexes on; the variant-specific
/// payload lives in [`Entry`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EntryBase {
    /// Session-unique entry identifier. A tool call's `invocationId` is this
    /// same id, so the two namespaces are intentionally one namespace.
    pub id: EntryId,
    /// Predecessor in the branch ancestry. `None` is a normal root entry, not
    /// a dangling reference; a `Some` id that was never committed is an
    /// invariant violation rejected at commit time.
    #[serde(rename = "parentId")]
    pub parent_id: Option<EntryId>,
    /// Storage-assigned sequence number, unique and strictly increasing across
    /// the whole session (backends start at 1). Ordering within a branch must
    /// use this, never [`Self::timestamp`], which is only millisecond-granular
    /// and may tie or move with the wall clock.
    pub seq: u64,
    /// Wall-clock time the backend stamped the commit that wrote this entry,
    /// in Unix epoch milliseconds. Assigned per commit, so entries from one
    /// batch share a value.
    pub timestamp: i64,
    /// Optional application discriminator carried alongside a non-`Custom`
    /// entry. Absence is the normal case. A [`Entry::Custom`] entry carries
    /// its own required `custom_type`, which takes precedence over this field.
    #[serde(rename = "customType", skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
}

/// Committed durable entry: one node of a branch's ancestry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// Tag `"message"` — a conversational message in the transcript.
    Message {
        /// Shared identity, parent link, sequence and timestamp.
        #[serde(flatten)]
        base: EntryBase,
        /// The message as replayed into provider context.
        message: AgentMessage,
        /// Tool-result signal that the run must stop here instead of
        /// requesting another assistant turn. Serialized only when set.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        terminate: bool,
    },
    /// Tag `"compaction"` — replacement of older transcript history with a
    /// summary, written by a summary operation.
    Compaction {
        /// Shared identity, parent link, sequence and timestamp.
        #[serde(flatten)]
        base: EntryBase,
        /// Summary text substituted for the compacted prefix.
        summary: String,
        /// Messages kept verbatim after the summary instead of being folded
        /// into it.
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        /// Approximate token count of the transcript before compaction, used
        /// to decide whether a further compaction is warranted.
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// Opaque producer-specific detail payload; never interpreted by
        /// storage.
        details: Option<serde_json::Value>,
        /// Token usage spent producing this summary, when the producing
        /// request reported any.
        usage: Option<pi_ai::Usage>,
        /// Whether a hook (rather than the runtime) authored this summary.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Tag `"branch_summary"` — summary of the abandoned side of a navigation.
    BranchSummary {
        /// Shared identity, parent link, sequence and timestamp.
        #[serde(flatten)]
        base: EntryBase,
        /// Entry the summarized branch diverged from. `None` means the summary
        /// covers the whole branch from its root.
        #[serde(rename = "fromId")]
        from_id: Option<EntryId>,
        /// Summary text for the abandoned side.
        summary: String,
        /// Opaque producer-specific detail payload; never interpreted by
        /// storage.
        details: Option<serde_json::Value>,
        /// Token usage spent producing this summary, when reported.
        usage: Option<pi_ai::Usage>,
        /// Whether a hook (rather than the runtime) authored this summary.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Tag `"custom"` — application-owned entry whose meaning is defined by
    /// [`Entry::custom_type`] and, optionally, an [`EntryProjector`].
    Custom {
        /// Shared identity, parent link, sequence and timestamp.
        #[serde(flatten)]
        base: EntryBase,
        /// Required discriminator selecting how this entry is projected into
        /// model context. Unknown types contribute nothing.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Arbitrary application payload; `None` is a marker entry with no
        /// data.
        data: Option<serde_json::Value>,
    },
}

/// Serialized discriminator for [`Entry`], used as a scan filter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    /// Tag `"message"`.
    Message,
    /// Tag `"compaction"`.
    Compaction,
    /// Tag `"branch_summary"`.
    BranchSummary,
    /// Tag `"custom"`.
    Custom,
}

impl Entry {
    /// Returns the shared identity/ordering header for any variant.
    #[must_use]
    pub fn base(&self) -> &EntryBase {
        match self {
            Self::Message { base, .. }
            | Self::Compaction { base, .. }
            | Self::BranchSummary { base, .. }
            | Self::Custom { base, .. } => base,
        }
    }
    /// Returns the session-unique id of this entry.
    #[must_use]
    pub fn id(&self) -> &EntryId {
        &self.base().id
    }
    /// Returns the predecessor id, or `None` when this entry is a branch root.
    #[must_use]
    pub fn parent_id(&self) -> Option<&EntryId> {
        self.base().parent_id.as_ref()
    }
    /// Returns the storage-assigned sequence number.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.base().seq
    }
    /// Returns the commit timestamp in Unix epoch milliseconds.
    #[must_use]
    pub fn timestamp(&self) -> i64 {
        self.base().timestamp
    }
    /// Returns the variant discriminator.
    #[must_use]
    pub fn entry_type(&self) -> EntryType {
        match self {
            Self::Message { .. } => EntryType::Message,
            Self::Compaction { .. } => EntryType::Compaction,
            Self::BranchSummary { .. } => EntryType::BranchSummary,
            Self::Custom { .. } => EntryType::Custom,
        }
    }
    /// Returns the conversational message, or `None` for summary and custom
    /// entries.
    #[must_use]
    pub fn message(&self) -> Option<&AgentMessage> {
        match self {
            Self::Message { message, .. } => Some(message),
            _ => None,
        }
    }
    /// Returns the effective application discriminator: the required type of a
    /// `Custom` entry, otherwise the optional base `custom_type`.
    #[must_use]
    pub fn custom_type(&self) -> Option<&str> {
        match self {
            Self::Custom { custom_type, .. } => Some(custom_type.as_str()),
            _ => self.base().custom_type.as_deref(),
        }
    }
}

/// Entry submitted to a transaction before storage assigns `seq` and
/// `timestamp` (candidate `NewEntry`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NewEntry {
    /// Caller-resolved session-unique id; storage never reassigns it.
    pub id: EntryId,
    /// Predecessor id, or `None` for a root. Validated against committed and
    /// earlier-in-batch entry ids before the write is accepted.
    pub parent_id: Option<EntryId>,
    /// Payload carried unchanged into the committed entry.
    pub body: NewEntryBody,
}

/// Payload half of a [`NewEntry`], mirroring [`Entry`] without the
/// storage-assigned sequence or timestamp.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NewEntryBody {
    /// Tag `"message"`.
    Message {
        /// Message to persist.
        message: AgentMessage,
        /// Termination signal copied onto the committed entry.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        terminate: bool,
    },
    /// Tag `"compaction"`.
    Compaction {
        /// Summary text substituted for the compacted prefix.
        summary: String,
        /// Messages kept verbatim after the summary.
        #[serde(rename = "retainedTail")]
        retained_tail: Vec<AgentMessage>,
        /// Approximate pre-compaction token count.
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// Opaque producer-specific detail payload.
        details: Option<serde_json::Value>,
        /// Token usage spent producing the summary, when reported.
        usage: Option<pi_ai::Usage>,
        /// Whether a hook authored the summary.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Tag `"branch_summary"`.
    BranchSummary {
        /// Divergence entry, or `None` for a whole-branch summary.
        #[serde(rename = "fromId")]
        from_id: Option<EntryId>,
        /// Summary text for the abandoned side.
        summary: String,
        /// Opaque producer-specific detail payload.
        details: Option<serde_json::Value>,
        /// Token usage spent producing the summary, when reported.
        usage: Option<pi_ai::Usage>,
        /// Whether a hook authored the summary.
        #[serde(rename = "fromHook")]
        from_hook: bool,
    },
    /// Tag `"custom"`.
    Custom {
        /// Application discriminator for projector lookup.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Arbitrary application payload; `None` is a marker entry.
        data: Option<serde_json::Value>,
    },
}

impl NewEntry {
    /// Fills in the backend-assigned `seq` and commit `timestamp`, producing
    /// the committed form. Callers never choose these values themselves.
    #[must_use]
    pub fn materialize(self, seq: u64, timestamp: i64) -> Entry {
        let Self {
            id,
            parent_id,
            body,
        } = self;
        let base = EntryBase {
            id,
            parent_id,
            seq,
            timestamp,
            custom_type: None,
        };
        match body {
            NewEntryBody::Message { message, terminate } => Entry::Message {
                base,
                message,
                terminate,
            },
            NewEntryBody::Compaction {
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
                from_hook,
            } => Entry::Compaction {
                base,
                summary,
                retained_tail,
                tokens_before,
                details,
                usage,
                from_hook,
            },
            NewEntryBody::BranchSummary {
                from_id,
                summary,
                details,
                usage,
                from_hook,
            } => Entry::BranchSummary {
                base,
                from_id,
                summary,
                details,
                usage,
                from_hook,
            },
            NewEntryBody::Custom { custom_type, data } => Entry::Custom {
                base,
                custom_type,
                data,
            },
        }
    }
}

/// Converts an application-defined custom entry into model context.
///
/// `Ok(None)` means the entry contributes no messages, which is the ordinary
/// answer for a type with no projector installed. The [`Context`] carries the
/// caller's cancellation token; a cancelled projection must surface as an
/// error or a dropped future, never as a fabricated message list.
pub type EntryProjector = Arc<
    dyn Fn(Entry, Context) -> BoxFuture<'static, Result<Option<Vec<AgentMessage>>, SessionError>>
        + Send
        + Sync,
>;

/// Assistant message whose `stop_reason` is terminal, i.e. not
/// `StopReason::Pending`.
///
/// This is the type-level form of the pending-message rejection enforced at
/// both append and commit time.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SettledAssistantMessage(pi_ai::AssistantMessage);

impl SettledAssistantMessage {
    /// Rejects a partial-stream assistant message before it reaches durable storage.
    /// `Deferred` is a terminal state; only `Pending` is non-settled.
    ///
    /// # Errors
    ///
    /// Returns [`PendingAssistantMessage`] when the message's stop reason is
    /// still [`pi_ai::StopReason::Pending`].
    pub fn new(message: pi_ai::AssistantMessage) -> Result<Self, PendingAssistantMessage> {
        if matches!(message.stop_reason, pi_ai::StopReason::Pending) {
            Err(PendingAssistantMessage)
        } else {
            Ok(Self(message))
        }
    }
    /// Borrows the settled message.
    #[must_use]
    pub fn get(&self) -> &pi_ai::AssistantMessage {
        &self.0
    }
    /// Consumes the wrapper and returns the inner message.
    #[must_use]
    pub fn into_inner(self) -> pi_ai::AssistantMessage {
        self.0
    }
}
