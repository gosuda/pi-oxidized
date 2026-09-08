use serde::{Deserialize, Serialize};

use super::{EntryId, EntryType};
use super::address::AddressKind;

/// Direction a scan walks its sequence space.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanOrder {
    /// `"asc"` — oldest sequence first.
    #[default]
    Asc,
    /// `"desc"` — newest sequence first.
    Desc
}

/// Exclusive sequence boundary for a paginated scan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntryCursor {
    /// Last sequence already observed; results exclude it.
    pub seq: u64
}

/// Branch-ancestry scan as callers request it, with an optional start.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct BranchScan {
    /// Entry to start from. `None` means the branch tip; an empty branch with
    /// no tip yields no results rather than an error.
    pub start: Option<EntryId>,
    /// Stop walking once an entry of this type is reached.
    #[serde(rename = "stopAtType")]
    pub stop_at_type: Option<EntryType>,
    /// Stop walking once this entry is reached.
    #[serde(rename = "stopAtId")]
    pub stop_at_id: Option<EntryId>,
    /// Keep only entries of this type; `None` keeps every type.
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    /// Keep only entries whose effective custom type equals this.
    #[serde(rename = "customType")]
    pub custom_type: Option<String>,
    /// Result order; `None` means the backend's default. Serialized in the
    /// candidate's wire form (`oldestFirst` / `newestFirst`), unlike the plain
    /// `asc` / `desc` spelling other scans use.
    #[serde(with = "branch_order")]
    pub order: Option<ScanOrder>,
    /// Maximum entries returned; `None` uses the backend default, and backends
    /// clamp an oversized value.
    pub limit: Option<u32>,
    /// Exclude entries at or beyond this sequence.
    pub cursor: Option<EntryCursor>,
}

/// [`BranchScan`] with the start resolved, as handed to storage.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StorageBranchScan {
    /// Resolved starting entry; the caller must supply a real id.
    pub start: EntryId,
    /// Stop walking once an entry of this type is reached.
    #[serde(rename = "stopAtType")]
    pub stop_at_type: Option<EntryType>,
    /// Stop walking once this entry is reached.
    #[serde(rename = "stopAtId")]
    pub stop_at_id: Option<EntryId>,
    /// Keep only entries of this type.
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    /// Keep only entries whose effective custom type equals this.
    #[serde(rename = "customType")]
    pub custom_type: Option<String>,
    /// Result order, serialized as `oldestFirst` / `newestFirst`.
    #[serde(with = "branch_order")]
    pub order: Option<ScanOrder>,
    /// Maximum entries returned.
    pub limit: Option<u32>,
    /// Exclude entries at or beyond this sequence.
    pub cursor: Option<EntryCursor>,
}

/// Session-wide entry lookup with cursor pagination.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct EntryQuery {
    /// Keep only entries of this type; `None` keeps every type.
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    /// Keep only entries whose effective custom type equals this.
    #[serde(rename = "customType")]
    pub custom_type: Option<String>,
    /// Traversal direction; `None` means the reader's default.
    pub order: Option<ScanOrder>,
    /// Maximum entries returned; `None` uses the backend default.
    pub limit: Option<u32>,
    /// Resume point, interpreted exclusively in the direction of travel.
    pub cursor: Option<EntryCursor>,
}

/// Storage-level session-wide entry scan over an inclusive sequence window.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct EntryScan {
    /// Lowest sequence to include, inclusive. `None` starts at the beginning.
    #[serde(rename = "fromSeq")]
    pub from_seq: Option<u64>,
    /// Highest sequence to include, inclusive. `None` runs to the end.
    #[serde(rename = "toSeq")]
    pub to_seq: Option<u64>,
    /// Traversal direction; `None` means the backend's default.
    pub order: Option<ScanOrder>,
    /// Maximum entries returned; `None` uses the backend default.
    pub limit: Option<u32>,
    /// Keep only entries of this type.
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    /// Keep only entries whose effective custom type equals this.
    #[serde(rename = "customType")]
    pub custom_type: Option<String>,
}

/// Storage-level usage-row scan over an inclusive sequence window.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageScan {
    /// Lowest sequence to include, inclusive.
    #[serde(rename = "fromSeq")]
    pub from_seq: Option<u64>,
    /// Highest sequence to include, inclusive.
    #[serde(rename = "toSeq")]
    pub to_seq: Option<u64>,
    /// Traversal direction; `None` means the backend's default.
    pub order: Option<ScanOrder>,
    /// Maximum rows returned; `None` uses the backend default.
    pub limit: Option<u32>,
}

/// Entry identity and ordering without the payload, for tree and inventory
/// views that never need message bodies.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EntryStructure {
    /// Session-unique entry id.
    pub id: EntryId,
    /// Predecessor id; `None` marks a branch root.
    #[serde(rename = "parentId")]
    pub parent_id: Option<EntryId>,
    /// Storage-assigned sequence number.
    pub seq: u64,
    /// Commit timestamp in Unix epoch milliseconds.
    pub timestamp: i64,
    /// Entry discriminator.
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    /// Effective application discriminator; `None` when the entry declares none.
    #[serde(rename = "customType", skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
}

/// A stored value as read through the untyped (JSON) boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RawStoredValue {
    /// Address namespace, e.g. `pi.lane.state`.
    pub namespace: String,
    /// Address key within the namespace.
    pub key: String,
    /// Whether the address names a single value or a list.
    pub kind: AddressKind,
    /// Serialized value, decoded by the typed reader at the call site.
    pub value: serde_json::Value,
    /// Sequence assigned by the commit that last wrote this address.
    pub seq: u64,
}

/// One list element as read through the untyped (JSON) boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RawListElement {
    /// Sequence assigned by the commit that appended this element; orders the
    /// list and serves as the pagination cursor.
    pub seq: u64,
    /// Serialized element value.
    pub value: serde_json::Value
}

impl From<&super::Entry> for EntryStructure {
    fn from(entry: &super::Entry) -> Self {
        Self { id: entry.id().clone(), parent_id: entry.parent_id().cloned(), seq: entry.seq(), timestamp: entry.timestamp(), entry_type: entry.entry_type(), custom_type: entry.custom_type().map(str::to_owned) }
    }
}

mod branch_order {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use super::ScanOrder;
    #[expect(
        clippy::ref_option,
        clippy::trivially_copy_pass_by_ref,
        reason = "Serde serialize_with passes a reference to the complete Option field"
    )]
    pub fn serialize<S: Serializer>(order: &Option<ScanOrder>, serializer: S) -> Result<S::Ok, S::Error> {
        order.map(|value| match value { ScanOrder::Asc => "oldestFirst", ScanOrder::Desc => "newestFirst" }).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<ScanOrder>, D::Error> {
        Option::<String>::deserialize(deserializer)?.map(|value| match value.as_str() { "oldestFirst" => Ok(ScanOrder::Asc), "newestFirst" => Ok(ScanOrder::Desc), _ => Err(serde::de::Error::custom("invalid branch scan order")) }).transpose()
    }
}
