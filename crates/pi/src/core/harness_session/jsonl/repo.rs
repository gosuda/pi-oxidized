use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::session::{
    ForkDestinationSnapshot, ForkOptions, ForkSource, ForkSourceSnapshot, IdGenerator, Session,
    SessionError, SessionMetadata, SessionMetadataLike, SessionRepo, Storage, StorageBackedSession,
    StorageErrorCode, StorageFailure, UuidV7Generator, create_fork_snapshot,
};

use super::codec::{self, JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION, JsonlStorageHeader};
use super::legacy_v3;
use super::paths::{
    discard_session_file, io_failure, list_session_files, remove_session_file,
    resolve_new_session_path, session_directory_name, verify_owned_session_path,
};
use super::storage::JsonlStorage;

/// Metadata returned by the JSONL session repository.
///
/// `cwd` is required: it is read from the wire header and rechecked on open
/// because the directory encoding is lossy (`/a/b` and `/a-b` both map to
/// `--a-b--`).
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct JsonlSessionMetadata {
    /// Unique session id, normally a `UUIDv7`.
    pub id: String,
    /// Creation time, milliseconds since the Unix epoch.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    /// Storage layout version this session was written with.
    #[serde(rename = "storageVersion")]
    pub storage_version: u32,
    /// Resolved working directory the session was started in.
    pub cwd: String,
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
    /// Physical session file path.
    pub path: PathBuf,
    /// Filesystem modification time in milliseconds since the Unix epoch.
    /// Fractional like Node's `stats.mtimeMs`; negative before the epoch.
    #[serde(rename = "modifiedAt")]
    pub modified_at: f64,
}

impl SessionMetadataLike for JsonlSessionMetadata {}

impl JsonlSessionMetadata {
    /// Backend-neutral projection handed to [`StorageBackedSession`].
    fn session_metadata(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.id.clone(),
            created_at: self.created_at,
            storage_version: self.storage_version,
            cwd: Some(self.cwd.clone()),
            parent_session_id: self.parent_session_id.clone(),
            legacy_parent_session_path: self.legacy_parent_session_path.clone(),
        }
    }
}

/// Creation options for JSONL sessions.
#[derive(Clone, Debug, Default)]
pub struct JsonlSessionCreateOptions {
    /// Working directory to resolve and persist in the header.
    pub cwd: String,
    /// Explicit session identity, or `None` to generate a `UUIDv7`.
    pub id: Option<String>,
    /// Parent session identity for a forked session.
    pub parent_session_id: Option<String>,
}

/// Listing options for JSONL sessions.
#[derive(Clone, Debug, Default)]
pub struct JsonlSessionListOptions {
    /// Optional working directory filter.
    pub cwd: Option<String>,
}

struct OpenStorage {
    storage: Arc<JsonlStorage>,
    dyn_storage: Arc<dyn Storage>,
}

struct IdReservation {
    pending: Arc<StdMutex<HashSet<String>>>,
    key: String,
}

impl Drop for IdReservation {
    fn drop(&mut self) {
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.remove(&self.key);
    }
}

/// JSONL session repository: one file per session below an encoded `cwd`
/// directory.
///
/// Open handles are exclusive per (`cwd`, `id`) within this repository process;
/// closing the repository seals new admissions but deliberately leaves
/// existing session handles usable.
pub struct JsonlSessionRepo {
    root: PathBuf,
    open: Arc<StdMutex<HashMap<String, OpenStorage>>>,
    pending: Arc<StdMutex<HashSet<String>>>,
    id_generator: Arc<UuidV7Generator>,
    closed: AtomicBool,
}

impl JsonlSessionRepo {
    /// Creates an empty repository rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = crate::core::config::resolve_path(root.to_string_lossy().as_ref());
        Self {
            root,
            open: Arc::new(StdMutex::new(HashMap::new())),
            pending: Arc::new(StdMutex::new(HashSet::new())),
            id_generator: Arc::new(UuidV7Generator::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Seals new repository admissions.
    ///
    /// Existing sessions are intentionally not closed; each remains usable
    /// until its own [`Session::close`] call.
    ///
    /// # Errors
    ///
    /// Returns an error if the context is cancelled or the open-session lock
    /// is poisoned.
    pub fn close(&self, cx: &Context) -> Result<(), SessionError> {
        cx.check().map_err(|_| aborted_error())?;
        let _open = self.open_guard()?;
        self.closed.store(true, Ordering::Release);
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(repo_closed_error())
        } else {
            Ok(())
        }
    }

    fn open_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, OpenStorage>>, SessionError> {
        self.open
            .lock()
            .map_err(|_| SessionError::Invariant("session id registry poisoned".to_owned()))
    }

    fn reserve_id(&self, key: &str, id: &str) -> Result<IdReservation, SessionError> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| SessionError::Invariant("session id registry poisoned".to_owned()))?;
        let open = self.open_guard()?;
        if open.contains_key(key) || pending.contains(key) {
            return Err(SessionError::Invariant(format!(
                "Session already exists: {id}"
            )));
        }
        pending.insert(key.to_owned());
        Ok(IdReservation {
            pending: Arc::clone(&self.pending),
            key: key.to_owned(),
        })
    }

    fn session_key(cwd: &str, id: &str) -> String {
        format!("{cwd}\0{id}")
    }

    fn session_directory(&self, cwd: &str) -> PathBuf {
        self.root.join(session_directory_name(cwd))
    }

    fn publish_open(
        &self,
        metadata: &JsonlSessionMetadata,
        storage: Arc<JsonlStorage>,
        key: &str,
    ) -> Result<Arc<dyn Session>, SessionError> {
        let dyn_storage: Arc<dyn Storage> = storage.clone();
        let mut open = self.open_guard()?;
        self.ensure_open()?;
        if open.contains_key(key) {
            return Err(SessionError::Invariant(format!(
                "Session is already open: {}",
                metadata.id
            )));
        }

        let open_sessions = Arc::clone(&self.open);
        let callback_key = key.to_owned();
        let callback_storage = Arc::clone(&dyn_storage);
        let id_generator: Arc<dyn IdGenerator> = self.id_generator.clone();
        let session = StorageBackedSession::new(
            metadata.session_metadata(),
            Arc::clone(&dyn_storage),
            id_generator,
            Some(Box::new(move || {
                let mut open = match open_sessions.lock() {
                    Ok(open) => open,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if open
                    .get(&callback_key)
                    .is_some_and(|record| Arc::ptr_eq(&record.dyn_storage, &callback_storage))
                {
                    open.remove(&callback_key);
                }
            })),
        );
        open.insert(
            key.to_owned(),
            OpenStorage {
                storage,
                dyn_storage,
            },
        );
        Ok(session as Arc<dyn Session>)
    }

    async fn load_storage(
        &self,
        metadata: &JsonlSessionMetadata,
        cx: &Context,
    ) -> Result<(JsonlStorageHeader, Arc<JsonlStorage>), SessionError> {
        cx.check().map_err(|_| aborted_error())?;
        if !path_exists(&metadata.path).await? {
            return Err(not_found(format!(
                "session file does not exist: {}",
                metadata.path.display()
            )));
        }
        verify_owned_session_path(&self.root, &metadata.cwd, &metadata.id, &metadata.path).await?;
        let (header, storage) = open_storage(&metadata.path, cx).await?;
        let validation = (|| {
            cx.check().map_err(|_| aborted_error())?;
            if header.id != metadata.id || header.cwd != metadata.cwd {
                return Err(SessionError::Invariant(format!(
                    "Session identity does not match header: {}",
                    metadata.id
                )));
            }
            if header.storage_version != JSONL_STORAGE_VERSION {
                return Err(SessionError::Backend(StorageFailure::new(
                    StorageErrorCode::VersionMismatch,
                    format!(
                        "Session {} uses unsupported storage version {}",
                        metadata.id, header.storage_version
                    ),
                )));
            }
            Ok(())
        })();
        if let Err(error) = validation {
            return {
                storage.close(cx).await?;
                Err(error)
            };
        }
        Ok((header, storage))
    }

    async fn capture_source_snapshot(
        &self,
        source: &JsonlSessionMetadata,
        cx: &Context,
    ) -> Result<ForkSourceSnapshot, SessionError> {
        let key = Self::session_key(&source.cwd, &source.id);
        let open_storage = {
            let open = self.open_guard()?;
            open.get(&key).map(|record| Arc::clone(&record.storage))
        };
        if let Some(storage) = open_storage {
            return storage.capture_fork_source(cx).await;
        }

        let (_, storage) = self.load_storage(source, cx).await?;
        let snapshot = storage.capture_fork_source(cx).await;
        let close = storage.close(cx).await;
        close?;
        snapshot
    }
}

impl SessionRepo for JsonlSessionRepo {
    type Metadata = JsonlSessionMetadata;
    type CreateOptions = JsonlSessionCreateOptions;
    type ListOptions = JsonlSessionListOptions;

    fn create<'a>(
        &'a self,
        options: Self::CreateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let created_at = pi_agent::now_millis();
            let id = match options.id {
                Some(id) => id,
                None => self.id_generator.next(Some(created_at))?,
            };
            let cwd = resolve_cwd(&options.cwd).await?;
            let key = Self::session_key(&cwd, &id);
            let _reservation = self.reserve_id(&key, &id)?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;

            let directory = self.session_directory(&cwd);
            let path = resolve_new_session_path(&directory, created_at, &id).await?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let header = JsonlStorageHeader {
                v: JSONL_FORMAT_VERSION,
                kind: "header".to_owned(),
                id: id.clone(),
                storage_version: JSONL_STORAGE_VERSION,
                created_at,
                cwd: cwd.clone(),
                parent_session_id: options.parent_session_id,
                legacy_parent_session_path: None,
                next_seq: None,
            };
            let storage = match create_storage(&path, header.clone(), cx).await {
                Ok(storage) => storage,
                Err(error) => {
                    discard_new_storage(None, Some(&path)).await;
                    return Err(error);
                }
            };
            let modified_at = match file_modified_at(&path).await {
                Ok(modified_at) => modified_at,
                Err(error) => {
                    discard_new_storage(Some(Arc::clone(&storage)), Some(&path)).await;
                    return Err(error);
                }
            };
            if let Err(error) = cx
                .check()
                .map_err(|_| aborted_error())
                .and_then(|()| self.ensure_open())
            {
                discard_new_storage(Some(Arc::clone(&storage)), Some(&path)).await;
                return Err(error);
            }
            let metadata = metadata_from_header(header, path.clone(), modified_at);
            match self.publish_open(&metadata, Arc::clone(&storage), &key) {
                Ok(session) => Ok(session),
                Err(error) => {
                    discard_new_storage(Some(storage), Some(&path)).await;
                    Err(error)
                }
            }
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
            let key = Self::session_key(&metadata.cwd, &metadata.id);
            {
                let open = self.open_guard()?;
                if open.contains_key(&key) {
                    return Err(SessionError::Invariant(format!(
                        "Session is already open: {}",
                        metadata.id
                    )));
                }
            }
            let _reservation = self.reserve_id(&key, &metadata.id)?;
            let (_, storage) = self.load_storage(metadata, cx).await?;
            if let Err(error) = cx
                .check()
                .map_err(|_| aborted_error())
                .and_then(|()| self.ensure_open())
            {
                discard_new_storage(Some(storage), None).await;
                return Err(error);
            }
            let result = self.publish_open(metadata, Arc::clone(&storage), &key);
            match result {
                Ok(session) => Ok(session),
                Err(error) => {
                    // The file remains durable; only this temporary open handle is released.
                    let _ = storage.close(&Context::background()).await;
                    Err(error)
                }
            }
        })
    }

    fn list<'a>(
        &'a self,
        options: Option<Self::ListOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Self::Metadata>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let cwd = match options.and_then(|options| options.cwd) {
                Some(cwd) => Some(resolve_cwd(&cwd).await?),
                None => None,
            };
            cx.check().map_err(|_| aborted_error())?;
            let paths = list_session_files(&self.root, cwd.as_deref()).await?;
            cx.check().map_err(|_| aborted_error())?;
            let mut metadata = Vec::new();
            for path in paths {
                cx.check().map_err(|_| aborted_error())?;
                let header = match read_header(&path, cx).await {
                    Ok(Some(header)) => header,
                    Ok(None) => continue,
                    Err(SessionError::Backend(failure))
                        if matches!(
                            failure.code,
                            StorageErrorCode::InvalidHeader | StorageErrorCode::Corrupt
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if cwd.as_deref().is_some_and(|cwd| cwd != header.cwd) {
                    continue;
                }
                let modified_at = file_modified_at(&path).await?;
                cx.check().map_err(|_| aborted_error())?;
                metadata.push(metadata_from_header(header, path, modified_at));
            }
            metadata.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| left.id.cmp(&right.id))
                    .then_with(|| left.cwd.cmp(&right.cwd))
            });
            Ok(metadata)
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
            let key = Self::session_key(&metadata.cwd, &metadata.id);
            {
                let open = self.open_guard()?;
                if open.contains_key(&key) {
                    return Err(SessionError::Invariant(format!(
                        "Session is open: {}",
                        metadata.id
                    )));
                }
            }
            let _reservation = self.reserve_id(&key, &metadata.id)?;
            if !path_exists(&metadata.path).await? {
                return Err(not_found(format!(
                    "session file does not exist: {}",
                    metadata.path.display()
                )));
            }
            verify_owned_session_path(&self.root, &metadata.cwd, &metadata.id, &metadata.path)
                .await?;
            // A legacy raw id can share an encoded filename; a readable
            // header is the authoritative identity check before deletion.
            // If the header is empty or corrupt, the ownership verdict above
            // still permits removing the stranded session file.
            if let Some(header) = read_header(&metadata.path, cx).await?
                && (header.id != metadata.id || header.cwd != metadata.cwd)
            {
                return Err(SessionError::Invariant(format!(
                    "Session identity does not match header: {}",
                    metadata.id
                )));
            }
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            remove_session_file(&metadata.path).await
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
            let created_at = pi_agent::now_millis();
            let source_snapshot = self.capture_source_snapshot(source, cx).await?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let cwd = source.cwd.clone();
            let id = match &options {
                ForkOptions::Branch { id: Some(id), .. } | ForkOptions::Tree { id: Some(id) } => {
                    id.clone()
                }
                ForkOptions::Branch { id: None, .. } | ForkOptions::Tree { id: None } => {
                    self.id_generator.next(Some(created_at))?
                }
            };
            let key = Self::session_key(&cwd, &id);
            let _reservation = self.reserve_id(&key, &id)?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let directory = self.session_directory(&cwd);
            let path = resolve_new_session_path(&directory, created_at, &id).await?;
            let snapshot = create_fork_snapshot(&source_snapshot, &options)?;
            cx.check().map_err(|_| aborted_error())?;
            self.ensure_open()?;
            let header = JsonlStorageHeader {
                v: JSONL_FORMAT_VERSION,
                kind: "header".to_owned(),
                id: id.clone(),
                storage_version: JSONL_STORAGE_VERSION,
                created_at,
                cwd: cwd.clone(),
                parent_session_id: Some(source.id.clone()),
                legacy_parent_session_path: None,
                next_seq: Some(snapshot.next_seq),
            };
            let storage = match create_fork_storage(&path, header.clone(), &snapshot, cx).await {
                Ok(storage) => storage,
                Err(error) => {
                    discard_new_storage(None, Some(&path)).await;
                    return Err(error);
                }
            };
            let modified_at = match file_modified_at(&path).await {
                Ok(modified_at) => modified_at,
                Err(error) => {
                    discard_new_storage(Some(Arc::clone(&storage)), Some(&path)).await;
                    return Err(error);
                }
            };
            if let Err(error) = cx
                .check()
                .map_err(|_| aborted_error())
                .and_then(|()| self.ensure_open())
            {
                discard_new_storage(Some(Arc::clone(&storage)), Some(&path)).await;
                return Err(error);
            }
            let metadata = metadata_from_header(header, path.clone(), modified_at);
            match self.publish_open(&metadata, Arc::clone(&storage), &key) {
                Ok(session) => Ok(session),
                Err(error) => {
                    discard_new_storage(Some(storage), Some(&path)).await;
                    Err(error)
                }
            }
        })
    }
}

fn metadata_from_header(
    header: JsonlStorageHeader,
    path: PathBuf,
    modified_at: f64,
) -> JsonlSessionMetadata {
    JsonlSessionMetadata {
        id: header.id,
        created_at: header.created_at,
        storage_version: header.storage_version,
        cwd: header.cwd,
        parent_session_id: header.parent_session_id,
        legacy_parent_session_path: header.legacy_parent_session_path,
        path,
        modified_at,
    }
}

/// Reads a JSONL session header, returning `None` when the file is absent,
/// empty, or not a recognized JSONL session format.
async fn read_header(
    path: &Path,
    cx: &Context,
) -> Result<Option<JsonlStorageHeader>, SessionError> {
    cx.check().map_err(aborted)?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        let mut reader = match File::open(&path) {
            Ok(file) => BufReader::new(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(failure(
                    StorageErrorCode::Io,
                    format!("failed to read JSONL header {}", path.display()),
                    Some(Arc::new(error)),
                ));
            }
        };
        if let Err(error) = reader.read_line(&mut line) {
            return Err(failure(
                StorageErrorCode::Io,
                format!("failed to read JSONL header {}", path.display()),
                Some(Arc::new(error)),
            ));
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            return Ok(None);
        }
        match codec::parse_header(line) {
            Ok(codec::ParsedHeader::V4(header)) => Ok(Some(header)),
            Ok(codec::ParsedHeader::LegacyV3(header)) => {
                legacy_v3::normalize_legacy_v3_header(&path, &header).map(Some)
            }
            Err(_) => Ok(None),
        }
    })
    .await
    .map_err(|error| join_error(error, "JSONL header task failed"))?
}

async fn create_storage(
    path: &Path,
    header: JsonlStorageHeader,
    cx: &Context,
) -> Result<Arc<JsonlStorage>, SessionError> {
    cx.check().map_err(aborted)?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || JsonlStorage::create_sync(&path, header))
        .await
        .map_err(|error| join_error(error, "JSONL create task failed"))?
}

async fn open_storage(
    path: &Path,
    cx: &Context,
) -> Result<(JsonlStorageHeader, Arc<JsonlStorage>), SessionError> {
    cx.check().map_err(aborted)?;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || JsonlStorage::open_sync(&path))
        .await
        .map_err(|error| join_error(error, "JSONL open task failed"))?
}

async fn create_fork_storage(
    path: &Path,
    header: JsonlStorageHeader,
    snapshot: &ForkDestinationSnapshot,
    cx: &Context,
) -> Result<Arc<JsonlStorage>, SessionError> {
    cx.check().map_err(aborted)?;
    let path = path.to_owned();
    let snapshot = snapshot.clone();
    tokio::task::spawn_blocking(move || {
        JsonlStorage::create_from_fork_sync(&path, header, &snapshot)
    })
    .await
    .map_err(|error| join_error(error, "JSONL fork task failed"))?
}

async fn discard_new_storage(storage: Option<Arc<JsonlStorage>>, path: Option<&Path>) {
    let cleanup_context = Context::background();
    if let Some(storage) = storage {
        let _ = storage.close(&cleanup_context).await;
    }
    if let Some(path) = path {
        let _ = discard_session_file(path).await;
    }
}

async fn resolve_cwd(input: &str) -> Result<String, SessionError> {
    let input = input.to_owned();
    let path_for_error = PathBuf::from(&input);
    tokio::task::spawn_blocking(move || {
        Ok(crate::core::config::resolve_path(input)
            .to_string_lossy()
            .into_owned())
    })
    .await
    .map_err(|source| io_failure(&path_for_error, "session cwd worker failed", source))?
}

async fn path_exists(path: &Path) -> Result<bool, SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    let path_for_worker_error = path.clone();
    tokio::task::spawn_blocking(move || match fs::metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_failure(
            &path_for_error,
            "failed to check session",
            source,
        )),
    })
    .await
    .map_err(|source| {
        io_failure(
            &path_for_worker_error,
            "session check worker failed",
            source,
        )
    })?
}

async fn file_modified_at(path: &Path) -> Result<f64, SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    let path_for_worker_error = path.clone();
    tokio::task::spawn_blocking(move || {
        let modified = fs::metadata(&path)
            .map_err(|source| {
                io_failure(&path_for_error, "failed to read session metadata", source)
            })?
            .modified()
            .map_err(|source| {
                io_failure(
                    &path_for_error,
                    "failed to read session modification time",
                    source,
                )
            })?;
        Ok(system_time_millis_f64(modified))
    })
    .await
    .map_err(|source| {
        io_failure(
            &path_for_worker_error,
            "session metadata worker failed",
            source,
        )
    })?
}

/// [`SystemTime`] → fractional Unix milliseconds, matching the precision and
/// sign of Node's `stats.mtimeMs` (sub-millisecond fraction, negative before
/// the epoch).
fn system_time_millis_f64(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64() * 1000.0,
        Err(error) => -error.duration().as_secs_f64() * 1000.0,
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

fn join_error(error: tokio::task::JoinError, action: &str) -> SessionError {
    failure(StorageErrorCode::Io, action, Some(Arc::new(error)))
}

fn not_found(message: String) -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::NotFound, message))
}

fn repo_closed_error() -> SessionError {
    SessionError::Backend(StorageFailure::new(
        StorageErrorCode::Closed,
        "session repository is closed",
    ))
}

fn aborted_error() -> SessionError {
    SessionError::Backend(StorageFailure::new(
        StorageErrorCode::Aborted,
        "operation cancelled",
    ))
}

#[cfg(test)]
mod tests {
    use super::super::paths::session_file_name;
    use super::*;
    use tempfile::tempdir;

    /// Creates then closes a session and returns its listed metadata.
    #[expect(clippy::expect_used, reason = "test setup")]
    async fn create_closed_metadata(
        repo: &JsonlSessionRepo,
        cwd: &str,
        id: &str,
    ) -> JsonlSessionMetadata {
        let cx = Context::background();
        let session = repo
            .create(
                JsonlSessionCreateOptions {
                    cwd: cwd.to_owned(),
                    id: Some(id.to_owned()),
                    ..JsonlSessionCreateOptions::default()
                },
                &cx,
            )
            .await
            .expect("create session");
        session.close(&cx).await.expect("close session");
        repo.list(
            Some(JsonlSessionListOptions {
                cwd: Some(cwd.to_owned()),
            }),
            &cx,
        )
        .await
        .expect("list sessions")
        .into_iter()
        .find(|metadata| metadata.id == id)
        .expect("created session is listed")
    }

    #[expect(
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions use expect and panic for irrecoverable failures"
    )]
    #[tokio::test]
    async fn open_rejects_session_file_outside_repository() {
        let cx = Context::background();
        let root = tempdir().expect("root tempdir");
        let foreign = tempdir().expect("foreign tempdir");
        let cwd_dir = tempdir().expect("cwd tempdir");
        let cwd = cwd_dir.path().to_string_lossy().into_owned();

        // A valid session file owned by a different repository root, using the
        // same identity so only the location check can reject it.
        let foreign_repo = JsonlSessionRepo::new(foreign.path());
        let foreign_metadata = create_closed_metadata(&foreign_repo, &cwd, "shared-id").await;

        let repo = JsonlSessionRepo::new(root.path());
        let mut metadata = create_closed_metadata(&repo, &cwd, "shared-id").await;
        metadata.path = foreign_metadata.path.clone();
        let Err(error) = repo.open(&metadata, &cx).await else {
            panic!("open must reject a file outside this repository");
        };
        assert!(matches!(error, SessionError::Invariant(_)), "{error:?}");
        assert!(foreign_metadata.path.exists());
    }

    #[expect(clippy::expect_used, reason = "test assertions use expect")]
    #[tokio::test]
    async fn delete_rejects_session_file_outside_repository() {
        let cx = Context::background();
        let root = tempdir().expect("root tempdir");
        let foreign = tempdir().expect("foreign tempdir");
        let cwd_dir = tempdir().expect("cwd tempdir");
        let cwd = cwd_dir.path().to_string_lossy().into_owned();

        let foreign_repo = JsonlSessionRepo::new(foreign.path());
        let foreign_metadata = create_closed_metadata(&foreign_repo, &cwd, "shared-id").await;

        let repo = JsonlSessionRepo::new(root.path());
        let mut metadata = create_closed_metadata(&repo, &cwd, "shared-id").await;

        // Absolute path into another repository's file.
        metadata.path = foreign_metadata.path.clone();
        let error = repo
            .delete(&metadata, &cx)
            .await
            .expect_err("delete must reject a file outside this repository");
        assert!(matches!(error, SessionError::Invariant(_)), "{error:?}");
        assert!(foreign_metadata.path.exists());

        // The same target reached through `..` traversal below the root is
        // rejected after canonicalization.
        let relative = foreign_metadata
            .path
            .strip_prefix(foreign.path())
            .expect("foreign file below foreign root");
        metadata.path = root
            .path()
            .join("..")
            .join(foreign.path().file_name().expect("foreign dir name"))
            .join(relative);
        let error = repo
            .delete(&metadata, &cx)
            .await
            .expect_err("delete must reject a traversing path");
        assert!(matches!(error, SessionError::Invariant(_)), "{error:?}");
        assert!(foreign_metadata.path.exists());
    }

    #[expect(
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions use expect and panic for irrecoverable failures"
    )]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_open_and_delete_are_mutually_exclusive() {
        let cx = Context::background();
        let root = tempdir().expect("root tempdir");
        let cwd_dir = tempdir().expect("cwd tempdir");
        let cwd = cwd_dir.path().to_string_lossy().into_owned();
        let repo = JsonlSessionRepo::new(root.path());
        for round in 0..8 {
            let id = format!("race-{round}");
            let metadata = create_closed_metadata(&repo, &cwd, &id).await;
            let (opened, deleted) =
                tokio::join!(repo.open(&metadata, &cx), repo.delete(&metadata, &cx));
            match (opened, deleted) {
                (Ok(session), Err(_)) => {
                    assert!(metadata.path.exists(), "open winner keeps the file");
                    session.close(&cx).await.expect("close raced-open session");
                    repo.delete(&metadata, &cx)
                        .await
                        .expect("delete after close");
                }
                (Err(_), Ok(())) => {
                    assert!(!metadata.path.exists(), "delete winner removes the file");
                }
                (Ok(_), Ok(())) => panic!("open and delete must not both succeed"),
                (Err(open_error), Err(delete_error)) => {
                    panic!("one side must win: open={open_error:?} delete={delete_error:?}")
                }
            }
        }
    }

    #[expect(clippy::expect_used, reason = "test assertions use expect")]
    #[tokio::test]
    async fn delete_rejects_percent_encoded_id_collision() {
        let cx = Context::background();
        let root = tempdir().expect("root tempdir");
        let cwd_dir = tempdir().expect("cwd tempdir");
        let cwd = cwd_dir.path().to_string_lossy().into_owned();
        let repo = JsonlSessionRepo::new(root.path());
        let raw = create_closed_metadata(&repo, &cwd, "%20").await;

        // A session id that is itself a percent escape collides with the
        // encoded file name of a literal-space id; the header must decide.
        let colliding = raw
            .path
            .parent()
            .expect("session directory")
            .join(session_file_name(raw.created_at, " "));
        fs::rename(&raw.path, &colliding).expect("rename to colliding file name");

        let mut metadata = raw.clone();
        metadata.id = " ".to_owned();
        metadata.path = colliding.clone();
        let error = repo
            .delete(&metadata, &cx)
            .await
            .expect_err("delete must reject a filename-only ownership match");
        assert!(matches!(error, SessionError::Invariant(_)), "{error:?}");
        assert!(colliding.exists(), "foreign session file remains");
    }
}
