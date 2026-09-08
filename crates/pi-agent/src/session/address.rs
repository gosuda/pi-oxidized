use std::borrow::Cow;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use super::{EntryId, LaneConfiguration, LaneName, OperationId, OperationResultRecord, OperationMeta, OperationState, PendingEntry, ScanOrder};
use super::DurableStructuralPreparation;
use crate::tool::AgentToolResult;

/// Which durable sub-space an address names: a single value or an append-only list.
///
/// Value slots and list slots are keyed independently, so the same
/// `(namespace, key)` pair can name both at once; `kind` selects which one a
/// read or write targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressKind {
    /// A single mutable value at one `(namespace, key)` pair.
    Value,
    /// An ordered sequence of elements appended under one `(namespace, key)` pair.
    ///
    /// Elements are addressed by their commit sequence, never by index.
    List,
}

/// Owned, type-erasure-ready address of one durable value slot.
///
/// `T` is a compile-time phantom: it never appears in storage, so it is not a
/// version or schema discriminator. Two addresses are equal when their
/// namespace and key match. The namespace is `&'static str` because every
/// durable namespace in this crate is a frozen literal; the key is owned
/// (`Cow<'static, str>`) because it is normally built from runtime identifiers.
#[derive(Debug, PartialEq, Eq)]
pub struct Value<T> {
    namespace: &'static str,
    key: Cow<'static, str>,
    _marker: PhantomData<fn() -> T>,
}

/// Owned, type-erasure-ready address of one durable list slot.
///
/// Same identity rules as [`Value`]: equality is namespace plus key, and `T` is
/// a phantom that only guides typed reads and writes.
#[derive(Debug, PartialEq, Eq)]
pub struct ValueList<T> {
    namespace: &'static str,
    key: Cow<'static, str>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Value<T> {
    fn clone(&self) -> Self {
        Self { namespace: self.namespace, key: self.key.clone(), _marker: self._marker }
    }
}

impl<T> Clone for ValueList<T> {
    fn clone(&self) -> Self {
        Self { namespace: self.namespace, key: self.key.clone(), _marker: self._marker }
    }
}

/// Rejects an address that a backend could not key on.
///
/// Returned by [`Value::new`] and [`ValueList::new`]. There is no payload: the
/// offending namespace and key are not copied into the error, so callers treat
/// this as a construction bug rather than a reportable storage failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid durable address")]
pub struct InvalidAddress;

impl<T> Value<T> {
    /// Builds a value address.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidAddress`] when `namespace` is empty or when either
    /// component contains a NUL, both of which would break a backend that keys
    /// on `(namespace, key)`.
    pub fn new(namespace: &'static str, key: impl Into<Cow<'static, str>>) -> Result<Self, InvalidAddress> {
        let key = key.into();
        validate(namespace, &key)?;
        Ok(Self { namespace, key, _marker: PhantomData })
    }
    /// Returns the namespace as a borrowed slice, for comparisons and display.
    #[must_use] pub fn namespace(&self) -> &str { self.namespace }
    /// Returns the namespace as its frozen `&'static str`.
    ///
    /// Typed scans rebuild each hit's address from the scanned prefix's
    /// namespace, which needs the static lifetime to survive past the borrow.
    #[must_use] pub fn static_namespace(&self) -> &'static str { self.namespace }
    /// Returns the key.
    #[must_use] pub fn key(&self) -> &str { self.key.as_ref() }
    /// Drops the `T` phantom, producing the address a backend trait method takes.
    #[must_use] pub fn erase(&self) -> RawAddress { RawAddress::value(self.namespace, self.key()) }
}

impl<T> ValueList<T> {
    /// Builds a list address under the same rules as [`Value::new`].
    ///
    /// # Errors
    ///
    /// Returns [`InvalidAddress`] when `namespace` is empty or when either
    /// component contains a NUL.
    pub fn new(namespace: &'static str, key: impl Into<Cow<'static, str>>) -> Result<Self, InvalidAddress> {
        let key = key.into();
        validate(namespace, &key)?;
        Ok(Self { namespace, key, _marker: PhantomData })
    }
    /// Returns the namespace as a borrowed slice.
    #[must_use] pub fn namespace(&self) -> &str { self.namespace }
    /// Returns the key.
    #[must_use] pub fn key(&self) -> &str { self.key.as_ref() }
    /// Drops the `T` phantom, producing the address a backend trait method takes.
    #[must_use] pub fn erase(&self) -> RawAddress { RawAddress::list(self.namespace, self.key()) }
}

fn validate(namespace: &str, key: &str) -> Result<(), InvalidAddress> {
    if namespace.is_empty() || namespace.contains('\0') || key.contains('\0') { Err(InvalidAddress) } else { Ok(()) }
}

/// Address as a backend sees it: all components owned, no phantom.
///
/// This is the wire and storage shape, so it is serde-serializable while
/// [`Value`] and [`ValueList`] are not. Backends key on the `(namespace, key)`
/// pair; `kind` selects the value or list sub-space.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RawAddress {
    /// Durable namespace, e.g. `pi.op.state`.
    pub namespace: String,
    /// Slot key within `namespace`, e.g. an operation id or a composite key.
    pub key: String,
    /// Which sub-space to read or write.
    pub kind: AddressKind,
}

impl RawAddress {
    /// Builds a value-sub-space address from borrowed components.
    #[must_use] pub fn value(namespace: &str, key: &str) -> Self {
        Self { namespace: namespace.to_owned(), key: key.to_owned(), kind: AddressKind::Value }
    }
    /// Builds a list-sub-space address from borrowed components.
    #[must_use] pub fn list(namespace: &str, key: &str) -> Self {
        Self { namespace: namespace.to_owned(), key: key.to_owned(), kind: AddressKind::List }
    }
}

/// One typed value read back from storage.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredValue<T> {
    /// Address the value was read from, rebuilt from the scanned or requested
    /// namespace plus the key the backend reported.
    pub address: Value<T>,
    /// Decoded payload.
    pub value: T,
    /// Commit sequence of the write that stored this value, useful for
    /// detecting whether a read is older than a known commit.
    pub seq: u64,
}
/// One typed list element read back from storage.
#[derive(Clone, Debug, PartialEq)]
pub struct ListElement<T> {
    /// Sequence assigned when the element was appended; stable across reads and
    /// the only handle a cursor can use.
    pub seq: u64,
    /// Decoded payload.
    pub value: T,
}

/// Opaque pagination handle into a list: the sequence of the last element seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListCursor {
    /// Sequence boundary, exclusive on the next read.
    pub seq: u64,
}
/// Caller-supplied list read window; every field unset means "backend default".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListReadOptions {
    /// Resume after this element.
    pub cursor: Option<ListCursor>,
    /// Traversal direction; defaults to [`ScanOrder::Asc`].
    pub order: Option<ScanOrder>,
    /// Maximum elements to return; defaults to [`LIST_READ_DEFAULT_LIMIT`],
    /// clamped to [`LIST_READ_MAX_LIMIT`].
    pub limit: Option<u32>,
}
/// List read window after defaults and clamping have been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedListReadOptions {
    /// Carried through unchanged from [`ListReadOptions`].
    pub cursor: Option<ListCursor>,
    /// Guaranteed direction.
    pub order: ScanOrder,
    /// Guaranteed positive, backend-clamped element cap.
    pub limit: u32,
}

/// List read limit applied when the caller supplies none.
pub const LIST_READ_DEFAULT_LIMIT: u32 = 1_000;
/// Largest list read limit a backend honours; higher requests are clamped down.
pub const LIST_READ_MAX_LIMIT: u32 = 10_000;

/// Fills in defaults and clamps a list read window.
///
/// Backends call this before honouring [`ListReadOptions`] so a caller cannot
/// request an unbounded read.
///
/// # Errors
///
/// Returns [`InvalidAddress`] when an explicit `limit` is zero; an absent limit
/// becomes [`LIST_READ_DEFAULT_LIMIT`] and a larger one is clamped to
/// [`LIST_READ_MAX_LIMIT`].
pub fn resolve_list_read_options(options: Option<ListReadOptions>) -> Result<ResolvedListReadOptions, InvalidAddress> {
    let options = options.unwrap_or_default();
    let limit = options.limit.unwrap_or(LIST_READ_DEFAULT_LIMIT);
    if limit == 0 { return Err(InvalidAddress); }
    Ok(ResolvedListReadOptions { cursor: options.cursor, order: options.order.unwrap_or(ScanOrder::Asc), limit: limit.min(LIST_READ_MAX_LIMIT) })
}

fn address<T>(namespace: &'static str, key: impl Into<Cow<'static, str>>) -> Value<T> {
    Value { namespace, key: key.into(), _marker: PhantomData }
}
fn list_address<T>(namespace: &'static str, key: impl Into<Cow<'static, str>>) -> ValueList<T> {
    ValueList { namespace, key: key.into(), _marker: PhantomData }
}

/// The frozen `pi.*` address helpers below name one durable slot each, so no
/// caller hand-writes a namespace string. Composite keys join their identifier
/// parts with `:`; a key ending in `:` is a scan prefix matching every slot
/// under the preceding part. Namespaces are literals and keys are built from
/// identifier strings, so construction cannot fail and these helpers unwrap.
/// Current tip entry of one branch lane, or `None` when the lane is empty.
#[must_use]
pub fn branch_tip(branch: &str) -> Value<Option<EntryId>> { address("pi.branch.tip", branch.to_owned()) }
/// Empty-key prefix over `pi.branch.tip`, for scanning which branch lanes exist.
#[must_use]
pub fn branch_tip_inventory_prefix() -> Value<Option<EntryId>> { address("pi.branch.tip", "") }
/// Persisted model, thinking level, and active tools of one lane.
#[must_use]
pub fn lane_config(lane: &LaneName) -> Value<LaneConfiguration> { address("pi.lane.config", lane.as_str().to_owned()) }
/// Persisted current/last operation ids and inbox of one lane.
#[must_use]
pub fn lane_state(lane: &LaneName) -> Value<super::LaneState> { address("pi.lane.state", lane.as_str().to_owned()) }
/// Recorded outcome of a completed operation, replayed instead of re-run.
#[must_use]
pub fn operation_result(op: &OperationId) -> Value<OperationResultRecord> { address("pi.result", op.as_str().to_owned()) }
/// Operation identity and request metadata.
#[must_use]
pub fn operation_meta(op: &OperationId) -> Value<OperationMeta> { address("pi.op.meta", op.as_str().to_owned()) }
/// Normalized state-machine record of one operation.
#[must_use]
pub fn operation_state(op: &OperationId) -> Value<OperationState> { address("pi.op.state", op.as_str().to_owned()) }
/// Resolved arguments of one tool call, keyed by operation, step, and source index.
#[must_use]
pub fn operation_tool_args(op: &OperationId, step: &str, source_index: u32) -> Value<serde_json::Map<String, serde_json::Value>> { address("pi.op.tool_args", format!("{op}:{step}:{source_index}")) }
/// Memoized tool output of one invocation, keyed by operation, invocation entry, and tool name.
#[must_use]
pub fn operation_tool_memo(op: &OperationId, inv: &EntryId, name: &str) -> Value<serde_json::Value> { address("pi.op.tool_memo", format!("{op}:{inv}:{name}")) }
/// Structural preparation captured for one named task of an operation.
#[must_use]
pub fn operation_preparation(op: &OperationId, task: &str) -> Value<DurableStructuralPreparation> { address("pi.op.preparation", format!("{op}:{task}")) }
/// Prefix over [`operation_tool_args`], narrowed to one step when given.
#[must_use]
pub fn operation_tool_args_prefix(op: &OperationId, step: Option<&str>) -> Value<serde_json::Map<String, serde_json::Value>> { address("pi.op.tool_args", step.map_or_else(|| format!("{op}:"), |s| format!("{op}:{s}:"))) }
/// Prefix over [`operation_tool_memo`], narrowed to one invocation when given.
#[must_use]
pub fn operation_tool_memo_prefix(op: &OperationId, inv: Option<&EntryId>) -> Value<serde_json::Value> { address("pi.op.tool_memo", inv.map_or_else(|| format!("{op}:"), |i| format!("{op}:{i}:"))) }
/// Prefix over [`operation_preparation`] for one operation.
#[must_use]
pub fn operation_preparation_prefix(op: &OperationId) -> Value<DurableStructuralPreparation> { address("pi.op.preparation", format!("{op}:")) }
/// Entry body staged under one entry id before it is committed.
#[must_use]
pub fn pending_entry(entry: &EntryId) -> Value<PendingEntry> { address("pi.pending.entry", entry.as_str().to_owned()) }
/// Tool output staged for one invocation before its entry commits.
#[must_use]
pub fn pending_tool_output(op: &OperationId, inv: &EntryId) -> Value<AgentToolResult> { address("pi.pending.tool_output", format!("{op}:{inv}")) }
/// Prefix over [`pending_tool_output`] for one operation.
#[must_use]
pub fn pending_tool_output_prefix(op: &OperationId) -> Value<AgentToolResult> { address("pi.pending.tool_output", format!("{op}:")) }
/// Assistant reply frames staged for one response, so a resumed run can continue
/// mid-stream instead of re-requesting it.
#[must_use]
pub fn pending_assistant_frames(op: &OperationId, response: &EntryId) -> ValueList<pi_ai::AssistantMessageFrame> { list_address("pi.pending.assistant_frame", format!("{op}:{response}")) }
/// Human-visible session name; the singleton key is empty.
#[must_use]
pub fn session_name() -> Value<String> { address("pi.session.name", "") }
/// Human-visible label attached to one entry.
#[must_use]
pub fn entry_label(entry: &EntryId) -> Value<String> { address("pi.entry.label", entry.as_str().to_owned()) }
