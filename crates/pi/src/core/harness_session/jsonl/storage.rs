use futures::future::BoxFuture;
use pi_agent::context::Context;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Notify;

use pi_agent::session::{
    CommitResult, CommittedWrite, Entry, EntryId, EntryScan, EntryStructure,
    ForkDestinationSnapshot, ForkSource, ForkSourceSnapshot, IdGenerator, InMemoryStorageState,
    RawListElement, RawStoredValue, ScanOrder, SessionError, SessionStats, Storage,
    StorageBranchScan, StorageErrorCode, StorageFailure, UsageRow, UsageScan, Write,
    address::{
        LIST_READ_DEFAULT_LIMIT, LIST_READ_MAX_LIMIT, RawAddress, resolve_list_read_options,
    },
    commit_writes, validate_committed_writes,
};
use pi_ai::Usage;

use super::codec::{
    JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION, JsonlStorageHeader, LegacyV3Header, ParsedHeader,
    parse_header, parse_transaction, publish_file_atomically, serialize_transaction,
    split_complete_lines,
};
use super::legacy_v3::{normalize_legacy_v3_header, normalize_legacy_v3_records};

/// Durable JSONL storage handle. The in-memory index is the canonical query
/// surface; the file is only touched by open, create, and committed appends.
pub struct JsonlStorage {
    inner: Arc<Mutex<Inner>>,
    closed: AtomicBool,
    admission: Arc<Mutex<()>>,
    active: Arc<AtomicUsize>,
    drained: Arc<Notify>,
    path: PathBuf,
}

struct Inner {
    state: InMemoryStorageState,
    backing: Backing,
    header: JsonlStorageHeader,
}

enum Backing {
    V4,
    V3 {
        imported_usage: Usage,
        baseline: Vec<CommittedWrite>,
    },
}
struct ActivePermit {
    active: Arc<AtomicUsize>,
    drained: Arc<Notify>,
}

impl Drop for ActivePermit {
    fn drop(&mut self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.notify_one();
            self.drained.notify_waiters();
        }
    }
}

fn failure(
    code: StorageErrorCode,
    message: impl Into<String>,
    source: Option<Arc<dyn std::error::Error + Send + Sync>>,
) -> SessionError {
    SessionError::Backend(StorageFailure {
        code,
        message: message.into(),
        source,
    })
}

fn aborted(error: pi_agent::context::Cancelled) -> SessionError {
    failure(
        StorageErrorCode::Aborted,
        "operation cancelled",
        Some(Arc::new(error)),
    )
}

fn closed() -> SessionError {
    failure(StorageErrorCode::Closed, "session storage is closed", None)
}

fn io_failure(
    path: &Path,
    action: &str,
    error: impl std::error::Error + Send + Sync + 'static,
) -> SessionError {
    failure(
        StorageErrorCode::Io,
        format!("{action} {}", path.display()),
        Some(Arc::new(error)),
    )
}

fn lock_inner<T>(inner: &Mutex<T>) -> MutexGuard<'_, T> {
    inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn now_millis() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn add_usage(total: &mut Usage, add: &Usage) {
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

fn serialize_header(header: &JsonlStorageHeader) -> Result<String, SessionError> {
    serde_json::to_string(header).map_err(|error| {
        failure(
            StorageErrorCode::Corrupt,
            "failed to serialize JSONL storage header",
            Some(Arc::new(error)),
        )
    })
}

fn complete_file(
    header: &JsonlStorageHeader,
    transactions: impl IntoIterator<Item = String>,
) -> Result<String, SessionError> {
    let mut content = serialize_header(header)?;
    for transaction in transactions {
        content.push('\n');
        content.push_str(&transaction);
    }
    content.push('\n');
    Ok(content)
}

impl JsonlStorage {
    fn new_handle(inner: Inner, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Mutex::new(inner)),
            closed: AtomicBool::new(false),
            admission: Arc::new(Mutex::new(())),
            active: Arc::new(AtomicUsize::new(0)),
            drained: Arc::new(Notify::new()),
            path,
        })
    }
    /// Returns the normalized header currently associated with this handle.
    #[must_use]
    pub fn header(&self) -> JsonlStorageHeader {
        lock_inner(&self.inner).header.clone()
    }

    /// Creates and publishes an empty v4 file.
    ///
    /// # Errors
    ///
    /// Returns an error if the header kind or format version is invalid, header
    /// serialization fails, or the file cannot be published.
    pub fn create_sync(path: &Path, header: JsonlStorageHeader) -> Result<Arc<Self>, SessionError> {
        if header.v != JSONL_FORMAT_VERSION || header.kind != "header" {
            return Err(failure(
                StorageErrorCode::InvalidHeader,
                "invalid JSONL storage header",
                None,
            ));
        }
        let content = complete_file(&header, std::iter::empty())?;
        publish_file_atomically(path, &content)?;
        Ok(Self::new_handle(
            Inner {
                state: InMemoryStorageState::new(),
                backing: Backing::V4,
                header,
            },
            path.to_owned(),
        ))
    }

    /// Opens a v4 or legacy v3 file, repairing only a torn final record.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, its header or records are
    /// invalid, its storage version is unsupported, replay or legacy normalization
    /// fails, or a torn final record cannot be repaired.
    pub fn open_sync(path: &Path) -> Result<(JsonlStorageHeader, Arc<Self>), SessionError> {
        let content = fs::read_to_string(path)
            .map_err(|error| io_failure(path, "failed to read JSONL storage", error))?;
        let (lines, torn) = split_complete_lines(&content);
        if lines.first().is_none_or(|line| line.is_empty()) {
            return Err(failure(
                StorageErrorCode::InvalidHeader,
                format!("invalid JSONL storage {}: missing header", path.display()),
                None,
            ));
        }
        let parsed = parse_header(lines[0]).map_err(|error| {
            failure(
                StorageErrorCode::InvalidHeader,
                format!("invalid JSONL storage {}: invalid header", path.display()),
                Some(Arc::new(error)),
            )
        })?;
        match parsed {
            ParsedHeader::V4(header) => Self::open_v4(path, header, &lines, torn),
            ParsedHeader::LegacyV3(header) => Self::open_v3(path, &header, &lines[1..]),
        }
    }

    fn open_v4(
        path: &Path,
        header: JsonlStorageHeader,
        lines: &[&str],
        torn: bool,
    ) -> Result<(JsonlStorageHeader, Arc<Self>), SessionError> {
        if header.storage_version != JSONL_STORAGE_VERSION {
            return Err(failure(
                StorageErrorCode::VersionMismatch,
                format!(
                    "session {} uses unsupported storage version {}",
                    header.id, header.storage_version
                ),
                None,
            ));
        }
        let mut state = InMemoryStorageState::new();
        for (index, line) in lines.iter().enumerate().skip(1) {
            let writes = parse_transaction(line).map_err(|error| {
                failure(
                    StorageErrorCode::Corrupt,
                    format!(
                        "invalid JSONL storage {}: line {}",
                        path.display(),
                        index + 1
                    ),
                    Some(Arc::new(error)),
                )
            })?;
            state.replay(&writes).map_err(|error| {
                failure(
                    StorageErrorCode::Corrupt,
                    format!(
                        "invalid JSONL storage {}: line {}",
                        path.display(),
                        index + 1
                    ),
                    Some(Arc::new(error)),
                )
            })?;
        }
        if let Some(next_seq) = header.next_seq {
            state.advance_next_seq(next_seq);
        }
        if torn {
            let repaired = format!("{}\n", lines.join("\n"));
            publish_file_atomically(path, &repaired)?;
        }
        let storage = Self::new_handle(
            Inner {
                state,
                backing: Backing::V4,
                header: header.clone(),
            },
            path.to_owned(),
        );
        Ok((header, storage))
    }

    fn open_v3(
        path: &Path,
        header: &LegacyV3Header,
        record_lines: &[&str],
    ) -> Result<(JsonlStorageHeader, Arc<Self>), SessionError> {
        let normalized = normalize_legacy_v3_records(record_lines)?;
        let mut target_header = normalize_legacy_v3_header(path, header)?;
        target_header.next_seq = Some(normalized.next_seq);
        let mut state = InMemoryStorageState::new();
        state.replay(&normalized.writes).map_err(|error| {
            failure(
                StorageErrorCode::Corrupt,
                format!("invalid legacy JSONL storage {}", path.display()),
                Some(Arc::new(error)),
            )
        })?;
        let storage = Self::new_handle(
            Inner {
                state,
                backing: Backing::V3 {
                    imported_usage: normalized.imported_usage,
                    baseline: normalized.writes,
                },
                header: target_header.clone(),
            },
            path.to_owned(),
        );
        Ok((target_header, storage))
    }

    /// Creates and publishes a v4 file from an already captured fork snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if header serialization or file publication fails, or if
    /// opening the published snapshot rejects its header, version, or records.
    pub fn create_from_fork_sync(
        path: &Path,
        mut header: JsonlStorageHeader,
        snapshot: &ForkDestinationSnapshot,
    ) -> Result<Arc<Self>, SessionError> {
        header.next_seq = Some(snapshot.next_seq);
        let writes = pi_agent::session::fork_snapshot_writes(snapshot);
        let transactions = writes
            .iter()
            .map(|write| serialize_transaction(std::slice::from_ref(write)));
        let content = complete_file(&header, transactions)?;
        publish_file_atomically(path, &content)?;
        Self::open_sync(path).map(|(_, storage)| storage)
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(closed())
        } else {
            Ok(())
        }
    }
    fn admit(&self) -> Result<ActivePermit, SessionError> {
        let _gate = lock_inner(&self.admission);
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        Ok(ActivePermit {
            active: Arc::clone(&self.active),
            drained: Arc::clone(&self.drained),
        })
    }

    fn commit_sync(
        inner_handle: &Arc<Mutex<Inner>>,
        path: &Path,
        writes: Vec<Write>,
    ) -> Result<CommitResult, SessionError> {
        let mut inner = lock_inner(inner_handle);
        match &inner.backing {
            Backing::V3 { .. } if writes.is_empty() => {
                return Ok(CommitResult {
                    first_seq: inner.state.next_seq,
                    seqs: Vec::new(),
                    timestamp: now_millis(),
                    stats: stats_with_imported_usage(&inner.state.stats, &inner.backing),
                });
            }
            Backing::V3 { .. } => return Self::upgrade_v3_locked(&mut inner, path, writes),
            Backing::V4 => {}
        }

        let timestamp = now_millis();
        let first_seq = inner.state.next_seq;
        validate_committed_writes(&writes, first_seq, &inner.state)?;
        let (committed, seqs) = commit_writes(writes, first_seq, timestamp)?;
        if !committed.is_empty() {
            let line = serialize_transaction(&committed);
            let append_result = (|| -> std::io::Result<()> {
                let mut file = OpenOptions::new().append(true).open(path)?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                file.flush()
            })();
            if let Err(error) = append_result {
                return Err(io_failure(path, "failed to append JSONL storage", error));
            }
        }
        let stats = inner.state.apply_committed(&committed);
        Ok(CommitResult {
            first_seq,
            seqs,
            timestamp,
            stats,
        })
    }

    fn upgrade_v3_locked(
        inner: &mut Inner,
        path: &Path,
        caller_writes: Vec<Write>,
    ) -> Result<CommitResult, SessionError> {
        let (imported_usage, baseline) = match &inner.backing {
            Backing::V3 {
                imported_usage,
                baseline,
            } => (imported_usage.clone(), baseline.clone()),
            Backing::V4 => unreachable!("v4 storage cannot enter legacy upgrade"),
        };
        let timestamp = now_millis();
        let usage_id = pi_agent::session::UuidV7Generator::new().next(Some(timestamp))?;
        let mut writes = Vec::with_capacity(caller_writes.len().saturating_add(1));
        writes.push(Write::Usage {
            row: pi_agent::session::NewUsageRow {
                id: pi_agent::session::UsageId::from(usage_id),
                usage: imported_usage,
                entry_id: None,
                adjustment: true,
                details: Some(serde_json::json!({"source": "v3-import"})),
            },
        });
        writes.extend(caller_writes);
        let first_seq = inner.state.next_seq;
        validate_committed_writes(&writes, first_seq, &inner.state)?;
        let (committed, seqs) = commit_writes(writes, first_seq, timestamp)?;
        let next_seq = first_seq
            .checked_add(
                u64::try_from(committed.len())
                    .map_err(|_| SessionError::Invariant("sequence overflow".to_owned()))?,
            )
            .ok_or_else(|| SessionError::Invariant("sequence overflow".to_owned()))?;
        let mut header = inner.header.clone();
        header.next_seq = Some(next_seq);
        let transactions = baseline
            .iter()
            .map(|write| serialize_transaction(std::slice::from_ref(write)))
            .chain(std::iter::once(serialize_transaction(&committed)));
        let content = complete_file(&header, transactions)?;
        publish_file_atomically(path, &content)?;
        let stats = inner.state.apply_committed(&committed);
        inner.header = header;
        inner.backing = Backing::V4;
        Ok(CommitResult {
            first_seq: first_seq.saturating_add(1),
            seqs: seqs.into_iter().skip(1).collect(),
            timestamp,
            stats,
        })
    }

    fn with_state<T>(&self, f: impl FnOnce(&InMemoryStorageState) -> T) -> T {
        let inner = lock_inner(&self.inner);
        f(&inner.state)
    }
}

fn stats_with_imported_usage(stats: &SessionStats, backing: &Backing) -> SessionStats {
    let mut output = stats.clone();
    if let Backing::V3 { imported_usage, .. } = backing {
        add_usage(&mut output.usage, imported_usage);
    }
    output
}

impl Storage for JsonlStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            let permit = self.admit()?;
            let path = self.path.clone();
            let inner = Arc::clone(&self.inner);
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                JsonlStorage::commit_sync(&inner, &path, writes)
            })
            .await
            .map_err(|error| {
                failure(
                    StorageErrorCode::Io,
                    "JSONL commit task failed",
                    Some(Arc::new(error)),
                )
            })?
        })
    }

    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            Ok(self.with_state(|state| {
                ids.iter()
                    .filter_map(|id| {
                        state
                            .entries
                            .get(id)
                            .cloned()
                            .map(|entry| (id.clone(), entry))
                    })
                    .collect()
            }))
        })
    }

    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            Ok(self.with_state(|state| {
                state
                    .values
                    .get(&(address.namespace.clone(), address.key.clone()))
                    .cloned()
            }))
        })
    }

    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            Ok(self.with_state(|state| {
                state
                    .values
                    .values()
                    .filter(|value| {
                        value.namespace == prefix.namespace && value.key.starts_with(&prefix.key)
                    })
                    .cloned()
                    .collect()
            }))
        })
    }

    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<pi_agent::session::ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            let options = resolve_list_read_options(options)
                .map_err(|_| SessionError::Invariant("list limit must be positive".to_owned()))?;
            Ok(self.with_state(|state| {
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
                values
            }))
        })
    }

    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            self.with_state(|state| state.scan_branch(query))
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
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            Ok(self.with_state(|state| {
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
                entries
            }))
        })
    }

    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            Ok(self.with_state(|state| {
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
                rows
            }))
        })
    }

    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            self.ensure_open()?;
            let inner = lock_inner(&self.inner);
            Ok(stats_with_imported_usage(
                &inner.state.stats,
                &inner.backing,
            ))
        })
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            {
                let _gate = lock_inner(&self.admission);
                self.closed.store(true, Ordering::Release);
            }
            let active = Arc::clone(&self.active);
            let drained = Arc::clone(&self.drained);
            tokio::spawn(async move {
                loop {
                    if active.load(Ordering::Acquire) == 0 {
                        return Ok::<(), SessionError>(());
                    }
                    drained.notified().await;
                }
            })
            .await
            .map_err(|error| {
                failure(
                    StorageErrorCode::Io,
                    "JSONL close task failed",
                    Some(Arc::new(error)),
                )
            })?
        })
    }
}
impl ForkSource for JsonlStorage {
    fn capture_fork_source<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<ForkSourceSnapshot, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(aborted)?;
            let permit = self.admit()?;
            let snapshot = self.with_state(InMemoryStorageState::snapshot_for_fork);
            drop(permit);
            Ok(snapshot)
        })
    }
}
