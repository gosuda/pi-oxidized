use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as JsonValue;

use crate::message::AgentMessage;
use super::address::{Value, ValueList};
use super::{Entry, EntryId, NewEntry, NewEntryBody, SessionError, UsageId};

/// One unit of proposed durable change.
///
/// A commit takes a `Vec<Write>` and applies it as the backend's atomic unit.
/// Entry and usage writes carry caller-chosen ids that must not collide with
/// anything already stored; value and list writes are keyed by namespace and
/// key, so they are idempotent per slot rather than identity-checked.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Write {
    /// Appends a conversation entry to the DAG.
    Entry {
        /// Unmaterialized entry: id, parent link, and body, without the
        /// sequence and timestamp the backend assigns.
        entry: NewEntry,
    },
    /// Records a token/cost measurement.
    Usage {
        /// Usage row without the sequence the backend assigns.
        row: NewUsageRow,
    },
    /// Sets or deletes one value slot.
    Value(ValueWrite),
    /// Appends to or deletes one list slot.
    List(ListWrite),
}

/// Mutation of a single-value slot, addressed by `(namespace, key)`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ValueWrite {
    /// Replaces the slot, creating it when absent.
    Set {
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
        /// New JSON payload.
        value: JsonValue,
    },
    /// Removes the slot; deleting an unset slot is not an error.
    Delete {
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
    },
}

/// Mutation of an append-only list slot, addressed by `(namespace, key)`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ListWrite {
    /// Adds one element to the end of the list.
    Append {
        /// Durable namespace of the list.
        namespace: String,
        /// Key within `namespace`.
        key: String,
        /// JSON payload of the new element.
        value: JsonValue,
    },
    /// Drops the whole list, not a single element.
    Delete {
        /// Durable namespace of the list.
        namespace: String,
        /// Key within `namespace`.
        key: String,
    },
}

/// Builds a typed value-set write.
///
/// # Errors
///
/// [`SessionError::Invariant`] when `next` cannot be serialized to JSON.
pub fn set_value<T: Serialize>(address: &Value<T>, next: &T) -> Result<Write, SessionError> {
    serde_json::to_value(next)
        .map(|value| Write::Value(ValueWrite::Set { namespace: address.namespace().to_owned(), key: address.key().to_owned(), value }))
        .map_err(|error| SessionError::Invariant(format!("value serialization failed: {error}")))
}

/// Builds a value-delete write from a typed address.
#[must_use]
pub fn delete_value<T>(address: &Value<T>) -> Write {
    Write::Value(ValueWrite::Delete { namespace: address.namespace().to_owned(), key: address.key().to_owned() })
}

/// Builds a typed list-append write.
///
/// # Errors
///
/// [`SessionError::Invariant`] when `element` cannot be serialized to JSON.
pub fn append_list<T: Serialize>(address: &ValueList<T>, element: &T) -> Result<Write, SessionError> {
    serde_json::to_value(element)
        .map(|value| Write::List(ListWrite::Append { namespace: address.namespace().to_owned(), key: address.key().to_owned(), value }))
        .map_err(|error| SessionError::Invariant(format!("list element serialization failed: {error}")))
}

/// Builds a list-delete write from a typed address.
#[must_use]
pub fn delete_list<T>(address: &ValueList<T>) -> Write {
    Write::List(ListWrite::Delete { namespace: address.namespace().to_owned(), key: address.key().to_owned() })
}

/// A committed token/cost measurement.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageRow {
    /// Caller-chosen id, unique against every stored entry and usage id.
    pub id: UsageId,
    /// Sequence the backend assigned at commit.
    pub seq: u64,
    /// The measured usage.
    pub usage: pi_ai::Usage,
    /// Entry this measurement is attributed to, when it belongs to one.
    #[serde(rename = "entryId")]
    pub entry_id: Option<EntryId>,
    /// Marks the row as a correction rather than a fresh measurement. The
    /// reference memory backend still adds its usage to the session totals.
    pub adjustment: bool,
    /// Backend-independent provenance payload for display or audit.
    pub details: Option<JsonValue>,
}

/// A proposed usage row, before the backend assigns its sequence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NewUsageRow {
    /// Id the row will be stored under.
    pub id: UsageId,
    /// The measured usage.
    pub usage: pi_ai::Usage,
    /// Entry to attribute the measurement to, when applicable.
    #[serde(rename = "entryId")]
    pub entry_id: Option<EntryId>,
    /// Whether this row is a correction rather than a fresh measurement.
    pub adjustment: bool,
    /// Provenance payload carried through to the stored [`UsageRow`].
    pub details: Option<JsonValue>,
}

/// What a backend reports after admitting a batch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommitResult {
    /// Sequence given to the first write; write *i* took `first_seq + i`.
    #[serde(rename = "firstSeq")]
    pub first_seq: u64,
    /// Assigned sequences, positionally matching the submitted `Vec<Write>`.
    pub seqs: Vec<u64>,
    /// Commit time, milliseconds since the Unix epoch; stamped on every entry
    /// in the batch.
    pub timestamp: i64,
    /// Session totals after the batch was applied.
    pub stats: SessionStats,
}

/// Running session totals maintained by the backend.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SessionStats {
    /// Committed message entries, incremented per `Entry::Message` write.
    #[serde(rename = "messageCount")]
    pub message_count: u64,
    /// Usage totals summed over every committed usage row.
    pub usage: pi_ai::Usage,
}

/// Pre-commit view over committed storage identity.
///
/// `contains_id` is the global identity guard: it covers every stored id,
/// entries and usage rows alike, and enforces cross-kind duplicate rejection.
/// `contains_entry_id` is the narrower entry-only membership used for parent
/// validation: a parent must name a committed *entry*, never a usage row.
pub trait CommittedIdView {
    /// Whether any stored record — entry or usage row — already owns `id`.
    fn contains_id(&self, id: &str) -> bool;
    /// Whether a committed *entry* owns `id`.
    fn contains_entry_id(&self, id: &EntryId) -> bool;
    /// The sequence the next committed write will receive.
    fn next_seq(&self) -> u64;
}

/// The one batch validator every backend shares.
///
/// Backends assign sequences at commit time; this checks a proposed batch
/// against that assignment and a [`CommittedIdView`] of what is already stored,
/// so the rules cannot drift per backend. It is a pre-commit admission check
/// only: it says nothing about whether a backend applies a batch atomically,
/// which stays the backend's own responsibility.
///
/// # Errors
///
/// Returns [`SessionError::Invariant`] when `first_seq` is not
/// `existing.next_seq()`, when a sequence would overflow, when an entry or
/// usage id duplicates a stored id or an earlier write in the same batch, or
/// when an entry's parent is neither stored nor an entry written earlier in
/// the batch (self-parents are rejected, and usage-row ids can never be
/// parents). Returns [`SessionError::PendingAssistantMessage`] for an
/// assistant message whose stop reason is still pending. Value and list
/// writes are always admitted.
pub fn validate_committed_writes(
    writes: &[Write], first_seq: u64, existing: &dyn CommittedIdView,
) -> Result<(), SessionError> {
    if first_seq != existing.next_seq() {
        return Err(SessionError::Invariant(format!("commit starts at sequence {first_seq}, expected {}", existing.next_seq())));
    }
    let mut batch_ids = std::collections::HashSet::<&str>::new();
    let mut batch_entry_ids = std::collections::HashSet::<&str>::new();
    for (index, write) in writes.iter().enumerate() {
        let index = u64::try_from(index).map_err(|_| SessionError::Invariant("sequence overflow".to_owned()))?;
        let _seq = first_seq.checked_add(index).ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
        match write {
            Write::Entry { entry } => {
                let id = entry.id.as_str();
                if existing.contains_id(id) || !batch_ids.insert(id) {
                    return Err(SessionError::Invariant(format!("duplicate entry id {id}")));
                }
                // The parent must name a committed entry or an entry written
                // earlier in this batch. Checking before `id` joins
                // `batch_entry_ids` rejects self-parents; usage-row ids never
                // enter the entry set, so they cannot parent an entry.
                if let Some(parent) = entry.parent_id.as_ref()
                    && !existing.contains_entry_id(parent)
                    && !batch_entry_ids.contains(parent.as_str())
                {
                    return Err(SessionError::Invariant(format!("missing parent id {parent}")));
                }
                batch_entry_ids.insert(id);
                if let NewEntryBody::Message { message: AgentMessage::Llm(llm), .. } = &entry.body
                    && let pi_ai::Message::Assistant(assistant) = llm.as_ref()
                    && matches!(assistant.stop_reason, pi_ai::StopReason::Pending)
                {
                    return Err(SessionError::PendingAssistantMessage);
                }
            }
            Write::Usage { row } => {
                let id = row.id.as_str();
                if existing.contains_id(id) || !batch_ids.insert(id) {
                    return Err(SessionError::Invariant(format!("duplicate usage id {id}")));
                }
            }
            Write::Value(_) | Write::List(_) => {}
        }
    }
    Ok(())
}

/// One write after a backend has assigned its durable sequence.
///
/// The serialized representation is the JSONL wire shape: `kind` identifies
/// the family, while entries and usage rows flatten their committed records
/// into the same object.
#[derive(Clone, Debug, PartialEq)]
pub enum CommittedWrite {
    /// A fully materialized entry.
    Entry(Entry),
    /// A fully materialized usage row.
    Usage(UsageRow),
    /// A value-slot mutation with its assigned sequence.
    Value(CommittedValueWrite),
    /// A list-slot mutation with its assigned sequence.
    List(CommittedListWrite),
}

impl Serialize for CommittedWrite {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (mut value, kind) = match self {
            Self::Entry(entry) => (serde_json::to_value(entry).map_err(S::Error::custom)?, "entry"),
            Self::Usage(row) => (serde_json::to_value(row).map_err(S::Error::custom)?, "usage"),
            Self::Value(write) => (serde_json::to_value(write).map_err(S::Error::custom)?, "value"),
            Self::List(write) => (serde_json::to_value(write).map_err(S::Error::custom)?, "list"),
        };
        let object = value
            .as_object_mut()
            .ok_or_else(|| S::Error::custom("committed write must serialize as an object"))?;
        object.insert("kind".to_owned(), JsonValue::String(kind.to_owned()));
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CommittedWrite {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = JsonValue::deserialize(deserializer)?;
        let kind = {
            let object = value
                .as_object_mut()
                .ok_or_else(|| D::Error::custom("committed write must deserialize from an object"))?;
            object
                .remove("kind")
                .and_then(|kind| kind.as_str().map(str::to_owned))
                .ok_or_else(|| D::Error::custom("committed write is missing string kind"))?
        };
        match kind.as_str() {
            "entry" => serde_json::from_value(value).map(Self::Entry).map_err(D::Error::custom),
            "usage" => serde_json::from_value(value).map(Self::Usage).map_err(D::Error::custom),
            "value" => serde_json::from_value(value).map(Self::Value).map_err(D::Error::custom),
            "list" => serde_json::from_value(value).map(Self::List).map_err(D::Error::custom),
            other => Err(D::Error::custom(format!("unknown committed write kind {other}"))),
        }
    }
}

/// A committed scalar value mutation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CommittedValueWrite {
    /// Stores or replaces one scalar slot.
    Set {
        /// Sequence assigned to this mutation.
        seq: u64,
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
        /// New JSON payload.
        value: JsonValue,
    },
    /// Removes one scalar slot.
    Delete {
        /// Sequence assigned to this mutation.
        seq: u64,
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
    },
}

/// A committed append-only list mutation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CommittedListWrite {
    /// Appends one element to a list slot.
    Append {
        /// Sequence assigned to this mutation.
        seq: u64,
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
        /// Appended JSON payload.
        value: JsonValue,
    },
    /// Removes every element in one list slot.
    Delete {
        /// Sequence assigned to this mutation.
        seq: u64,
        /// Durable namespace of the slot.
        namespace: String,
        /// Key within `namespace`.
        key: String,
    },
}

impl CommittedWrite {
    /// Returns the sequence assigned to this committed write.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Self::Entry(entry) => entry.seq(),
            Self::Usage(row) => row.seq,
            Self::Value(CommittedValueWrite::Set { seq, .. } | CommittedValueWrite::Delete { seq, .. })
            | Self::List(CommittedListWrite::Append { seq, .. } | CommittedListWrite::Delete { seq, .. }) => *seq,
        }
    }
}

/// Materializes a proposed batch with backend-assigned sequence numbers.
///
/// # Errors
///
/// Returns [`SessionError::Invariant`] when sequence assignment would
/// overflow.
pub fn commit_writes(
    writes: Vec<Write>,
    first_seq: u64,
    timestamp: i64,
) -> Result<(Vec<CommittedWrite>, Vec<u64>), SessionError> {
    let mut committed = Vec::with_capacity(writes.len());
    let mut seqs = Vec::with_capacity(writes.len());
    for (index, write) in writes.into_iter().enumerate() {
        let index = u64::try_from(index).map_err(|_| SessionError::Invariant("sequence overflow".to_owned()))?;
        let seq = first_seq
            .checked_add(index)
            .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
        seqs.push(seq);
        let committed_write = match write {
            Write::Entry { entry } => CommittedWrite::Entry(entry.materialize(seq, timestamp)),
            Write::Usage { row } => CommittedWrite::Usage(UsageRow {
                id: row.id,
                seq,
                usage: row.usage,
                entry_id: row.entry_id,
                adjustment: row.adjustment,
                details: row.details,
            }),
            Write::Value(ValueWrite::Set { namespace, key, value }) => {
                CommittedWrite::Value(CommittedValueWrite::Set { seq, namespace, key, value })
            }
            Write::Value(ValueWrite::Delete { namespace, key }) => {
                CommittedWrite::Value(CommittedValueWrite::Delete { seq, namespace, key })
            }
            Write::List(ListWrite::Append { namespace, key, value }) => {
                CommittedWrite::List(CommittedListWrite::Append { seq, namespace, key, value })
            }
            Write::List(ListWrite::Delete { namespace, key }) => {
                CommittedWrite::List(CommittedListWrite::Delete { seq, namespace, key })
            }
        };
        committed.push(committed_write);
    }
    Ok((committed, seqs))
}

/// Validates committed writes replayed from durable storage or a fork.
///
/// Unlike [`validate_committed_writes`], this accepts sequence gaps because
/// materialized entries retain their source sequence numbers. Sequences must
/// still be strictly increasing and cannot precede the destination
/// high-water mark.
///
/// # Errors
///
/// Returns [`SessionError::Invariant`] for a non-monotonic sequence, a
/// duplicate entry/usage identity, or an entry whose parent is not a stored
/// entry or an earlier entry in the same replay batch.
pub fn validate_replayed_writes(
    writes: &[CommittedWrite],
    existing: &dyn CommittedIdView,
) -> Result<(), SessionError> {
    let mut previous_seq = existing.next_seq().saturating_sub(1);
    let mut batch_ids = std::collections::HashSet::<&str>::new();
    let mut batch_entry_ids = std::collections::HashSet::<&str>::new();

    for write in writes {
        let seq = write.seq();
        if seq <= previous_seq {
            return Err(SessionError::Invariant(format!("non-monotonic storage sequence {seq}")));
        }
        previous_seq = seq;
        let (id, parent) = match write {
            CommittedWrite::Entry(entry) => (entry.id().as_str(), entry.parent_id()),
            CommittedWrite::Usage(row) => (row.id.as_str(), None),
            CommittedWrite::Value(_) | CommittedWrite::List(_) => continue,
        };
        if existing.contains_id(id) || !batch_ids.insert(id) {
            return Err(SessionError::Invariant(format!("duplicate entry or usage id {id}")));
        }
        if let Some(parent) = parent
            && !existing.contains_entry_id(parent)
            && !batch_entry_ids.contains(parent.as_str())
        {
            return Err(SessionError::Invariant(format!("missing parent entry {parent}")));
        }
        if matches!(write, CommittedWrite::Entry(_)) {
            batch_entry_ids.insert(id);
        }
    }
    Ok(())
}

