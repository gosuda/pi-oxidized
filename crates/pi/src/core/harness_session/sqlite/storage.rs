use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use rusqlite::{Connection, Row, Transaction, TransactionBehavior, named_params, params};
use tokio::sync::Notify;

use pi_agent::context::Context;
use pi_agent::session::{
    AddressKind, CommittedIdView, CommittedListWrite, CommittedValueWrite, CommittedWrite, Entry,
    EntryId, EntryScan, EntryStructure, EntryType, ForkDestinationSnapshot, ForkSource,
    ForkSourceSnapshot, LIST_READ_DEFAULT_LIMIT, LIST_READ_MAX_LIMIT, ListReadOptions, RawAddress,
    RawListElement, RawStoredValue, ScanOrder, SessionError, SessionMetadata, SessionStats,
    Storage, StorageBranchScan, StorageErrorCode, StorageFailure, UsageRow, UsageScan, Write,
    commit_writes, fork_snapshot_writes, validate_committed_writes, validate_replayed_writes,
};

use super::repo::SqliteSessionMetadata;
use super::schema;

fn invariant(message: impl Into<String>) -> SessionError {
    SessionError::Invariant(message.into())
}

pub(crate) fn aborted() -> SessionError {
    SessionError::Backend(StorageFailure::new(
        StorageErrorCode::Aborted,
        "operation cancelled",
    ))
}

fn closed() -> SessionError {
    SessionError::Backend(StorageFailure::new(
        StorageErrorCode::Closed,
        "session storage is closed",
    ))
}

fn not_found(message: impl Into<String>) -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::NotFound, message))
}

fn db_failure(operation: &str, error: rusqlite::Error) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Io,
        message: operation.to_owned(),
        source: Some(Arc::new(error)),
    })
}

pub(crate) fn join_failure(operation: &str, error: tokio::task::JoinError) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Io,
        message: operation.to_owned(),
        source: Some(Arc::new(error)),
    })
}

fn json_failure(code: StorageErrorCode, operation: &str, error: serde_json::Error) -> SessionError {
    SessionError::Backend(StorageFailure {
        code,
        message: operation.to_owned(),
        source: Some(Arc::new(error)),
    })
}

fn lock_failure(operation: &str) -> SessionError {
    invariant(format!("{operation}: SQLite mutex is poisoned"))
}

fn sqlite_seq(value: u64) -> Result<i64, SessionError> {
    i64::try_from(value).map_err(|_| invariant("storage sequence exceeds SQLite INTEGER range"))
}

fn from_sqlite_seq(value: i64) -> Result<u64, SessionError> {
    u64::try_from(value).map_err(|_| invariant("stored SQLite sequence is negative"))
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
        .map_err(|_| invariant("system clock timestamp exceeds i64 range"))
}

struct Inner {
    connection: StdMutex<Option<Connection>>,
    admission: StdMutex<()>,
    active: AtomicUsize,
    closed: AtomicBool,
    drained: Notify,
}

struct OperationPermit {
    inner: Arc<Inner>,
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        let previous = self.inner.active.fetch_sub(1, Ordering::AcqRel);
        if previous <= 1 {
            self.inner.drained.notify_waiters();
        }
    }
}

impl Inner {
    fn admit(self: &Arc<Self>) -> Result<OperationPermit, SessionError> {
        let _admission = self.admission.lock().map_err(|_| lock_failure("admit"))?;
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                active.checked_add(1)
            })
            .map_err(|_| invariant("SQLite operation counter overflow"))?;
        Ok(OperationPermit {
            inner: Arc::clone(self),
        })
    }

    async fn drain(&self) {
        loop {
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            let notified = self.drained.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// SQLite-backed implementation of the backend-neutral storage contract.
pub struct SqliteStorage {
    inner: Arc<Inner>,
    session_id: String,
}

impl SqliteStorage {
    pub(crate) fn from_connection(connection: Connection, session_id: String) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                connection: StdMutex::new(Some(connection)),
                admission: StdMutex::new(()),
                active: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                drained: Notify::new(),
            }),
            session_id,
        })
    }

    fn run_blocking<'a, T, F>(
        &'a self,
        cx: &'a Context,
        operation: F,
    ) -> BoxFuture<'a, Result<T, SessionError>>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            cx.check().map_err(|_| aborted())?;
            let permit = inner.admit()?;
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut connection = inner
                    .connection
                    .lock()
                    .map_err(|_| lock_failure("run SQLite operation"))?;
                let connection = connection.as_mut().ok_or_else(closed)?;
                operation(connection)
            });
            task.await
                .map_err(|error| join_failure("SQLite task failed", error))?
        })
    }
}

impl Storage for SqliteStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<pi_agent::session::CommitResult, SessionError>> {
        let session_id = self.session_id.clone();
        self.run_blocking(cx, move |connection| {
            apply_commit_on_connection(connection, &session_id, writes)
        })
    }

    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        let session_id = self.session_id.clone();
        let ids = ids.to_vec();
        self.run_blocking(cx, move |connection| {
            read_entries(connection, &session_id, &ids)
        })
    }

    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        let session_id = self.session_id.clone();
        let address = address.clone();
        self.run_blocking(cx, move |connection| {
            read_scalar_value(connection, &session_id, &address)
        })
    }

    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        let session_id = self.session_id.clone();
        let prefix = prefix.clone();
        self.run_blocking(cx, move |connection| {
            scan_scalar_values(connection, &session_id, &prefix)
        })
    }

    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        let session_id = self.session_id.clone();
        let address = address.clone();
        self.run_blocking(cx, move |connection| {
            read_list_values(connection, &session_id, &address, options)
        })
    }

    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        let session_id = self.session_id.clone();
        let query = query.clone();
        self.run_blocking(cx, move |connection| {
            scan_branch_rows(connection, &session_id, &query)?
                .into_iter()
                .map(decode_entry_row)
                .collect()
        })
    }

    fn scan_branch_structure<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
        let session_id = self.session_id.clone();
        let query = query.clone();
        self.run_blocking(cx, move |connection| {
            scan_branch_rows(connection, &session_id, &query)?
                .into_iter()
                .map(entry_structure_from_row)
                .collect()
        })
    }

    fn scan_entries<'a>(
        &'a self,
        query: &'a EntryScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        let session_id = self.session_id.clone();
        let query = query.clone();
        self.run_blocking(cx, move |connection| {
            scan_entry_rows(connection, &session_id, &query)?
                .into_iter()
                .map(decode_entry_row)
                .collect()
        })
    }

    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        let session_id = self.session_id.clone();
        let query = query.clone();
        self.run_blocking(cx, move |connection| {
            scan_usage_rows(connection, &session_id, &query)
        })
    }

    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        let session_id = self.session_id.clone();
        self.run_blocking(cx, move |connection| read_stats(connection, &session_id))
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            cx.check().map_err(|_| aborted())?;
            {
                let _admission = inner.admission.lock().map_err(|_| lock_failure("close"))?;
                inner.closed.store(true, Ordering::Release);
            }
            inner.drain().await;
            let connection = {
                let mut connection = inner
                    .connection
                    .lock()
                    .map_err(|_| lock_failure("close connection"))?;
                connection.take()
            };
            if let Some(connection) = connection {
                let result = tokio::task::spawn_blocking(move || connection.close())
                    .await
                    .map_err(|source| join_failure("SQLite close", source))?;
                if let Err((_connection, error)) = result {
                    return Err(db_failure("failed to close SQLite connection", error));
                }
            }
            Ok(())
        })
    }
}

impl ForkSource for SqliteStorage {
    fn capture_fork_source<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<ForkSourceSnapshot, SessionError>> {
        let session_id = self.session_id.clone();
        self.run_blocking(cx, move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(|error| db_failure("failed to begin SQLite fork snapshot", error))?;
            let entries = read_all_entry_rows(&transaction, &session_id)?
                .into_iter()
                .map(decode_entry_row)
                .collect::<Result<Vec<_>, _>>()?;
            let values = read_all_scalar_values(&transaction, &session_id)?;
            transaction
                .commit()
                .map_err(|error| db_failure("failed to finish SQLite fork snapshot", error))?;
            Ok(ForkSourceSnapshot {
                entries,
                values,
                entries_complete: true,
            })
        })
    }
}

fn apply_commit_on_connection(
    connection: &mut Connection,
    session_id: &str,
    writes: Vec<Write>,
) -> Result<pi_agent::session::CommitResult, SessionError> {
    let timestamp = now_millis()?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| db_failure("failed to begin SQLite commit transaction", error))?;
    let first_seq = read_next_seq(&transaction, session_id)?;
    let id_view = load_id_view(&transaction, session_id, first_seq)?;
    validate_committed_writes(&writes, first_seq, &id_view)?;
    let (committed, seqs) = commit_writes(writes, first_seq, timestamp)?;
    let mut stats = read_stats(&transaction, session_id)?;
    apply_committed_rows(&transaction, session_id, &committed, &mut stats)?;
    let next_seq = first_seq
        .checked_add(
            u64::try_from(committed.len()).map_err(|_| invariant("commit sequence overflow"))?,
        )
        .ok_or_else(|| invariant("commit sequence overflow"))?;
    update_session_totals(&transaction, session_id, next_seq, &stats)?;
    transaction
        .commit()
        .map_err(|error| db_failure("failed to commit SQLite transaction", error))?;
    Ok(pi_agent::session::CommitResult {
        first_seq,
        seqs,
        timestamp,
        stats,
    })
}
#[derive(Clone)]
struct EntryRow {
    id: String,
    parent_id: Option<String>,
    seq: i64,
    entry_type: String,
    custom_type: Option<String>,
    timestamp: i64,
    payload: String,
}

fn read_entry_row_from_sql(row: &Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        seq: row.get(2)?,
        entry_type: row.get(3)?,
        custom_type: row.get(4)?,
        timestamp: row.get(5)?,
        payload: row.get(6)?,
    })
}

fn entry_type_name(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message { .. } => "message",
        Entry::Compaction { .. } => "compaction",
        Entry::BranchSummary { .. } => "branch_summary",
        Entry::Custom { .. } => "custom",
    }
}

fn parse_entry_type(value: &str) -> Result<EntryType, SessionError> {
    match value {
        "message" => Ok(EntryType::Message),
        "compaction" => Ok(EntryType::Compaction),
        "branch_summary" => Ok(EntryType::BranchSummary),
        "custom" => Ok(EntryType::Custom),
        other => Err(SessionError::Backend(StorageFailure::new(
            StorageErrorCode::Corrupt,
            format!("unknown stored entry type {other}"),
        ))),
    }
}

fn json_value<T: serde::Serialize>(
    operation: &str,
    value: &T,
) -> Result<serde_json::Value, SessionError> {
    serde_json::to_value(value)
        .map_err(|error| json_failure(StorageErrorCode::Corrupt, operation, error))
}

fn entry_payload(entry: &Entry) -> Result<String, SessionError> {
    let mut payload = serde_json::Map::new();
    match entry {
        Entry::Message {
            message, terminate, ..
        } => {
            payload.insert(
                "message".to_owned(),
                json_value("failed to encode message entry", message)?,
            );
            if *terminate {
                payload.insert("terminate".to_owned(), serde_json::Value::Bool(true));
            }
        }
        Entry::Compaction {
            summary,
            retained_tail,
            tokens_before,
            details,
            usage,
            from_hook,
            ..
        } => {
            payload.insert(
                "summary".to_owned(),
                serde_json::Value::String(summary.clone()),
            );
            payload.insert(
                "retainedTail".to_owned(),
                json_value("failed to encode compaction tail", retained_tail)?,
            );
            payload.insert(
                "tokensBefore".to_owned(),
                json_value("failed to encode compaction token count", tokens_before)?,
            );
            if let Some(details) = details {
                payload.insert("details".to_owned(), details.clone());
            }
            if let Some(usage) = usage {
                payload.insert(
                    "usage".to_owned(),
                    json_value("failed to encode compaction usage", usage)?,
                );
            }
            payload.insert("fromHook".to_owned(), serde_json::Value::Bool(*from_hook));
        }
        Entry::BranchSummary {
            from_id,
            summary,
            details,
            usage,
            from_hook,
            ..
        } => {
            if let Some(from_id) = from_id {
                payload.insert(
                    "fromId".to_owned(),
                    serde_json::Value::String(from_id.to_string()),
                );
            }
            payload.insert(
                "summary".to_owned(),
                serde_json::Value::String(summary.clone()),
            );
            if let Some(details) = details {
                payload.insert("details".to_owned(), details.clone());
            }
            if let Some(usage) = usage {
                payload.insert(
                    "usage".to_owned(),
                    json_value("failed to encode branch summary usage", usage)?,
                );
            }
            payload.insert("fromHook".to_owned(), serde_json::Value::Bool(*from_hook));
        }
        Entry::Custom { data, .. } => {
            if let Some(data) = data {
                payload.insert("data".to_owned(), data.clone());
            }
        }
    }
    serde_json::to_string(&serde_json::Value::Object(payload)).map_err(|error| {
        json_failure(
            StorageErrorCode::Corrupt,
            "failed to encode entry payload",
            error,
        )
    })
}

fn decode_entry_row(row: EntryRow) -> Result<Entry, SessionError> {
    let mut payload = serde_json::from_str::<serde_json::Value>(&row.payload).map_err(|error| {
        json_failure(
            StorageErrorCode::Corrupt,
            "stored entry payload decode failed",
            error,
        )
    })?;
    let object = payload.as_object_mut().ok_or_else(|| {
        SessionError::Backend(StorageFailure::new(
            StorageErrorCode::Corrupt,
            format!("stored entry {} payload is not an object", row.id),
        ))
    })?;
    object.insert("type".to_owned(), serde_json::Value::String(row.entry_type));
    object.insert("id".to_owned(), serde_json::Value::String(row.id));
    match row.parent_id {
        Some(parent) => object.insert("parentId".to_owned(), serde_json::Value::String(parent)),
        None => object.insert("parentId".to_owned(), serde_json::Value::Null),
    };
    object.insert(
        "seq".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(row.seq)),
    );
    object.insert(
        "timestamp".to_owned(),
        serde_json::Value::Number(serde_json::Number::from(row.timestamp)),
    );
    if let Some(custom_type) = row.custom_type {
        object.insert(
            "customType".to_owned(),
            serde_json::Value::String(custom_type),
        );
    }
    serde_json::from_value(payload).map_err(|error| {
        json_failure(
            StorageErrorCode::Corrupt,
            "stored entry decode failed",
            error,
        )
    })
}

fn entry_structure_from_row(row: EntryRow) -> Result<EntryStructure, SessionError> {
    Ok(EntryStructure {
        id: EntryId::from(row.id),
        parent_id: row.parent_id.map(EntryId::from),
        seq: from_sqlite_seq(row.seq)?,
        timestamp: row.timestamp,
        entry_type: parse_entry_type(&row.entry_type)?,
        custom_type: row.custom_type,
    })
}

fn insert_entry_row(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    tx.execute(
        "INSERT INTO entries (session_id, id, parent_id, seq, type, custom_type, timestamp, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            session_id,
            entry.id().as_str(),
            entry.parent_id().map(EntryId::as_str),
            sqlite_seq(entry.seq())?,
            entry_type_name(entry),
            entry.custom_type(),
            entry.timestamp(),
            entry_payload(entry)?,
        ],
    )
    .map_err(|error| db_failure("failed to insert SQLite entry", error))?;
    Ok(())
}

fn read_entry_row(
    connection: &Connection,
    session_id: &str,
    entry_id: &EntryId,
) -> Result<Option<EntryRow>, SessionError> {
    let result = connection.query_row(
        "SELECT id, parent_id, seq, type, custom_type, timestamp, payload FROM entries WHERE session_id = ?1 AND id = ?2",
        params![session_id, entry_id.as_str()],
        read_entry_row_from_sql,
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(db_failure("failed to read SQLite entry", error)),
    }
}

fn read_all_entry_rows(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<EntryRow>, SessionError> {
    let mut statement = connection
        .prepare("SELECT id, parent_id, seq, type, custom_type, timestamp, payload FROM entries WHERE session_id = ?1 ORDER BY seq ASC")
        .map_err(|error| db_failure("failed to prepare SQLite entry scan", error))?;
    let rows = statement
        .query_map(params![session_id], read_entry_row_from_sql)
        .map_err(|error| db_failure("failed to scan SQLite entries", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|error| db_failure("failed to decode SQLite entry row", error))?);
    }
    Ok(output)
}

fn read_entries(
    connection: &Connection,
    session_id: &str,
    ids: &[EntryId],
) -> Result<HashMap<EntryId, Entry>, SessionError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = (1..=ids.len())
        .map(|index| format!("?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT id, parent_id, seq, type, custom_type, timestamp, payload FROM entries WHERE session_id = ?1 AND id IN ({placeholders})"
    );
    let mut parameters = Vec::with_capacity(ids.len() + 1);
    parameters.push(session_id);
    parameters.extend(ids.iter().map(EntryId::as_str));
    let mut statement = connection
        .prepare(&query)
        .map_err(|error| db_failure("failed to prepare SQLite entry lookup", error))?;
    let rows = statement
        .query_map(
            rusqlite::params_from_iter(parameters),
            read_entry_row_from_sql,
        )
        .map_err(|error| db_failure("failed to query SQLite entries", error))?;
    let mut by_id = HashMap::new();
    for row in rows {
        let entry = decode_entry_row(
            row.map_err(|error| db_failure("failed to decode SQLite entry row", error))?,
        )?;
        by_id.insert(entry.id().clone(), entry);
    }
    Ok(ids
        .iter()
        .filter_map(|id| by_id.remove(id).map(|entry| (id.clone(), entry)))
        .collect())
}

fn effective_scan_limit(limit: Option<u32>) -> u32 {
    limit
        .unwrap_or(LIST_READ_DEFAULT_LIMIT)
        .min(LIST_READ_MAX_LIMIT)
}

fn scan_entry_rows(
    connection: &Connection,
    session_id: &str,
    query: &EntryScan,
) -> Result<Vec<EntryRow>, SessionError> {
    let order = match query.order.unwrap_or(ScanOrder::Asc) {
        ScanOrder::Asc => "ASC",
        ScanOrder::Desc => "DESC",
    };
    let entry_type = query.entry_type.map(|value| match value {
        EntryType::Message => "message",
        EntryType::Compaction => "compaction",
        EntryType::BranchSummary => "branch_summary",
        EntryType::Custom => "custom",
    });
    let from_seq = query.from_seq.map(sqlite_seq).transpose()?;
    let to_seq = query.to_seq.map(sqlite_seq).transpose()?;
    let limit = i64::from(effective_scan_limit(query.limit));
    let sql = format!(
        "SELECT id, parent_id, seq, type, custom_type, timestamp, payload FROM entries WHERE session_id = :session AND (:from_seq IS NULL OR seq >= :from_seq) AND (:to_seq IS NULL OR seq <= :to_seq) AND (:entry_type IS NULL OR type = :entry_type) AND (:custom_type IS NULL OR custom_type = :custom_type) ORDER BY seq {order} LIMIT :limit"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| db_failure("failed to prepare SQLite entry scan", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":session": session_id,
                ":from_seq": from_seq,
                ":to_seq": to_seq,
                ":entry_type": entry_type,
                ":custom_type": query.custom_type.as_deref(),
                ":limit": limit,
            },
            read_entry_row_from_sql,
        )
        .map_err(|error| db_failure("failed to scan SQLite entries", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|error| db_failure("failed to decode SQLite entry row", error))?);
    }
    Ok(output)
}

fn next_prefix_boundary(prefix: &str) -> Option<String> {
    let mut code_points: Vec<char> = prefix.chars().collect();
    while let Some(code_point) = code_points.pop() {
        let value = u32::from(code_point);
        let next_value = if value == 0x10_ffff {
            None
        } else if value == 0xD7FF {
            Some(0xE000)
        } else {
            value.checked_add(1)
        };
        if let Some(next) = next_value.and_then(char::from_u32) {
            code_points.push(next);
            return Some(code_points.into_iter().collect());
        }
    }
    None
}

#[derive(Clone)]
struct ScalarValueRow {
    namespace: String,
    key: String,
    seq: i64,
    value: String,
}

fn scalar_row_from_sql(row: &Row<'_>) -> rusqlite::Result<ScalarValueRow> {
    Ok(ScalarValueRow {
        namespace: row.get(0)?,
        key: row.get(1)?,
        seq: row.get(2)?,
        value: row.get(3)?,
    })
}

fn decode_scalar_row(row: ScalarValueRow) -> Result<RawStoredValue, SessionError> {
    Ok(RawStoredValue {
        namespace: row.namespace,
        key: row.key,
        kind: AddressKind::Value,
        value: serde_json::from_str(&row.value).map_err(|error| {
            json_failure(
                StorageErrorCode::Corrupt,
                "stored scalar value decode failed",
                error,
            )
        })?,
        seq: from_sqlite_seq(row.seq)?,
    })
}

fn read_scalar_value(
    connection: &Connection,
    session_id: &str,
    address: &RawAddress,
) -> Result<Option<RawStoredValue>, SessionError> {
    let result = connection.query_row(
        "SELECT namespace, key, seq, value FROM scalar_values WHERE session_id = ?1 AND namespace = ?2 AND key = ?3",
        params![session_id, address.namespace, address.key],
        scalar_row_from_sql,
    );
    match result {
        Ok(row) => decode_scalar_row(row).map(Some),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(db_failure("failed to read SQLite scalar value", error)),
    }
}

fn read_all_scalar_values(
    connection: &Connection,
    session_id: &str,
) -> Result<Vec<RawStoredValue>, SessionError> {
    let mut statement = connection
        .prepare("SELECT namespace, key, seq, value FROM scalar_values WHERE session_id = ?1 ORDER BY seq ASC")
        .map_err(|error| db_failure("failed to prepare SQLite scalar scan", error))?;
    let rows = statement
        .query_map(params![session_id], scalar_row_from_sql)
        .map_err(|error| db_failure("failed to scan SQLite scalar values", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(decode_scalar_row(row.map_err(|error| {
            db_failure("failed to decode SQLite scalar row", error)
        })?)?);
    }
    Ok(output)
}

fn scan_scalar_values(
    connection: &Connection,
    session_id: &str,
    prefix: &RawAddress,
) -> Result<Vec<RawStoredValue>, SessionError> {
    let boundary = next_prefix_boundary(&prefix.key);
    let sql = "SELECT namespace, key, seq, value FROM scalar_values WHERE session_id = :session AND namespace = :namespace AND key >= :lower AND (:upper IS NULL OR key < :upper) ORDER BY key ASC";
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| db_failure("failed to prepare SQLite scalar scan", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":session": session_id,
                ":namespace": prefix.namespace.as_str(),
                ":lower": prefix.key.as_str(),
                ":upper": boundary.as_deref(),
            },
            scalar_row_from_sql,
        )
        .map_err(|error| db_failure("failed to scan SQLite scalar values", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(decode_scalar_row(row.map_err(|error| {
            db_failure("failed to decode SQLite scalar row", error)
        })?)?);
    }
    Ok(output)
}

fn read_list_values(
    connection: &Connection,
    session_id: &str,
    address: &RawAddress,
    options: Option<ListReadOptions>,
) -> Result<Vec<RawListElement>, SessionError> {
    let options = pi_agent::session::resolve_list_read_options(options)
        .map_err(|_| invariant("list limit must be positive"))?;
    let limit = i64::from(options.limit);
    let cursor = options
        .cursor
        .map(|value| sqlite_seq(value.seq))
        .transpose()?;
    let (order, comparator) = match options.order {
        ScanOrder::Asc => ("ASC", ">"),
        ScanOrder::Desc => ("DESC", "<"),
    };
    let sql = format!(
        "SELECT seq, value FROM list_values WHERE session_id = :session AND namespace = :namespace AND key = :key AND (:cursor IS NULL OR seq {comparator} :cursor) ORDER BY seq {order} LIMIT :limit"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| db_failure("failed to prepare SQLite list read", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":session": session_id,
                ":namespace": address.namespace.as_str(),
                ":key": address.key.as_str(),
                ":cursor": cursor,
                ":limit": limit,
            },
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| db_failure("failed to read SQLite list", error))?;
    let mut output = Vec::new();
    for row in rows {
        let (seq, value) =
            row.map_err(|error| db_failure("failed to decode SQLite list row", error))?;
        output.push(RawListElement {
            seq: from_sqlite_seq(seq)?,
            value: serde_json::from_str(&value).map_err(|error| {
                json_failure(
                    StorageErrorCode::Corrupt,
                    "stored list value decode failed",
                    error,
                )
            })?,
        });
    }
    Ok(output)
}

#[derive(Clone)]
struct UsageRowSql {
    id: String,
    seq: i64,
    entry_id: Option<String>,
    adjustment: i64,
    usage: String,
    details: Option<String>,
}

fn usage_row_from_sql(row: &Row<'_>) -> rusqlite::Result<UsageRowSql> {
    Ok(UsageRowSql {
        id: row.get(0)?,
        seq: row.get(1)?,
        entry_id: row.get(2)?,
        adjustment: row.get(3)?,
        usage: row.get(4)?,
        details: row.get(5)?,
    })
}

fn decode_usage_row(row: UsageRowSql) -> Result<UsageRow, SessionError> {
    Ok(UsageRow {
        id: pi_agent::session::UsageId::from(row.id),
        seq: from_sqlite_seq(row.seq)?,
        usage: serde_json::from_str(&row.usage).map_err(|error| {
            json_failure(
                StorageErrorCode::Corrupt,
                "stored usage decode failed",
                error,
            )
        })?,
        entry_id: row.entry_id.map(pi_agent::session::EntryId::from),
        adjustment: row.adjustment != 0,
        details: row
            .details
            .map(|details| {
                serde_json::from_str(&details).map_err(|error| {
                    json_failure(
                        StorageErrorCode::Corrupt,
                        "stored usage details decode failed",
                        error,
                    )
                })
            })
            .transpose()?,
    })
}

fn scan_usage_rows(
    connection: &Connection,
    session_id: &str,
    query: &UsageScan,
) -> Result<Vec<UsageRow>, SessionError> {
    let order = match query.order.unwrap_or(ScanOrder::Asc) {
        ScanOrder::Asc => "ASC",
        ScanOrder::Desc => "DESC",
    };
    let from_seq = query.from_seq.map(sqlite_seq).transpose()?;
    let to_seq = query.to_seq.map(sqlite_seq).transpose()?;
    let limit = i64::from(effective_scan_limit(query.limit));
    let sql = format!(
        "SELECT id, seq, entry_id, adjustment, usage, details FROM usage_ledger WHERE session_id = :session AND (:from_seq IS NULL OR seq >= :from_seq) AND (:to_seq IS NULL OR seq <= :to_seq) ORDER BY seq {order} LIMIT :limit"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| db_failure("failed to prepare SQLite usage scan", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":session": session_id,
                ":from_seq": from_seq,
                ":to_seq": to_seq,
                ":limit": limit,
            },
            usage_row_from_sql,
        )
        .map_err(|error| db_failure("failed to scan SQLite usage", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(decode_usage_row(row.map_err(|error| {
            db_failure("failed to decode SQLite usage row", error)
        })?)?);
    }
    Ok(output)
}

fn add_usage(total: &mut pi_ai::Usage, add: &pi_ai::Usage) {
    total.input = total.input.saturating_add(add.input);
    total.output = total.output.saturating_add(add.output);
    total.cache_read = total.cache_read.saturating_add(add.cache_read);
    total.cache_write = total.cache_write.saturating_add(add.cache_write);
    total.total_tokens = total.total_tokens.saturating_add(add.total_tokens);
    total.cache_write1h = match (total.cache_write1h, add.cache_write1h) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (left, None) => left,
        (None, right) => right,
    };
    total.reasoning = match (total.reasoning, add.reasoning) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (left, None) => left,
        (None, right) => right,
    };
    total.cost.input += add.cost.input;
    total.cost.output += add.cost.output;
    total.cost.cache_read += add.cost.cache_read;
    total.cost.cache_write += add.cost.cache_write;
    total.cost.total += add.cost.total;
}

fn read_next_seq(connection: &Connection, session_id: &str) -> Result<u64, SessionError> {
    let result = connection.query_row(
        "SELECT next_seq FROM sessions WHERE id = ?1",
        params![session_id],
        |row| row.get::<_, i64>(0),
    );
    match result {
        Ok(value) => from_sqlite_seq(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(not_found(format!("unknown SQLite session {session_id}")))
        }
        Err(error) => Err(db_failure("failed to read SQLite sequence", error)),
    }
}

fn read_stats(connection: &Connection, session_id: &str) -> Result<SessionStats, SessionError> {
    let result = connection.query_row(
        "SELECT message_count, usage_payload FROM sessions WHERE id = ?1",
        params![session_id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    );
    let (message_count, usage_payload) = match result {
        Ok(value) => value,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(not_found(format!("unknown SQLite session {session_id}")));
        }
        Err(error) => return Err(db_failure("failed to read SQLite session stats", error)),
    };
    Ok(SessionStats {
        message_count: u64::try_from(message_count)
            .map_err(|_| invariant("stored message count is negative"))?,
        usage: serde_json::from_str(&usage_payload).map_err(|error| {
            json_failure(
                StorageErrorCode::Corrupt,
                "stored usage totals decode failed",
                error,
            )
        })?,
    })
}

fn update_session_totals(
    tx: &Transaction<'_>,
    session_id: &str,
    next_seq: u64,
    stats: &SessionStats,
) -> Result<(), SessionError> {
    let changed = tx
        .execute(
            "UPDATE sessions SET message_count = ?1, usage_payload = ?2, next_seq = ?3 WHERE id = ?4",
            params![
                i64::try_from(stats.message_count).map_err(|_| invariant("message count exceeds SQLite INTEGER range"))?,
                serde_json::to_string(&stats.usage).map_err(|error| json_failure(StorageErrorCode::Corrupt, "failed to encode usage totals", error))?,
                sqlite_seq(next_seq)?,
                session_id,
            ],
        )
        .map_err(|error| db_failure("failed to update SQLite session totals", error))?;
    if changed != 1 {
        return Err(invariant(format!(
            "expected one SQLite session update, changed {changed}"
        )));
    }
    Ok(())
}

struct SqliteIdView {
    ids: HashSet<String>,
    entry_ids: HashSet<EntryId>,
    next_seq: u64,
}

impl CommittedIdView for SqliteIdView {
    fn contains_id(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    fn contains_entry_id(&self, id: &EntryId) -> bool {
        self.entry_ids.contains(id)
    }

    fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

fn load_id_view(
    connection: &Transaction<'_>,
    session_id: &str,
    next_seq: u64,
) -> Result<SqliteIdView, SessionError> {
    let mut ids = HashSet::new();
    let mut entry_ids = HashSet::new();
    {
        let mut statement = connection
            .prepare("SELECT id FROM entries WHERE session_id = ?1")
            .map_err(|error| db_failure("failed to prepare SQLite entry identity scan", error))?;
        let rows = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))
            .map_err(|error| db_failure("failed to scan SQLite entry identities", error))?;
        for row in rows {
            let id =
                row.map_err(|error| db_failure("failed to decode SQLite entry identity", error))?;
            ids.insert(id.clone());
            entry_ids.insert(EntryId::from(id));
        }
    }
    let mut statement = connection
        .prepare("SELECT id FROM usage_ledger WHERE session_id = ?1")
        .map_err(|error| db_failure("failed to prepare SQLite usage identity scan", error))?;
    let rows = statement
        .query_map(params![session_id], |row| row.get::<_, String>(0))
        .map_err(|error| db_failure("failed to scan SQLite usage identities", error))?;
    for row in rows {
        ids.insert(
            row.map_err(|error| db_failure("failed to decode SQLite usage identity", error))?,
        );
    }
    Ok(SqliteIdView {
        ids,
        entry_ids,
        next_seq,
    })
}

fn insert_usage_row(
    tx: &Transaction<'_>,
    session_id: &str,
    row: &UsageRow,
) -> Result<(), SessionError> {
    let usage = serde_json::to_string(&row.usage).map_err(|error| {
        json_failure(
            StorageErrorCode::Corrupt,
            "failed to encode usage row",
            error,
        )
    })?;
    let details = row
        .details
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            json_failure(
                StorageErrorCode::Corrupt,
                "failed to encode usage details",
                error,
            )
        })?;
    tx.execute(
        "INSERT INTO usage_ledger (session_id, id, seq, entry_id, adjustment, usage, details) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            session_id,
            row.id.as_str(),
            sqlite_seq(row.seq)?,
            row.entry_id.as_ref().map(EntryId::as_str),
            i64::from(row.adjustment),
            usage,
            details,
        ],
    )
    .map_err(|error| db_failure("failed to insert SQLite usage row", error))?;
    Ok(())
}

fn apply_committed_rows(
    tx: &Transaction<'_>,
    session_id: &str,
    writes: &[CommittedWrite],
    stats: &mut SessionStats,
) -> Result<(), SessionError> {
    for write in writes {
        match write {
            CommittedWrite::Entry(entry) => {
                insert_entry_row(tx, session_id, entry)?;
                append_entry_to_branch_index(tx, session_id, entry)?;
                if entry.entry_type() == EntryType::Message {
                    stats.message_count = stats.message_count.saturating_add(1);
                }
            }
            CommittedWrite::Usage(row) => {
                insert_usage_row(tx, session_id, row)?;
                add_usage(&mut stats.usage, &row.usage);
            }
            CommittedWrite::Value(CommittedValueWrite::Set {
                seq,
                namespace,
                key,
                value,
            }) => {
                tx.execute(
                    "INSERT INTO scalar_values (session_id, namespace, key, seq, value) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(session_id, namespace, key) DO UPDATE SET seq = excluded.seq, value = excluded.value",
                    params![session_id, namespace, key, sqlite_seq(*seq)?, serde_json::to_string(value).map_err(|error| json_failure(StorageErrorCode::Corrupt, "failed to encode scalar value", error))?],
                )
                .map_err(|error| db_failure("failed to set SQLite scalar value", error))?;
            }
            CommittedWrite::Value(CommittedValueWrite::Delete { namespace, key, .. }) => {
                tx.execute(
                    "DELETE FROM scalar_values WHERE session_id = ?1 AND namespace = ?2 AND key = ?3",
                    params![session_id, namespace, key],
                )
                .map_err(|error| db_failure("failed to delete SQLite scalar value", error))?;
            }
            CommittedWrite::List(CommittedListWrite::Append {
                seq,
                namespace,
                key,
                value,
            }) => {
                tx.execute(
                    "INSERT INTO list_values (session_id, namespace, key, seq, value) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session_id, namespace, key, sqlite_seq(*seq)?, serde_json::to_string(value).map_err(|error| json_failure(StorageErrorCode::Corrupt, "failed to encode list value", error))?],
                )
                .map_err(|error| db_failure("failed to append SQLite list value", error))?;
            }
            CommittedWrite::List(CommittedListWrite::Delete { namespace, key, .. }) => {
                tx.execute(
                    "DELETE FROM list_values WHERE session_id = ?1 AND namespace = ?2 AND key = ?3",
                    params![session_id, namespace, key],
                )
                .map_err(|error| db_failure("failed to delete SQLite list value", error))?;
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct BranchSegment {
    branch_id: String,
    lower_seq: i64,
    upper_seq: i64,
}

fn read_branch_tip_for_parent(
    tx: &Transaction<'_>,
    session_id: &str,
    parent_id: &str,
) -> Result<Option<String>, SessionError> {
    let result = tx.query_row(
        "SELECT branch_id FROM branch_meta WHERE session_id = ?1 AND tip_entry_id = ?2",
        params![session_id, parent_id],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(branch) => Ok(Some(branch)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(db_failure("failed to read SQLite branch tip", error)),
    }
}

fn insert_branch_entry(
    tx: &Transaction<'_>,
    session_id: &str,
    branch_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    tx.execute(
        "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq, entry_type) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, branch_id, entry.id().as_str(), sqlite_seq(entry.seq())?, entry_type_name(entry)],
    )
    .map_err(|error| db_failure("failed to insert SQLite branch index row", error))?;
    Ok(())
}

fn append_entry_to_existing_branch(
    tx: &Transaction<'_>,
    session_id: &str,
    branch_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    insert_branch_entry(tx, session_id, branch_id, entry)?;
    let changed = tx
        .execute(
            "UPDATE branch_meta SET tip_entry_id = ?1, tip_seq = ?2 WHERE session_id = ?3 AND branch_id = ?4",
            params![entry.id().as_str(), sqlite_seq(entry.seq())?, session_id, branch_id],
        )
        .map_err(|error| db_failure("failed to update SQLite branch index", error))?;
    if changed != 1 {
        return Err(invariant(format!(
            "expected one branch update, changed {changed}"
        )));
    }
    Ok(())
}

fn create_root_branch(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    tx.execute(
        "INSERT INTO branch_meta (session_id, branch_id, tip_entry_id, tip_seq, base_branch_id, base_seq) VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
        params![session_id, entry.id().as_str(), entry.id().as_str(), sqlite_seq(entry.seq())?],
    )
    .map_err(|error| db_failure("failed to create SQLite root branch", error))?;
    insert_branch_entry(tx, session_id, entry.id().as_str(), entry)
}

fn read_branch_membership(
    connection: &Connection,
    session_id: &str,
    entry_id: &str,
) -> Result<(String, i64), SessionError> {
    let result = connection.query_row(
        "SELECT b.branch_id, b.entry_seq FROM branch_entries b JOIN branch_meta m ON m.session_id = b.session_id AND m.branch_id = b.branch_id WHERE b.session_id = ?1 AND b.entry_id = ?2 AND ((m.base_seq IS NULL AND b.entry_seq > 0) OR (m.base_seq IS NOT NULL AND b.entry_seq > m.base_seq)) AND b.entry_seq <= m.tip_seq ORDER BY m.tip_seq DESC, b.branch_id LIMIT 1",
        params![session_id, entry_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    );
    result.map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => {
            invariant(format!("branch cache missing entry {entry_id}"))
        }
        other => db_failure("failed to read SQLite branch membership", other),
    })
}

fn read_branch_meta(
    connection: &Connection,
    session_id: &str,
    branch_id: &str,
) -> Result<(String, i64, Option<String>, Option<i64>), SessionError> {
    let result = connection.query_row(
        "SELECT tip_entry_id, tip_seq, base_branch_id, base_seq FROM branch_meta WHERE session_id = ?1 AND branch_id = ?2",
        params![session_id, branch_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        },
    );
    result.map_err(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => {
            invariant(format!("branch metadata missing for {branch_id}"))
        }
        other => db_failure("failed to read SQLite branch metadata", other),
    })
}

fn read_branch_segments(
    connection: &Connection,
    session_id: &str,
    start: &str,
) -> Result<Vec<BranchSegment>, SessionError> {
    let (mut branch_id, mut upper_seq) = read_branch_membership(connection, session_id, start)?;
    let mut segments = Vec::new();
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(branch_id.clone()) {
            return Err(invariant("cycle in SQLite branch metadata"));
        }
        let (_tip_entry_id, _tip_seq, base_branch_id, base_seq) =
            read_branch_meta(connection, session_id, &branch_id)?;
        let lower_seq = base_seq.unwrap_or(0);
        segments.push(BranchSegment {
            branch_id: branch_id.clone(),
            lower_seq,
            upper_seq,
        });
        let Some(base_branch_id) = base_branch_id else {
            break;
        };
        let Some(base_seq) = base_seq else {
            return Err(invariant(format!(
                "branch {branch_id} has base without base sequence"
            )));
        };
        branch_id = base_branch_id;
        upper_seq = base_seq;
    }
    Ok(segments)
}

fn newest_compaction(
    connection: &Connection,
    session_id: &str,
    segments: &[BranchSegment],
) -> Result<Option<(String, i64)>, SessionError> {
    for segment in segments {
        let result = connection.query_row(
            "SELECT MAX(entry_seq) FROM branch_entries WHERE session_id = ?1 AND branch_id = ?2 AND entry_seq > ?3 AND entry_seq <= ?4 AND entry_type = 'compaction'",
            params![session_id, segment.branch_id, segment.lower_seq, segment.upper_seq],
            |row| row.get::<_, Option<i64>>(0),
        );
        let value = result
            .map_err(|error| db_failure("failed to read SQLite compaction boundary", error))?;
        if let Some(seq) = value {
            return Ok(Some((segment.branch_id.clone(), seq)));
        }
    }
    Ok(None)
}

fn copy_branch_entries_after(
    tx: &Transaction<'_>,
    session_id: &str,
    target_branch_id: &str,
    segments: &[BranchSegment],
    after_seq: i64,
) -> Result<(), SessionError> {
    for segment in segments.iter().rev() {
        let lower_seq = segment.lower_seq.max(after_seq);
        if segment.upper_seq <= lower_seq {
            continue;
        }
        tx.execute(
            "INSERT INTO branch_entries (session_id, branch_id, entry_id, entry_seq, entry_type) SELECT ?1, ?2, entry_id, entry_seq, entry_type FROM branch_entries WHERE session_id = ?1 AND branch_id = ?3 AND entry_seq > ?4 AND entry_seq <= ?5",
            params![session_id, target_branch_id, segment.branch_id, lower_seq, segment.upper_seq],
        )
        .map_err(|error| db_failure("failed to copy SQLite branch index rows", error))?;
    }
    Ok(())
}

fn create_divergent_branch(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    let parent = entry
        .parent_id()
        .ok_or_else(|| invariant("root entry cannot diverge a branch"))?;
    let segments = read_branch_segments(tx, session_id, parent.as_str())?;
    let compaction = newest_compaction(tx, session_id, &segments)?;
    let (base_branch, base_seq) = compaction.as_ref().map_or((None, None), |(branch, seq)| {
        (Some(branch.as_str()), Some(*seq))
    });
    tx.execute(
        "INSERT INTO branch_meta (session_id, branch_id, tip_entry_id, tip_seq, base_branch_id, base_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![session_id, entry.id().as_str(), entry.id().as_str(), sqlite_seq(entry.seq())?, base_branch, base_seq],
    )
    .map_err(|error| db_failure("failed to create SQLite divergent branch", error))?;
    copy_branch_entries_after(
        tx,
        session_id,
        entry.id().as_str(),
        &segments,
        base_seq.unwrap_or(0),
    )?;
    insert_branch_entry(tx, session_id, entry.id().as_str(), entry)
}

fn append_entry_to_branch_index(
    tx: &Transaction<'_>,
    session_id: &str,
    entry: &Entry,
) -> Result<(), SessionError> {
    if entry.parent_id().is_none() {
        return create_root_branch(tx, session_id, entry);
    }
    let parent = entry
        .parent_id()
        .ok_or_else(|| invariant("missing entry parent"))?;
    match read_branch_tip_for_parent(tx, session_id, parent.as_str())? {
        Some(branch) => append_entry_to_existing_branch(tx, session_id, &branch, entry),
        None => create_divergent_branch(tx, session_id, entry),
    }
}

fn scan_branch_rows(
    connection: &Connection,
    session_id: &str,
    query: &StorageBranchScan,
) -> Result<Vec<EntryRow>, SessionError> {
    let limit = effective_scan_limit(query.limit);
    if limit == 0 {
        return Ok(Vec::new());
    }
    if read_entry_row(connection, session_id, &query.start)?.is_none() {
        return Err(SessionError::UnknownTarget(query.start.clone()));
    }
    let oldest_first = query.order.unwrap_or(ScanOrder::Desc) == ScanOrder::Asc;
    let segments_newest_first = read_branch_segments(connection, session_id, query.start.as_str())?;
    let segments = if oldest_first {
        segments_newest_first.into_iter().rev().collect::<Vec<_>>()
    } else {
        segments_newest_first
    };
    let limit = usize::try_from(limit).map_err(|_| invariant("branch scan limit overflow"))?;
    let mut rows = Vec::new();
    for segment in &segments {
        let remaining = limit.saturating_sub(rows.len());
        if remaining == 0 {
            break;
        }
        let remaining =
            u32::try_from(remaining).map_err(|_| invariant("branch scan limit overflow"))?;
        let stop_seq = read_stop_seq(connection, session_id, segment, query, oldest_first)?;
        rows.extend(scan_branch_segment_rows(
            connection,
            session_id,
            segment,
            query,
            oldest_first,
            stop_seq,
            remaining,
        )?);
        if stop_seq.is_some() {
            break;
        }
    }
    Ok(rows)
}

fn entry_type_filter_name(value: EntryType) -> &'static str {
    match value {
        EntryType::Message => "message",
        EntryType::Compaction => "compaction",
        EntryType::BranchSummary => "branch_summary",
        EntryType::Custom => "custom",
    }
}

fn read_stop_seq(
    connection: &Connection,
    session_id: &str,
    segment: &BranchSegment,
    query: &StorageBranchScan,
    oldest_first: bool,
) -> Result<Option<i64>, SessionError> {
    if query.stop_at_type.is_none() && query.stop_at_id.is_none() {
        return Ok(None);
    }
    let stop_predicate = "(:stop_type IS NOT NULL AND b.entry_type = :stop_type) OR (:stop_id IS NOT NULL AND b.entry_id = :stop_id)";
    let sql = format!(
        "SELECT MIN(b.entry_seq), MAX(b.entry_seq) FROM branch_entries b WHERE b.session_id = :session AND b.branch_id = :branch AND b.entry_seq > :lower_seq AND b.entry_seq <= :upper_seq AND ({stop_predicate})"
    );
    let stop_type = query.stop_at_type.map(entry_type_filter_name);
    let stop_id = query.stop_at_id.as_ref().map(EntryId::as_str);
    let (minimum, maximum) = connection
        .query_row(
            &sql,
            named_params! {
                ":session": session_id,
                ":branch": segment.branch_id.as_str(),
                ":lower_seq": segment.lower_seq,
                ":upper_seq": segment.upper_seq,
                ":stop_type": stop_type,
                ":stop_id": stop_id,
            },
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|error| db_failure("failed to read SQLite branch stop boundary", error))?;
    Ok(if oldest_first { minimum } else { maximum })
}

fn scan_branch_segment_rows(
    connection: &Connection,
    session_id: &str,
    segment: &BranchSegment,
    query: &StorageBranchScan,
    oldest_first: bool,
    stop_seq: Option<i64>,
    limit: u32,
) -> Result<Vec<EntryRow>, SessionError> {
    let order = if oldest_first { "ASC" } else { "DESC" };
    let stop_comparator = if oldest_first { "<=" } else { ">=" };
    let cursor_comparator = if oldest_first { ">" } else { "<" };
    let entry_type = query.entry_type.map(entry_type_filter_name);
    let cursor = query
        .cursor
        .map(|cursor| sqlite_seq(cursor.seq))
        .transpose()?;
    let sql = format!(
        "SELECT e.id, e.parent_id, e.seq, e.type, e.custom_type, e.timestamp, e.payload FROM branch_entries b JOIN entries e ON e.session_id = b.session_id AND e.id = b.entry_id WHERE b.session_id = :session AND b.branch_id = :branch AND b.entry_seq > :lower_seq AND b.entry_seq <= :upper_seq AND (:stop_seq IS NULL OR b.entry_seq {stop_comparator} :stop_seq) AND (:entry_type IS NULL OR b.entry_type = :entry_type) AND (:custom_type IS NULL OR e.custom_type = :custom_type) AND (:cursor IS NULL OR b.entry_seq {cursor_comparator} :cursor) ORDER BY b.entry_seq {order} LIMIT :limit"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| db_failure("failed to prepare SQLite branch scan", error))?;
    let rows = statement
        .query_map(
            named_params! {
                ":session": session_id,
                ":branch": segment.branch_id.as_str(),
                ":lower_seq": segment.lower_seq,
                ":upper_seq": segment.upper_seq,
                ":stop_seq": stop_seq,
                ":entry_type": entry_type,
                ":custom_type": query.custom_type.as_deref(),
                ":cursor": cursor,
                ":limit": i64::from(limit),
            },
            read_entry_row_from_sql,
        )
        .map_err(|error| db_failure("failed to scan SQLite branch entries", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|error| db_failure("failed to decode SQLite branch row", error))?);
    }
    Ok(output)
}

fn insert_session_row(
    tx: &Transaction<'_>,
    metadata: &SessionMetadata,
    next_seq: u64,
) -> Result<(), SessionError> {
    let usage = serde_json::to_string(&pi_ai::Usage::default()).map_err(|error| {
        json_failure(
            StorageErrorCode::Corrupt,
            "failed to encode empty usage totals",
            error,
        )
    })?;
    tx.execute(
        "INSERT INTO sessions (id, created_at, parent_session_id, storage_version, metadata, message_count, usage_payload, next_seq) VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5, ?6)",
        params![
            &metadata.id,
            metadata.created_at,
            metadata.parent_session_id,
            i64::from(schema::SQLITE_STORAGE_VERSION),
            usage,
            sqlite_seq(next_seq)?,
        ],
    )
    .map_err(|error| db_failure("failed to insert SQLite session row", error))?;
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    pub(crate) created_at: i64,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) storage_version: i64,
    pub(crate) next_seq: i64,
}

fn session_row_from_sql(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        created_at: row.get(1)?,
        parent_session_id: row.get(2)?,
        storage_version: row.get(3)?,
        next_seq: row.get(4)?,
    })
}

pub(crate) fn read_session_row(
    connection: &Connection,
    session_id: &str,
) -> Result<SessionRow, SessionError> {
    let result = connection.query_row(
        "SELECT id, created_at, parent_session_id, storage_version, next_seq FROM sessions WHERE id = ?1",
        params![session_id],
        session_row_from_sql,
    );
    match result {
        Ok(row) => Ok(row),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Err(not_found(format!("Unknown SQLite session: {session_id}")))
        }
        Err(error) => Err(db_failure("failed to read SQLite session row", error)),
    }
}

pub(crate) fn read_all_session_rows(
    connection: &Connection,
) -> Result<Vec<SessionRow>, SessionError> {
    let mut statement = connection
        .prepare(
            "SELECT id, created_at, parent_session_id, storage_version, next_seq FROM sessions",
        )
        .map_err(|error| db_failure("failed to prepare SQLite session listing", error))?;
    let rows = statement
        .query_map([], session_row_from_sql)
        .map_err(|error| db_failure("failed to list SQLite session rows", error))?;
    let mut output = Vec::new();
    for row in rows {
        output.push(row.map_err(|error| db_failure("failed to decode SQLite session row", error))?);
    }
    Ok(output)
}

pub(crate) fn has_session_row(
    connection: &Connection,
    session_id: &str,
) -> Result<bool, SessionError> {
    let result = connection.query_row(
        "SELECT id FROM sessions WHERE id = ?1",
        params![session_id],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(error) => Err(db_failure("failed to check SQLite session row", error)),
    }
}

pub(crate) fn metadata_from_session_row(
    path: &Path,
    row: &SessionRow,
) -> Result<SqliteSessionMetadata, SessionError> {
    let current = i64::from(schema::SQLITE_STORAGE_VERSION);
    if row.storage_version > current {
        return Err(SessionError::Backend(StorageFailure::new(
            StorageErrorCode::VersionMismatch,
            format!(
                "SQLite session storage version {} is newer than {}",
                row.storage_version,
                schema::SQLITE_STORAGE_VERSION
            ),
        )));
    }
    if row.storage_version < current {
        return Err(SessionError::Backend(StorageFailure::new(
            StorageErrorCode::VersionMismatch,
            format!(
                "SQLite session storage version {} requires migrations",
                row.storage_version
            ),
        )));
    }
    let next_seq = from_sqlite_seq(row.next_seq)?;
    if next_seq == 0 {
        return Err(invariant("stored SQLite next sequence must be positive"));
    }
    Ok(SqliteSessionMetadata {
        session: SessionMetadata {
            id: row.id.clone(),
            created_at: row.created_at,
            storage_version: schema::SQLITE_STORAGE_VERSION,
            cwd: None,
            parent_session_id: row.parent_session_id.clone(),
            legacy_parent_session_path: None,
        },
        path: path.to_path_buf(),
    })
}

pub(crate) fn delete_session_rows(
    tx: &Transaction<'_>,
    session_id: &str,
) -> Result<(), SessionError> {
    for (table, label) in [
        ("entries", "entries"),
        ("scalar_values", "scalar values"),
        ("list_values", "list values"),
        ("usage_ledger", "usage ledger"),
        ("branch_entries", "branch entries"),
        ("branch_meta", "branch metadata"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE session_id = ?1");
        tx.execute(&statement, params![session_id])
            .map_err(|error| db_failure(&format!("failed to delete SQLite {label}"), error))?;
    }
    let changed = tx
        .execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
        .map_err(|error| db_failure("failed to delete SQLite session row", error))?;
    if changed != 1 {
        return Err(invariant(format!(
            "Expected to delete one SQLite session {session_id}, deleted {changed}"
        )));
    }
    Ok(())
}

pub(crate) fn create_session(
    path: &Path,
    metadata: &SqliteSessionMetadata,
) -> Result<Arc<SqliteStorage>, SessionError> {
    let mut connection = schema::open_create(path)
        .map_err(|error| db_failure("failed to create SQLite session file", error))?;
    schema::configure_writable(&connection)
        .map_err(|error| db_failure("failed to configure SQLite connection", error))?;
    schema::initialize(&connection)
        .map_err(|error| db_failure("failed to initialize SQLite schema", error))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| db_failure("failed to begin SQLite session creation", error))?;
    if has_session_row(&transaction, &metadata.session.id)? {
        return Err(invariant(format!(
            "SQLite session already exists: {}",
            metadata.session.id
        )));
    }
    insert_session_row(&transaction, &metadata.session, schema::FIRST_COMMIT_SEQ)?;
    transaction
        .commit()
        .map_err(|error| db_failure("failed to commit SQLite session creation", error))?;
    Ok(SqliteStorage::from_connection(
        connection,
        metadata.session.id.clone(),
    ))
}

pub(crate) fn open_session(
    path: &Path,
    session_id: &str,
) -> Result<(SqliteSessionMetadata, Arc<SqliteStorage>), SessionError> {
    let connection = schema::open_existing(path)
        .map_err(|error| db_failure("failed to open SQLite session file", error))?;
    schema::configure_writable(&connection)
        .map_err(|error| db_failure("failed to configure SQLite connection", error))?;
    let row = read_session_row(&connection, session_id)?;
    let metadata = metadata_from_session_row(path, &row)?;
    let storage = SqliteStorage::from_connection(connection, session_id.to_owned());
    Ok((metadata, storage))
}

fn has_sessions_table(connection: &Connection) -> Result<bool, SessionError> {
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| db_failure("failed to inspect SQLite schema", error))?;
    Ok(count != 0)
}

pub(crate) fn list_container(path: &Path) -> Result<Vec<SqliteSessionMetadata>, SessionError> {
    let connection = schema::open_read_only(path)
        .map_err(|error| db_failure("failed to open SQLite read-only connection", error))?;
    schema::configure_read_only(&connection)
        .map_err(|error| db_failure("failed to configure SQLite read-only connection", error))?;
    if !has_sessions_table(&connection)? {
        return Ok(Vec::new());
    }
    read_all_session_rows(&connection)?
        .into_iter()
        .map(|row| metadata_from_session_row(path, &row))
        .collect()
}

pub(crate) fn delete_session_row(path: &Path, session_id: &str) -> Result<(), SessionError> {
    let mut connection = schema::open_existing(path)
        .map_err(|error| db_failure("failed to open SQLite session file", error))?;
    schema::configure_writable(&connection)
        .map_err(|error| db_failure("failed to configure SQLite connection", error))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| db_failure("failed to begin SQLite session deletion", error))?;
    let row = read_session_row(&transaction, session_id)?;
    metadata_from_session_row(path, &row)?;
    delete_session_rows(&transaction, session_id)?;
    transaction
        .commit()
        .map_err(|error| db_failure("failed to commit SQLite session deletion", error))?;
    Ok(())
}

pub(crate) fn verify_session_row(path: &Path, session_id: &str) -> Result<(), SessionError> {
    let connection = schema::open_existing(path)
        .map_err(|error| db_failure("failed to open SQLite session file", error))?;
    schema::configure_writable(&connection)
        .map_err(|error| db_failure("failed to configure SQLite connection", error))?;
    let row = read_session_row(&connection, session_id)?;
    metadata_from_session_row(path, &row)?;
    Ok(())
}

pub(crate) fn read_fork_source(
    path: &Path,
    session_id: &str,
) -> Result<ForkSourceSnapshot, SessionError> {
    let mut connection = schema::open_read_only(path)
        .map_err(|error| db_failure("failed to open SQLite fork source", error))?;
    schema::configure_read_only(&connection)
        .map_err(|error| db_failure("failed to configure SQLite read-only connection", error))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|error| db_failure("failed to begin SQLite fork snapshot", error))?;
    let row = read_session_row(&transaction, session_id)?;
    metadata_from_session_row(path, &row)?;
    let entries = read_all_entry_rows(&transaction, session_id)?
        .into_iter()
        .map(decode_entry_row)
        .collect::<Result<Vec<_>, _>>()?;
    let values = read_all_scalar_values(&transaction, session_id)?;
    transaction
        .commit()
        .map_err(|error| db_failure("failed to finish SQLite fork snapshot", error))?;
    Ok(ForkSourceSnapshot {
        entries,
        values,
        entries_complete: true,
    })
}

pub(crate) fn create_fork_session(
    path: &Path,
    metadata: &SqliteSessionMetadata,
    snapshot: &ForkDestinationSnapshot,
) -> Result<Arc<SqliteStorage>, SessionError> {
    if snapshot.next_seq == 0 {
        return Err(invariant("SQLite fork next sequence must be positive"));
    }
    let mut connection = schema::open_create(path)
        .map_err(|error| db_failure("failed to create SQLite fork file", error))?;
    schema::configure_writable(&connection)
        .map_err(|error| db_failure("failed to configure SQLite fork connection", error))?;
    schema::initialize(&connection)
        .map_err(|error| db_failure("failed to initialize SQLite fork schema", error))?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| db_failure("failed to begin SQLite fork creation", error))?;
    if has_session_row(&transaction, &metadata.session.id)? {
        return Err(invariant(format!(
            "SQLite session already exists: {}",
            metadata.session.id
        )));
    }
    insert_session_row(&transaction, &metadata.session, snapshot.next_seq)?;
    let writes = fork_snapshot_writes(snapshot);
    let view = SqliteIdView {
        ids: HashSet::new(),
        entry_ids: HashSet::new(),
        next_seq: schema::FIRST_COMMIT_SEQ,
    };
    validate_replayed_writes(&writes, &view)?;
    let mut stats = SessionStats::default();
    apply_committed_rows(&transaction, &metadata.session.id, &writes, &mut stats)?;
    update_session_totals(
        &transaction,
        &metadata.session.id,
        snapshot.next_seq,
        &stats,
    )?;
    transaction
        .commit()
        .map_err(|error| db_failure("failed to commit SQLite fork creation", error))?;
    Ok(SqliteStorage::from_connection(
        connection,
        metadata.session.id.clone(),
    ))
}
