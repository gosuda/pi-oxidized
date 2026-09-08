use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use tokio::sync::Mutex;

use super::address::{
    AddressKind, LIST_READ_DEFAULT_LIMIT, LIST_READ_MAX_LIMIT, ListReadOptions, RawAddress,
    resolve_list_read_options,
};
use super::backed::{StorageBackedSession, aborted_error, closed_error, validate_branch_name};
use super::entry::Entry;
use super::error::{SessionError, StorageErrorCode, StorageFailure};
use super::fork::{ForkSource, ForkSourceSnapshot, create_fork_snapshot, fork_snapshot_writes};
use super::ids::{EntryId, UsageId, UuidV7Generator};
use super::scan::{EntryScan, EntryStructure, ScanOrder, StorageBranchScan, UsageScan};
use super::traits::{ForkOptions, IdGenerator, Session, SessionMetadata, SessionRepo, Storage};
use super::write::{
    CommitResult, CommittedIdView, CommittedListWrite, CommittedValueWrite, CommittedWrite,
    SessionStats, UsageRow, Write, commit_writes, validate_committed_writes,
    validate_replayed_writes,
};
use super::{RawListElement, RawStoredValue};
use crate::context::Context;

/// Complete contents of the reference in-memory backend, held behind one
/// mutex by [`MemoryStorage`].
///
/// Maps are keyed so the two identity views a commit needs are direct lookups:
/// by id for entries/usage, by `(namespace, key)` for value and list slots.
#[derive(Clone, Debug)]
pub struct InMemoryStorageState {
    /// Stored entries keyed by id.
    pub entries: BTreeMap<EntryId, Entry>,
    /// Reverse index from assigned sequence to entry id.
    pub entries_by_seq: BTreeMap<u64, EntryId>,
    /// Value slots keyed by `(namespace, key)`.
    pub values: BTreeMap<(String, String), RawStoredValue>,
    /// List slots keyed by `(namespace, key)`.
    pub lists: BTreeMap<(String, String), Vec<RawListElement>>,
    /// Usage rows keyed by id.
    pub usage: BTreeMap<UsageId, UsageRow>,
    /// Totals accumulated from committed message and usage writes.
    pub stats: SessionStats,
    /// Sequence the next committed write receives; starts at 1.
    pub next_seq: u64,
}

impl Default for InMemoryStorageState {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorageState {
    /// Empty state whose first write takes sequence 1.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            entries_by_seq: BTreeMap::new(),
            values: BTreeMap::new(),
            lists: BTreeMap::new(),
            usage: BTreeMap::new(),
            stats: SessionStats::default(),
            next_seq: 1,
        }
    }

    /// Validates and applies `writes` as one batch.
    ///
    /// Validation runs before any state is changed. Once admitted, the batch
    /// is converted to canonical committed writes and applied under the
    /// storage handle's state lock.
    ///
    /// # Errors
    ///
    /// Returns the shared commit-validation errors from
    /// [`super::validate_committed_writes`].
    pub fn apply(
        &mut self,
        writes: &[Write],
        timestamp: i64,
    ) -> Result<CommitResult, SessionError> {
        let first_seq = self.next_seq;
        validate_committed_writes(writes, first_seq, self)?;
        let (committed, seqs) = commit_writes(writes.to_vec(), first_seq, timestamp)?;
        let stats = self.apply_committed(&committed);
        Ok(CommitResult {
            first_seq,
            seqs,
            timestamp,
            stats,
        })
    }

    /// Applies committed writes that have already passed validation.
    ///
    /// This is the common materialization path for fresh commits, replay, and
    /// fork destinations. The caller owns the atomicity boundary.
    pub fn apply_committed(&mut self, writes: &[CommittedWrite]) -> SessionStats {
        for write in writes {
            match write {
                CommittedWrite::Entry(entry) => {
                    if matches!(entry, Entry::Message { .. }) {
                        self.stats.message_count = self.stats.message_count.saturating_add(1);
                    }
                    self.entries_by_seq.insert(entry.seq(), entry.id().clone());
                    self.entries.insert(entry.id().clone(), entry.clone());
                }
                CommittedWrite::Usage(row) => {
                    add_usage(&mut self.stats.usage, &row.usage);
                    self.usage.insert(row.id.clone(), row.clone());
                }
                CommittedWrite::Value(CommittedValueWrite::Set {
                    seq,
                    namespace,
                    key,
                    value,
                }) => {
                    self.values.insert(
                        (namespace.clone(), key.clone()),
                        RawStoredValue {
                            namespace: namespace.clone(),
                            key: key.clone(),
                            kind: AddressKind::Value,
                            value: value.clone(),
                            seq: *seq,
                        },
                    );
                }
                CommittedWrite::Value(CommittedValueWrite::Delete { namespace, key, .. }) => {
                    self.values.remove(&(namespace.clone(), key.clone()));
                }
                CommittedWrite::List(CommittedListWrite::Append {
                    seq,
                    namespace,
                    key,
                    value,
                }) => {
                    self.lists
                        .entry((namespace.clone(), key.clone()))
                        .or_default()
                        .push(RawListElement {
                            seq: *seq,
                            value: value.clone(),
                        });
                }
                CommittedWrite::List(CommittedListWrite::Delete { namespace, key, .. }) => {
                    self.lists.remove(&(namespace.clone(), key.clone()));
                }
            }
            self.next_seq = write.seq().saturating_add(1);
        }
        self.stats.clone()
    }

    /// Validates and applies explicitly sequenced writes from durable replay.
    ///
    /// Replay preserves sequence gaps, but still requires strict ordering,
    /// unique identities, and entry-only parent links.
    /// # Errors
    ///
    /// Returns [`SessionError::Invariant`] when replay validation rejects a
    /// sequence, identity, or parent-link invariant.
    pub fn replay(&mut self, writes: &[CommittedWrite]) -> Result<SessionStats, SessionError> {
        validate_replayed_writes(writes, self)?;
        Ok(self.apply_committed(writes))
    }

    /// Advances the sequence high-water mark without materializing a write.
    pub fn advance_next_seq(&mut self, next_seq: u64) {
        self.next_seq = self.next_seq.max(next_seq);
    }

    /// Reconstructs a fresh state from a backend-neutral fork snapshot.
    ///
    /// Entry sequence numbers remain the source numbers. Scalar values carry
    /// the destination sequence numbers assigned by [`create_fork_snapshot`].
    ///
    /// # Errors
    ///
    /// Returns the shared replay-validation error when the snapshot is
    /// inconsistent with the destination's identity and parent rules.
    pub fn from_fork_snapshot(
        snapshot: &super::fork::ForkDestinationSnapshot,
    ) -> Result<Self, SessionError> {
        let mut state = Self::new();
        let writes = fork_snapshot_writes(snapshot);
        state.replay(&writes)?;
        state.advance_next_seq(snapshot.next_seq);
        Ok(state)
    }

    /// Walks parent links backwards from `query.start`, keeping entries the
    /// query's filters accept and stopping at the limit or stop marker.
    ///
    /// # Errors
    ///
    /// [`SessionError::UnknownTarget`] for a missing ancestor;
    /// [`SessionError::Invariant`] on an ancestry cycle.
    pub fn scan_branch(&self, query: &StorageBranchScan) -> Result<Vec<Entry>, SessionError> {
        let mut id = query.start.clone();
        let mut output = Vec::new();
        let mut seen = HashSet::new();
        let limit = usize::try_from(
            query
                .limit
                .unwrap_or(LIST_READ_DEFAULT_LIMIT)
                .min(LIST_READ_MAX_LIMIT),
        )
        .unwrap_or(usize::MAX);
        while output.len() < limit {
            if !seen.insert(id.clone()) {
                return Err(SessionError::Invariant(
                    "cycle in branch ancestry".to_owned(),
                ));
            }
            let entry = self
                .entries
                .get(&id)
                .ok_or_else(|| SessionError::UnknownTarget(id.clone()))?;
            let stop = query.stop_at_id.as_ref().is_some_and(|value| value == &id)
                || query
                    .stop_at_type
                    .is_some_and(|value| value == entry.entry_type());
            if query
                .entry_type
                .is_none_or(|value| value == entry.entry_type())
                && query
                    .custom_type
                    .as_deref()
                    .is_none_or(|value| entry.custom_type() == Some(value))
                && query.cursor.is_none_or(|value| entry.seq() < value.seq)
            {
                output.push(entry.clone());
            }
            let Some(parent) = entry.parent_id() else {
                break;
            };
            id = parent.clone();
            if stop {
                break;
            }
        }
        if query.order.unwrap_or(ScanOrder::Desc) == ScanOrder::Asc {
            output.reverse();
        }
        Ok(output)
    }

    /// Clones the complete source boundary needed by a fork.
    #[must_use]
    pub fn snapshot_for_fork(&self) -> ForkSourceSnapshot {
        let mut entries: Vec<Entry> = self.entries.values().cloned().collect();
        entries.sort_by_key(Entry::seq);
        ForkSourceSnapshot {
            entries,
            values: self.values.values().cloned().collect(),
            entries_complete: true,
        }
    }
}

impl CommittedIdView for InMemoryStorageState {
    fn contains_id(&self, id: &str) -> bool {
        self.entries.contains_key(&EntryId::from(id)) || self.usage.contains_key(&UsageId::from(id))
    }

    fn contains_entry_id(&self, id: &EntryId) -> bool {
        self.entries.contains_key(id)
    }

    fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

fn add_usage(total: &mut pi_ai::Usage, add: &pi_ai::Usage) {
    total.input = total.input.saturating_add(add.input);
    total.output = total.output.saturating_add(add.output);
    total.cache_read = total.cache_read.saturating_add(add.cache_read);
    total.cache_write = total.cache_write.saturating_add(add.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(add.total_tokens);
    total.cache_write1h = match (total.cache_write1h, add.cache_write1h) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, None) => a,
        (None, b) => b,
    };
    total.reasoning = match (total.reasoning, add.reasoning) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, None) => a,
        (None, b) => b,
    };
    total.cost.input += add.cost.input;
    total.cost.output += add.cost.output;
    total.cost.cache_read += add.cost.cache_read;
    total.cost.cache_write += add.cost.cache_write;
    total.cost.total += add.cost.total;
}

fn now_millis() -> Result<i64, SessionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            SessionError::Backend(StorageFailure {
                code: StorageErrorCode::Io,
                message: "failed to read the system clock".to_owned(),
                source: Some(Arc::new(error)),
            })
        })?;
    i64::try_from(duration.as_millis())
        .map_err(|_| SessionError::Invariant("system clock timestamp exceeds i64 range".to_owned()))
}

fn repo_closed_error() -> SessionError {
    SessionError::Backend(StorageFailure::new(
        StorageErrorCode::Closed,
        "session repository is closed",
    ))
}

/// Reference [`Storage`] keeping the whole session in process memory.
///
/// Each value is a handle over shared durable state. Handles have independent
/// closed flags, so a repository can close one session handle and reopen a
/// fresh handle over the same state.
pub struct MemoryStorage {
    state: Arc<Mutex<InMemoryStorageState>>,
    closed: AtomicBool,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStorage {
    /// Layout version recorded in [`SessionMetadata::storage_version`] for
    /// sessions this backend writes.
    pub const STORAGE_VERSION: u32 = 1;

    /// Creates an empty, open storage handle with fresh durable state.
    #[must_use]
    pub fn new() -> Self {
        Self::attach(Arc::new(Mutex::new(InMemoryStorageState::new())))
    }

    /// Creates a fresh open handle over existing durable state.
    #[must_use]
    pub fn attach(state: Arc<Mutex<InMemoryStorageState>>) -> Self {
        Self {
            state,
            closed: AtomicBool::new(false),
        }
    }

    /// Returns the shared durable state handle.
    #[must_use]
    pub fn state(&self) -> Arc<Mutex<InMemoryStorageState>> {
        Arc::clone(&self.state)
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(closed_error())
        } else {
            Ok(())
        }
    }
}

impl Storage for MemoryStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let mut state = self.state.lock().await;
            state.apply(&writes, now_millis()?)
        })
    }

    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            Ok(ids
                .iter()
                .filter_map(|id| {
                    state
                        .entries
                        .get(id)
                        .cloned()
                        .map(|entry| (id.clone(), entry))
                })
                .collect())
        })
    }

    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            Ok(state
                .values
                .get(&(address.namespace.clone(), address.key.clone()))
                .cloned())
        })
    }

    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            Ok(state
                .values
                .values()
                .filter(|value| {
                    value.namespace == prefix.namespace && value.key.starts_with(&prefix.key)
                })
                .cloned()
                .collect())
        })
    }

    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let options = resolve_list_read_options(options)
                .map_err(|_| SessionError::Invariant("list limit must be positive".to_owned()))?;
            let state = self.state.lock().await;
            let mut values = state
                .lists
                .get(&(address.namespace.clone(), address.key.clone()))
                .cloned()
                .unwrap_or_default();
            if let Some(cursor) = options.cursor {
                values.retain(|item| match options.order {
                    ScanOrder::Asc => item.seq > cursor.seq,
                    ScanOrder::Desc => item.seq < cursor.seq,
                });
            }
            if options.order == ScanOrder::Desc {
                values.reverse();
            }
            values.truncate(usize::try_from(options.limit).unwrap_or(usize::MAX));
            Ok(values)
        })
    }

    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            state.scan_branch(query)
        })
    }

    fn scan_branch_structure<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
        Box::pin(async move {
            self.scan_branch(query, cx)
                .await
                .map(|entries| entries.iter().map(EntryStructure::from).collect())
        })
    }

    fn scan_entries<'a>(
        &'a self,
        query: &'a EntryScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            let mut entries: Vec<Entry> = state
                .entries
                .values()
                .filter(|entry| query.from_seq.is_none_or(|value| entry.seq() >= value))
                .filter(|entry| query.to_seq.is_none_or(|value| entry.seq() <= value))
                .filter(|entry| {
                    query
                        .entry_type
                        .is_none_or(|value| entry.entry_type() == value)
                })
                .filter(|entry| {
                    query
                        .custom_type
                        .as_deref()
                        .is_none_or(|value| entry.custom_type() == Some(value))
                })
                .cloned()
                .collect();
            entries.sort_by_key(Entry::seq);
            if query.order == Some(ScanOrder::Desc) {
                entries.reverse();
            }
            entries.truncate(
                usize::try_from(
                    query
                        .limit
                        .unwrap_or(LIST_READ_DEFAULT_LIMIT)
                        .min(LIST_READ_MAX_LIMIT),
                )
                .unwrap_or(usize::MAX),
            );
            Ok(entries)
        })
    }

    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            let mut rows: Vec<UsageRow> = state
                .usage
                .values()
                .filter(|row| query.from_seq.is_none_or(|value| row.seq >= value))
                .filter(|row| query.to_seq.is_none_or(|value| row.seq <= value))
                .cloned()
                .collect();
            rows.sort_by_key(|row| row.seq);
            if query.order == Some(ScanOrder::Desc) {
                rows.reverse();
            }
            rows.truncate(
                usize::try_from(
                    query
                        .limit
                        .unwrap_or(LIST_READ_DEFAULT_LIMIT)
                        .min(LIST_READ_MAX_LIMIT),
                )
                .unwrap_or(usize::MAX),
            );
            Ok(rows)
        })
    }

    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            Ok(state.stats.clone())
        })
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.closed.store(true, Ordering::Release);
            let _state = self.state.lock().await;
            Ok(())
        })
    }
}

impl ForkSource for MemoryStorage {
    fn capture_fork_source<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<ForkSourceSnapshot, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = self.state.lock().await;
            Ok(state.snapshot_for_fork())
        })
    }
}

/// Creation knobs for [`MemorySessionRepo`].
#[derive(Clone, Debug, Default)]
pub struct MemoryCreateOptions {
    /// Metadata to store verbatim; `None` generates id, creation time, and
    /// storage version.
    pub metadata: Option<SessionMetadata>,
}

/// Listing knobs for [`MemorySessionRepo`]; the in-memory repo has none.
#[derive(Clone, Debug, Default)]
pub struct MemoryListOptions;

struct Record {
    metadata: SessionMetadata,
    state: Arc<Mutex<InMemoryStorageState>>,
    open: Arc<AtomicBool>,
    session: Option<Arc<StorageBackedSession>>,
}
struct IdReservation {
    pending_ids: Arc<StdMutex<HashSet<String>>>,
    id: String,
}

impl Drop for IdReservation {
    fn drop(&mut self) {
        let mut pending = match self.pending_ids.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.remove(&self.id);
    }
}

/// In-process [`SessionRepo`] holding sessions for the lifetime of the process.
pub struct MemorySessionRepo {
    sessions: Arc<Mutex<BTreeMap<String, Record>>>,
    pending_ids: Arc<StdMutex<HashSet<String>>>,
    id_generator: Arc<UuidV7Generator>,
    closed: AtomicBool,
}

impl Default for MemorySessionRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySessionRepo {
    /// Repository with no sessions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            pending_ids: Arc::new(StdMutex::new(HashSet::new())),
            id_generator: Arc::new(UuidV7Generator::new()),
            closed: AtomicBool::new(false),
        }
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(repo_closed_error())
        } else {
            Ok(())
        }
    }

    async fn reserve_id(&self, id: &str) -> Result<IdReservation, SessionError> {
        let sessions = self.sessions.lock().await;
        let mut pending = self
            .pending_ids
            .lock()
            .map_err(|_| SessionError::Invariant("session id registry poisoned".to_owned()))?;
        if sessions.contains_key(id) || pending.contains(id) {
            return Err(SessionError::Invariant(format!(
                "session already exists: {id}"
            )));
        }
        pending.insert(id.to_owned());
        Ok(IdReservation {
            pending_ids: Arc::clone(&self.pending_ids),
            id: id.to_owned(),
        })
    }

    fn make_session(&self, record: &Record) -> Arc<StorageBackedSession> {
        let open = Arc::clone(&record.open);
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::attach(Arc::clone(&record.state)));
        let id_generator: Arc<dyn IdGenerator> = self.id_generator.clone();
        StorageBackedSession::new(
            record.metadata.clone(),
            storage,
            id_generator,
            Some(Box::new(move || open.store(false, Ordering::Release))),
        )
    }

    /// Closes the repository and every session handle it currently owns.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Aborted`] when `cx` is cancelled, or the first
    /// session-close error reported while draining owned handles.
    pub async fn close(&self, cx: &Context) -> Result<(), SessionError> {
        cx.check().map_err(|_| aborted_error())?;
        self.closed.store(true, Ordering::Release);
        let sessions: Vec<Arc<StorageBackedSession>> = self
            .sessions
            .lock()
            .await
            .values()
            .filter_map(|record| record.session.clone())
            .collect();
        let mut first_error = None;
        for session in sessions {
            first_error = first_error.or(session.close(cx).await.err());
        }

        first_error.map_or(Ok(()), Err)
    }
}

impl SessionRepo for MemorySessionRepo {
    type Metadata = SessionMetadata;
    type CreateOptions = MemoryCreateOptions;
    type ListOptions = MemoryListOptions;

    fn create<'a>(
        &'a self,
        options: Self::CreateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let metadata = if let Some(metadata) = options.metadata {
                metadata
            } else {
                let created_at = now_millis()?;
                SessionMetadata {
                    id: self.id_generator.next(Some(created_at))?,
                    created_at,
                    storage_version: MemoryStorage::STORAGE_VERSION,
                    cwd: None,
                    parent_session_id: None,
                    legacy_parent_session_path: None,
                }
            };
            let id = metadata.id.clone();
            let _reservation = self.reserve_id(&id).await?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let state = Arc::new(Mutex::new(InMemoryStorageState::new()));
            let open = Arc::new(AtomicBool::new(true));
            let mut record = Record {
                metadata,
                state,
                open,
                session: None,
            };
            let session = self.make_session(&record);
            record.session = Some(Arc::clone(&session));
            let mut sessions = self.sessions.lock().await;
            self.ensure_open()?;
            sessions.insert(id, record);
            Ok(session as Arc<dyn Session>)
        })
    }

    fn open<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let mut sessions = self.sessions.lock().await;
            self.ensure_open()?;
            let record = sessions.get_mut(&metadata.id).ok_or_else(|| {
                SessionError::Backend(StorageFailure::new(
                    StorageErrorCode::NotFound,
                    "session not found",
                ))
            })?;
            if record.open.swap(true, Ordering::AcqRel) {
                return Err(SessionError::Invariant(format!(
                    "session is already open: {}",
                    metadata.id
                )));
            }
            let session = self.make_session(record);
            record.session = Some(Arc::clone(&session));
            Ok(session as Arc<dyn Session>)
        })
    }

    fn list<'a>(
        &'a self,
        _options: Option<Self::ListOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Self::Metadata>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let sessions = self.sessions.lock().await;
            self.ensure_open()?;
            Ok(sessions
                .values()
                .map(|record| record.metadata.clone())
                .collect())
        })
    }

    fn delete<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let mut sessions = self.sessions.lock().await;
            self.ensure_open()?;
            let record = sessions.get(&metadata.id).ok_or_else(|| {
                SessionError::Backend(StorageFailure::new(
                    StorageErrorCode::NotFound,
                    "session not found",
                ))
            })?;
            if record.open.load(Ordering::Acquire) {
                return Err(SessionError::Invariant(format!(
                    "session is open: {}",
                    metadata.id
                )));
            }
            sessions.remove(&metadata.id);
            Ok(())
        })
    }

    fn fork<'a>(
        &'a self,
        source: &'a Self::Metadata,
        options: ForkOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            if let ForkOptions::Branch { branch, .. } = &options {
                validate_branch_name(branch)?;
            }
            let (source_metadata, source_state) = {
                let sessions = self.sessions.lock().await;
                let record = sessions.get(&source.id).ok_or_else(|| {
                    SessionError::Backend(StorageFailure::new(
                        StorageErrorCode::NotFound,
                        "session not found",
                    ))
                })?;
                (record.metadata.clone(), Arc::clone(&record.state))
            };
            let created_at = now_millis()?;
            let id = match &options {
                ForkOptions::Branch { id: Some(id), .. } | ForkOptions::Tree { id: Some(id) } => {
                    id.clone()
                }
                ForkOptions::Branch { id: None, .. } | ForkOptions::Tree { id: None } => {
                    self.id_generator.next(Some(created_at))?
                }
            };
            let _reservation = self.reserve_id(&id).await?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let source_storage = MemoryStorage::attach(source_state);
            let source_snapshot = source_storage.capture_fork_source(cx).await?;
            let snapshot = create_fork_snapshot(&source_snapshot, &options)?;
            let destination_state = Arc::new(Mutex::new(InMemoryStorageState::from_fork_snapshot(
                &snapshot,
            )?));
            let metadata = SessionMetadata {
                id: id.clone(),
                created_at,
                storage_version: MemoryStorage::STORAGE_VERSION,
                cwd: source_metadata.cwd,
                parent_session_id: Some(source_metadata.id),
                legacy_parent_session_path: None,
            };
            let open = Arc::new(AtomicBool::new(true));
            let mut record = Record {
                metadata,
                state: destination_state,
                open,
                session: None,
            };
            let session = self.make_session(&record);
            record.session = Some(Arc::clone(&session));
            let mut sessions = self.sessions.lock().await;
            self.ensure_open()?;
            sessions.insert(id, record);
            Ok(session as Arc<dyn Session>)
        })
    }
}

#[cfg(test)]
#[tokio::test]
async fn dropping_pending_custom_id_fork_releases_reservation() -> Result<(), SessionError> {
    // `MemorySessionRepo::create` reserves an id and then performs only
    // synchronous work and `sessions` lock acquisition before inserting the
    // record, so there is no externally controllable await point *after*
    // reservation for a create future to be dropped from. Only fork's
    // `capture_fork_source` call — which must lock the source session state —
    // provides a real post-reservation block point, so this case uses that.
    use std::sync::Arc;
    use std::task::{Context as PollContext, Poll};

    use futures::task::noop_waker_ref;

    use crate::context::Context;
    use crate::session::{ForkOptions, SessionRepo};

    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let source = repo
        .create(
            MemoryCreateOptions {
                metadata: Some(SessionMetadata {
                    id: "fork-source".to_owned(),
                    created_at: 1,
                    storage_version: MemoryStorage::STORAGE_VERSION,
                    cwd: None,
                    parent_session_id: None,
                    legacy_parent_session_path: None,
                }),
            },
            &cx,
        )
        .await?;
    let source_metadata = source.metadata().clone();
    let source_state = {
        let sessions = repo.sessions.lock().await;
        let record = sessions
            .get(&source_metadata.id)
            .ok_or_else(|| SessionError::Invariant("source record should exist".to_owned()))?;
        Arc::clone(&record.state)
    };
    let source_guard = source_state.lock().await;

    let options = ForkOptions::Tree {
        id: Some("retry-fork".to_owned()),
    };
    let mut pending = Box::pin(repo.fork(&source_metadata, options.clone(), &cx));
    let mut poll_cx = PollContext::from_waker(noop_waker_ref());
    assert!(matches!(pending.as_mut().poll(&mut poll_cx), Poll::Pending));

    match repo.fork(&source_metadata, options.clone(), &cx).await {
        Err(SessionError::Invariant(message)) => {
            assert_eq!(message, "session already exists: retry-fork")
        }
        Err(_) | Ok(_) => {
            return Err(SessionError::Invariant(
                "expected pending reservation to reject duplicate fork".to_owned(),
            ));
        }
    }

    drop(pending);
    drop(source_guard);
    let retried = repo.fork(&source_metadata, options, &cx).await?;
    assert_eq!(retried.metadata().id, "retry-fork");
    Ok(())
}
#[cfg(test)]
#[tokio::test]
async fn dropping_commit_observer_retains_committed_write_after_close_reopen()
-> Result<(), SessionError> {
    // Once `begin_mutation` admits a commit, the owned task carries the
    // mutation permit through backend I/O and settlement even if only the
    // caller's observer future is dropped: close must drain that task instead
    // of bypassing it, and the reopened session must expose the committed
    // branch tip, entry, and name value. The real backend state mutex is held
    // so the spawned commit task parks on genuine storage I/O.
    use std::task::{Context as PollContext, Poll};

    use futures::task::noop_waker_ref;

    use super::entry::{NewEntry, NewEntryBody};
    use super::ids::LaneName;

    let repo = MemorySessionRepo::new();
    let cx = Context::background();
    let metadata = SessionMetadata {
        id: "dropped-commit-observer".to_owned(),
        created_at: 1,
        storage_version: MemoryStorage::STORAGE_VERSION,
        cwd: None,
        parent_session_id: None,
        legacy_parent_session_path: None,
    };
    let session = repo
        .create(
            MemoryCreateOptions {
                metadata: Some(metadata.clone()),
            },
            &cx,
        )
        .await?;

    let state = {
        let sessions = repo.sessions.lock().await;
        let record = sessions
            .get(&metadata.id)
            .ok_or_else(|| SessionError::Invariant("session record should exist".to_owned()))?;
        Arc::clone(&record.state)
    };
    let state_guard = state.lock().await;

    let lane = LaneName::from("main");
    let entry_id = EntryId::from(session.id_generator().next(None)?);
    let writes = vec![
        Write::Entry {
            entry: NewEntry {
                id: entry_id.clone(),
                parent_id: None,
                body: NewEntryBody::Custom {
                    custom_type: "note".to_owned(),
                    data: Some(serde_json::json!({"body": "durable"})),
                },
            },
        },
        super::write::set_value(
            &super::address::branch_tip(lane.as_str()),
            &Some(entry_id.clone()),
        )?,
        super::write::set_value(
            &super::address::session_name(),
            &"dropped-commit-observer".to_owned(),
        )?,
    ];

    let mutation = session.begin_mutation(&cx).await?;
    let mut pending = mutation.commit(writes, &cx);
    let mut poll_cx = PollContext::from_waker(noop_waker_ref());
    assert!(matches!(pending.as_mut().poll(&mut poll_cx), Poll::Pending));

    // Drop only the observer; the owned commit task keeps the mutation permit.
    drop(pending);

    // With the backend state mutex still held, the admitted commit owns the
    // mutation permit until its backend I/O settles, so close can only line
    // up behind it instead of bypassing it.
    let mut close_waiter = session.close(&cx);
    assert!(matches!(
        close_waiter.as_mut().poll(&mut poll_cx),
        Poll::Pending
    ));

    drop(state_guard);
    close_waiter.await?;

    let reopened = repo.open(&metadata, &cx).await?;
    let branch = reopened.branch(&lane, &cx).await?.ok_or_else(|| {
        SessionError::Invariant("committed branch should survive close and reopen".to_owned())
    })?;
    assert_eq!(branch.get_tip_id(&cx).await?, Some(entry_id.clone()));
    let committed = reopened.get_entry(&entry_id, &cx).await?.ok_or_else(|| {
        SessionError::Invariant(
            "dropped-observer commit should be settled by close and visible after reopen"
                .to_owned(),
        )
    })?;
    assert_eq!(committed.custom_type(), Some("note"));
    match committed {
        Entry::Custom {
            data: Some(data), ..
        } => assert_eq!(data, serde_json::json!({"body": "durable"})),
        other => {
            return Err(SessionError::Invariant(format!(
                "reopened entry should be the committed custom note, got {other:?}"
            )));
        }
    }
    assert_eq!(
        reopened.get_name(&cx).await?,
        Some("dropped-commit-observer".to_owned())
    );
    Ok(())
}
