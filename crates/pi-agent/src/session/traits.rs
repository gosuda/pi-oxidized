use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, de::DeserializeOwned};

use super::error::{StorageErrorCode, StorageFailure};
use super::{
    BranchScan, CommitResult, Entry, EntryId, EntryQuery, EntryScan, EntryStructure, LaneName,
    ListElement, ListReadOptions, RawAddress, RawListElement, RawStoredValue, SessionError,
    SessionStats, StorageBranchScan, StoredValue, UsageRow, UsageScan, Value, ValueList, Write,
};
use crate::context::Context;
use crate::message::AgentMessage;

/// The raw backend contract: JSON-in, JSON-out, with no typed-address or
/// serde knowledge above the `(namespace, key)` boundary.
///
/// A backend owns sequence assignment and commit atomicity; callers go through
/// [`Session`] or [`SessionReaderExt`] for typed access. Every method is
/// cancellation-aware: it checks `cx` before touching storage and maps a
/// cancelled scope to `SessionError::Backend` with
/// [`StorageErrorCode::Aborted`].
pub trait Storage: Send + Sync {
    /// Applies `writes` as one atomic batch.
    ///
    /// The backend assigns each write a fresh, gap-free sequence starting at
    /// the batch's `first_seq`, validates the batch with
    /// [`super::validate_committed_writes`] before admitting it, and either
    /// applies every write or none.
    ///
    /// # Errors
    ///
    /// [`SessionError::Invariant`] when validation rejects the batch (stale
    /// `first_seq`, duplicate or unknown-parent ids, pending assistant
    /// messages); [`StorageErrorCode::Closed`] once the backend is closed.
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>>;
    /// Fetches entries by id, omitting ids that do not exist.
    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>>;
    /// Reads one value slot; `None` when unset.
    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>>;
    /// Reads every value whose namespace matches `prefix.namespace` and whose
    /// key starts with `prefix.key`, ordered by `(namespace, key)`.
    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>>;
    /// Reads a window of one list slot under `options`.
    ///
    /// # Errors
    ///
    /// [`SessionError::Invariant`] when the resolved limit is zero.
    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>>;
    /// Walks parent links backwards from `query.start`, returning the entries
    /// the query's filters keep.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTarget`] for an ancestor id that is not stored;
    /// [`SessionError::Invariant`] if the ancestry contains a cycle.
    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>>;
    /// [`scan_branch`](Self::scan_branch) projected onto
    /// [`EntryStructure`], for callers that need the graph without payloads.
    fn scan_branch_structure<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>>;
    /// Reads entries by sequence range and type filters, ordered by sequence.
    fn scan_entries<'a>(
        &'a self,
        query: &'a EntryScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>>;
    /// Reads usage rows by sequence range, ordered by sequence.
    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>>;
    /// Returns the running message count and usage totals.
    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>>;
    /// Closes the backend. Later reads and commits fail with
    /// [`StorageErrorCode::Closed`]; closing twice is not an error.
    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>>;
}

/// The read surface a session handle exposes, in raw JSON form.
///
/// Implementors may layer admission control (open/close state, per-session
/// scoping) on top of a [`Storage`]. Typed callers should use
/// [`SessionReaderExt`], which decodes these JSON reads.
///
/// Every method is cancellation-aware; see [`Storage`] for the abort and
/// closed error conventions.
pub trait SessionReader: Send + Sync {
    /// Fetches entries by id; missing ids are absent from the map.
    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>>;
    /// Returns the session's message count and usage totals.
    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>>;
    /// Reads one raw value slot; `None` when unset.
    fn get_value_json<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>>;
    /// Reads every raw value under a `(namespace, key-prefix)`.
    fn scan_values_json<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>>;
    /// Reads a raw window of one list slot.
    fn read_list_json<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>>;
    /// Walks parent links backwards from the query's start entry.
    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>>;
}

/// Typed JSON decoding layered on the raw [`SessionReader`] reads.
///
/// Blanket-implemented for every reader, so callers use these methods directly
/// on any [`SessionReader`].
pub trait SessionReaderExt: SessionReader {
    /// Reads and decodes one typed value slot.
    ///
    /// # Errors
    ///
    /// [`StorageErrorCode::Corrupt`] when the stored JSON no longer decodes as
    /// `T`; backend errors from the underlying read pass through.
    fn get_value<'a, T: DeserializeOwned + Send + 'a>(
        &'a self,
        address: &'a Value<T>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<StoredValue<T>>, SessionError>> {
        Box::pin(async move {
            let Some(raw) = self.get_value_json(&address.erase(), cx).await? else {
                return Ok(None);
            };
            let value = serde_json::from_value(raw.value).map_err(corrupt_value)?;
            Ok(Some(StoredValue {
                address: address.clone(),
                value,
                seq: raw.seq,
            }))
        })
    }
    /// Scans and decodes every typed value under `prefix`.
    ///
    /// Each hit's address is rebuilt from the prefix's namespace plus the key
    /// the backend reported.
    ///
    /// # Errors
    ///
    /// [`StorageErrorCode::Corrupt`] when a stored payload fails to decode as
    /// `T`, or when a reported key is not a valid address; backend errors pass
    /// through.
    fn scan_values<'a, T: DeserializeOwned + Send + 'a>(
        &'a self,
        prefix: &'a Value<T>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<StoredValue<T>>, SessionError>> {
        Box::pin(async move {
            let values = self.scan_values_json(&prefix.erase(), cx).await?;
            values
                .into_iter()
                .map(|raw| {
                    let value = serde_json::from_value(raw.value).map_err(corrupt_value)?;
                    let address = Value::new(prefix.static_namespace(), raw.key)
                        .map_err(|_| corrupt_message("stored address is invalid"))?;
                    Ok(StoredValue {
                        address,
                        value,
                        seq: raw.seq,
                    })
                })
                .collect()
        })
    }
    /// Reads and decodes a typed window of one list slot.
    ///
    /// # Errors
    ///
    /// [`StorageErrorCode::Corrupt`] when an element fails to decode as `T`;
    /// backend errors (including a zero resolved limit) pass through.
    fn read_list<'a, T: DeserializeOwned + Send + 'a>(
        &'a self,
        address: &'a ValueList<T>,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<ListElement<T>>, SessionError>> {
        Box::pin(async move {
            self.read_list_json(&address.erase(), options, cx)
                .await?
                .into_iter()
                .map(|raw| {
                    serde_json::from_value(raw.value)
                        .map(|value| ListElement {
                            seq: raw.seq,
                            value,
                        })
                        .map_err(corrupt_value)
                })
                .collect()
        })
    }
}
fn corrupt_value(error: serde_json::Error) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Corrupt,
        message: "stored value decode failed".to_owned(),
        source: Some(Arc::new(error)),
    })
}

fn corrupt_message(message: &str) -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::Corrupt, message))
}
impl<R: SessionReader + ?Sized> SessionReaderExt for R {}

/// The single-writer handle returned by [`Session::begin_mutation`].
///
/// The `'s` lifetime ties the guard to the session borrow, so the guard cannot
/// outlive the session it locks. Dropping it without committing releases the
/// session's mutation slot; nothing is written.
pub type MutationGuard<'s> = Box<dyn SessionMutation<'s> + Send + 's>;
/// A session held for one mutation: reads plus the right to commit.
///
/// The guard is consumed by committing — `self: Box<Self>` makes a second
/// commit on the same handle impossible, so each guarded batch is exactly one
/// commit attempt.
///
/// # Errors (commit)
///
/// Propagates the backend's validation, closed, and abort errors; see
/// [`Storage::commit`].
pub trait SessionMutation<'s>: SessionReader + 's {
    /// Consumes the guard and commits `writes` as one batch.
    fn commit(
        self: Box<Self>,
        writes: Vec<Write>,
        cx: &Context,
    ) -> BoxFuture<'s, Result<CommitResult, SessionError>>;
}

/// One named conversation lane with its own tip and append path.
///
/// A handle reads and appends through the session it came from; the tip it
/// reports is whatever the backend currently holds for that lane, so two calls
/// can differ if another writer moved it in between.
pub trait Branch: Send + Sync {
    /// The lane name this handle was created under.
    fn name(&self) -> &LaneName;
    /// The current tip entry, or `None` when the branch is empty.
    fn get_tip_id<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<EntryId>, SessionError>>;
    /// Walks ancestry from the query's start (defaulting to the tip) and
    /// returns the entries the filters keep.
    fn find_entries<'a>(
        &'a self,
        q: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>>;
    /// The first entry [`find_entries`](Self::find_entries) returns, if any.
    fn find_entry<'a>(
        &'a self,
        q: Option<&'a BranchScan>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Entry>, SessionError>>;
    /// Appends `message` as a new entry whose parent is the current tip, then
    /// moves the tip to it in the same commit.
    fn append_message<'a>(
        &'a self,
        message: AgentMessage,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, SessionError>>;
    /// Appends a custom-typed entry under the same tip-then-move protocol as
    /// [`append_message`](Self::append_message).
    fn append_custom_entry<'a>(
        &'a self,
        custom_type: &'a str,
        data: Option<serde_json::Value>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<EntryId, SessionError>>;
}

/// The full session surface: raw reads, branch handles, guarded mutation, and
/// the named/labelled value slots the product shows.
///
/// The `*_json` write methods each land one write; a caller needing several
/// writes to commit together must take [`begin_mutation`](Self::begin_mutation)
/// and submit them as one batch.
pub trait Session: SessionReader {
    /// The session's stored identity and provenance, borrowed.
    fn metadata(&self) -> &SessionMetadata;
    /// The id source this session stamps new entries with.
    fn id_generator(&self) -> &dyn IdGenerator;
    /// Fetches one entry by id.
    fn get_entry<'a>(
        &'a self,
        id: &'a EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Entry>, SessionError>>;
    /// The human-visible session name, when set.
    fn get_name<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, SessionError>>;
    /// The human-visible label on one entry, when set.
    fn get_label<'a>(
        &'a self,
        target: &'a EntryId,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<String>, SessionError>>;
    /// Scans session-wide entries by type and cursor.
    fn find_entries<'a>(
        &'a self,
        q: Option<&'a EntryQuery>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>>;
    /// The first entry [`find_entries`](Self::find_entries) returns, if any.
    fn find_entry<'a>(
        &'a self,
        q: Option<&'a EntryQuery>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Entry>, SessionError>>;
    /// The handle for an existing lane, `None` when no such lane exists.
    fn branch<'a>(
        &'a self,
        name: &'a LaneName,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Arc<dyn Branch>>, SessionError>>;
    /// Registers a new lane starting at `at` (default: empty).
    ///
    /// # Errors
    ///
    /// [`SessionError::BranchExists`] when the name is taken;
    /// [`SessionError::UnknownTarget`] when `at` names no stored entry.
    fn create_branch<'a>(
        &'a self,
        name: &'a LaneName,
        at: Option<&'a EntryId>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Branch>, SessionError>>;
    /// Takes the session's single mutation slot.
    ///
    /// The holder has exclusive write access until the guard is dropped or
    /// consumed; other callers wait rather than fail.
    fn begin_mutation<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<MutationGuard<'a>, SessionError>>;
    /// Sets one raw value slot in its own commit.
    fn set_value_json<'a>(
        &'a self,
        address: &'a RawAddress,
        next: serde_json::Value,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Deletes one raw value slot in its own commit.
    fn delete_value_json<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Appends one raw element to a list slot in its own commit.
    fn append_list_json<'a>(
        &'a self,
        address: &'a RawAddress,
        element: serde_json::Value,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Deletes a whole list slot in its own commit.
    fn delete_list_json<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Renames the session; `None` deletes the name.
    fn set_name<'a>(
        &'a self,
        name: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Labels one entry; `None` deletes its label.
    fn set_label<'a>(
        &'a self,
        target: &'a EntryId,
        label: Option<&'a str>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Closes the session, rejecting later reads and writes. Idempotent.
    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>>;
}

/// Session lifecycle at the collection level: create, reopen, list, delete, fork.
pub trait SessionRepo: Send + Sync {
    /// The metadata record this repository stores and keys sessions by.
    type Metadata: SessionMetadataLike;
    /// Backend-specific creation knobs.
    type CreateOptions: Send + Sync;
    /// Backend-specific listing knobs.
    type ListOptions: Send + Sync;
    /// Creates a session from `options`, reserving its id.
    ///
    /// # Errors
    ///
    /// [`SessionError::Invariant`] when the id is already reserved.
    fn create<'a>(
        &'a self,
        options: Self::CreateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>>;
    /// Reopens a known session by metadata.
    ///
    /// # Errors
    ///
    /// [`StorageErrorCode::NotFound`] when the repository holds no such id.
    fn open<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>>;
    /// Lists the metadata of every stored session.
    fn list<'a>(
        &'a self,
        options: Option<Self::ListOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Self::Metadata>, SessionError>>;
    /// Removes a session and frees its id.
    ///
    /// # Errors
    ///
    /// [`SessionError::Invariant`] when the session is still open;
    /// [`StorageErrorCode::NotFound`] for an unknown id.
    fn delete<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
    /// Creates a new session seeded from `source`'s state.
    ///
    /// The fork records `source` as its parent session and copies either the
    /// whole tree or one branch's ancestry, per [`ForkOptions`].
    fn fork<'a>(
        &'a self,
        source: &'a Self::Metadata,
        options: ForkOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>>;
}

/// Source of entry, session, and usage ids.
pub trait IdGenerator: Send + Sync {
    /// Produces the next id.
    ///
    /// `timestamp_ms` pins the time component when the caller needs the id to
    /// agree with a known creation time (e.g. session metadata); `None` lets
    /// the generator use the current time.
    ///
    /// # Errors
    ///
    /// Returns a [`SessionError`] when the generator cannot produce a valid id
    /// (clock, timestamp, or sequence failure).
    fn next(&self, timestamp_ms: Option<i64>) -> Result<String, SessionError>;
}

/// Stored identity and provenance of one session.
#[derive(Clone, Debug, Deserialize, PartialEq, serde::Serialize)]
pub struct SessionMetadata {
    /// Unique session id, normally a `UUIDv7`.
    pub id: String,
    /// Creation time, milliseconds since the Unix epoch.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Storage layout version this session was written with, so a backend can
    /// reject or migrate foreign data.
    #[serde(rename = "storageVersion")]
    pub storage_version: u32,
    /// Working directory the session was started in, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Id of the session this one was forked from, when forked.
    #[serde(rename = "parentSessionId", skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Filesystem path of the parent session recorded by older on-disk
    /// formats; retained so legacy forks keep their provenance.
    #[serde(
        rename = "legacyParentSessionPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_parent_session_path: Option<String>,
}

/// Marker for repository-specific metadata records.
///
/// Each backend implements this trait for its metadata type.
pub trait SessionMetadataLike: Clone + Send + Sync + 'static {}
impl SessionMetadataLike for SessionMetadata {}

/// How a fork selects the state the new session starts with.
#[derive(Clone, Debug)]
pub enum ForkOptions {
    /// Copy one branch's ancestry: the entries reachable from a starting
    /// entry (default: the branch's current tip) plus that lane's config and
    /// tip. `id` overrides the generated session id.
    Branch {
        /// Lane whose tip anchors the copied ancestry.
        branch: LaneName,
        /// Entry to fork at; `None` uses the branch tip.
        entry_id: Option<EntryId>,
        /// Whether the anchor entry itself is included.
        position: ForkPosition,
        /// Explicit session id, or `None` to generate one.
        id: Option<String>,
    },
    /// Copy the whole session: every entry, value, and usage row, minus
    /// transient `pi.pending.*` lists. `id` overrides the generated id.
    Tree {
        /// Explicit session id, or `None` to generate one.
        id: Option<String>,
    },
}
/// Where a branch fork sits relative to its anchor entry.
#[derive(Clone, Copy, Debug, Default)]
pub enum ForkPosition {
    /// Anchor on the entry's parent; the anchor entry is not copied.
    Before,
    /// Anchor on the entry itself; it is part of the copied ancestry.
    #[default]
    At,
}
