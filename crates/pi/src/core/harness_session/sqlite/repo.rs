//! Durable SQLite session repository, mirroring the source `sqlite-node`
//! backend (`repo.ts`, `session-row.ts`, `storage.ts`).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::session::{
    create_fork_snapshot, ForkOptions, ForkSource, ForkSourceSnapshot, IdGenerator,
    Session, SessionError, SessionMetadata, SessionMetadataLike, SessionRepo, Storage,
    StorageBackedSession, StorageErrorCode, StorageFailure, UuidV7Generator,
};

use super::schema::{SQLITE_SESSION_EXTENSION, SQLITE_STORAGE_VERSION};
use super::storage::{self, SqliteStorage};

fn aborted() -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::Aborted, "operation cancelled"))
}

fn not_found(message: impl Into<String>) -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::NotFound, message))
}

fn repo_closed() -> SessionError {
    SessionError::Backend(StorageFailure::new(StorageErrorCode::Closed, "SqliteSessionRepo is closed"))
}

fn invariant(message: impl Into<String>) -> SessionError {
    SessionError::Invariant(message.into())
}

fn io_failure(path: &Path, action: &str, source: impl std::error::Error + Send + Sync + 'static) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Io,
        message: format!("{action}: {}", path.display()),
        source: Some(Arc::new(source)),
    })
}

fn join_failure(operation: &str, source: tokio::task::JoinError) -> SessionError {
    SessionError::Backend(StorageFailure {
        code: StorageErrorCode::Io,
        message: format!("{operation} worker failed"),
        source: Some(Arc::new(source)),
    })
}

fn is_not_found(error: &SessionError) -> bool {
    matches!(error, SessionError::Backend(failure) if failure.code == StorageErrorCode::NotFound)
}

/// Options for creating a SQLite session.
#[derive(Clone, Debug, Default)]
pub struct SqliteSessionCreateOptions {
    /// Explicit session identity, or `None` to generate a UUIDv7.
    pub id: Option<String>,
    /// Parent session identity for a forked session.
    pub parent_session_id: Option<String>,
}

/// Metadata for a stored SQLite session: the backend-neutral projection with
/// the canonical container path replacing `cwd`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SqliteSessionMetadata {
    /// Backend-neutral session identity and provenance; `cwd` is always
    /// `None` in this backend.
    #[serde(flatten)]
    pub session: SessionMetadata,
    /// Canonical path of the container file holding the session row.
    pub path: PathBuf,
}

impl SessionMetadataLike for SqliteSessionMetadata {}

impl SqliteSessionMetadata {
    fn from_row(
        id: String,
        created_at: i64,
        parent_session_id: Option<String>,
        path: PathBuf,
    ) -> Self {
        Self {
            session: SessionMetadata {
                id,
                created_at,
                storage_version: SQLITE_STORAGE_VERSION,
                cwd: None,
                parent_session_id,
                legacy_parent_session_path: None,
            },
            path,
        }
    }
}

/// Constructor options mirroring the source `SqliteSessionRepoOptions`.
#[derive(Clone, Debug)]
pub struct SqliteSessionRepoOptions {
    /// Directory holding one encoded `<id>.sqlite` container per session.
    pub directory: PathBuf,
    /// Optional single shared container; when set, every session lives in this
    /// file and `directory` is only used to create the parent directory of the
    /// shared container when it does not yet exist.
    pub database_path: Option<PathBuf>,
}

/// Physical identity of an open storage: canonical container path plus session
/// id. Keeping the path typed avoids lossy display-string collisions.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StorageIdentity {
    path: PathBuf,
    id: String,
}

/// SQLite session repository.
///
/// Mirrors the source placement rules: per-session containers named
/// `id.sqlite` (safe ids verbatim, others `~` + UTF16LE/base64url) or one
/// shared database carrying every session row. Identity is the canonical
/// container path plus the session id, so a metadata record with a foreign
/// path is rejected. Closing the repository closes every still-open session.
pub struct SqliteSessionRepo {
    directory: PathBuf,
    database_path: Option<PathBuf>,
    id_generator: Arc<UuidV7Generator>,
    pending: Arc<StdMutex<HashSet<String>>>,
    open: Arc<StdMutex<HashMap<StorageIdentity, OpenRecord>>>,
    closed: AtomicBool,
}

struct OpenRecord {
    storage: Arc<SqliteStorage>,
    dyn_storage: Arc<dyn Storage>,
    session: Arc<dyn Session>,
    _reservation: IdReservation,
}

struct IdReservation {
    pending: Arc<StdMutex<HashSet<String>>>,
    id: String,
}

impl Drop for IdReservation {
    fn drop(&mut self) {
        let mut pending = match self.pending.lock() {
            Ok(pending) => pending,
            Err(poisoned) => poisoned.into_inner(),
        };
        pending.remove(&self.id);
    }
}

/// Holds multiple close failures and formats them as a single error source.
#[derive(Debug)]
struct CloseErrors(Vec<SessionError>);

impl fmt::Display for CloseErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} close failures", self.0.len())?;
        for (index, error) in self.0.iter().enumerate() {
            write!(formatter, "; [{index}] {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CloseErrors {}

fn lock_failed(operation: &str) -> SessionError {
    invariant(format!("{operation}: session id registry poisoned"))
}

/// Matches the source safe id rule: filename-safe ASCII only; `/` and `\` are
/// path separators and therefore unsafe.
fn is_safe_session_file_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Encodes a session id into its container filename: safe ids verbatim,
/// otherwise `~` + base64url over the UTF-16LE bytes.
fn session_file_name(id: &str) -> String {
    if is_safe_session_file_id(id) {
        format!("{id}{SQLITE_SESSION_EXTENSION}")
    } else {
        let mut utf16le = Vec::with_capacity(id.len() * 2);
        for unit in id.encode_utf16() {
            utf16le.extend_from_slice(&unit.to_le_bytes());
        }
        format!("~{}{SQLITE_SESSION_EXTENSION}", URL_SAFE_NO_PAD.encode(utf16le))
    }
}
fn storage_identity(path: &Path, session_id: &str) -> StorageIdentity {
    StorageIdentity {
        path: path.to_path_buf(),
        id: session_id.to_owned(),
    }
}

fn parent_directory(path: &Path) -> Result<&Path, SessionError> {
    Ok(path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(".")))
}

/// Removes a container and its SQLite sidecars. The primary file must exist;
/// missing sidecars are tolerated, but other I/O errors surface.
async fn remove_session_files(path: &Path) -> Result<(), SessionError> {
    remove_one(path).await?;
    remove_one_if_present(&sidecar_path(path, "-wal")).await?;
    remove_one_if_present(&sidecar_path(path, "-shm")).await?;
    Ok(())
}

/// Removes a newly reserved container during failure cleanup. Every path may
/// already be absent, but non-NotFound I/O errors still surface to the caller.
async fn remove_session_files_if_present(path: &Path) -> Result<(), SessionError> {
    remove_one_if_present(path).await?;
    remove_one_if_present(&sidecar_path(path, "-wal")).await?;
    remove_one_if_present(&sidecar_path(path, "-shm")).await?;
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

async fn remove_one(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(not_found(format!(
            "session file does not exist: {}",
            path.display()
        ))),
        Err(source) => Err(io_failure(&path, "failed to remove SQLite session file", source)),
    })
    .await
    .map_err(|source| join_failure("session removal", source))?
}

async fn remove_one_if_present(path: &Path) -> Result<(), SessionError> {
    match remove_one(path).await {
        Err(error) if is_not_found(&error) => Ok(()),
        result => result,
    }
}

async fn create_dir_all(path: &Path) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&path))
        .await
        .map_err(|source| join_failure("session directory", source))?
        .map_err(|source| io_failure(&path_for_error, "failed to create SQLite session directory", source))
}

async fn canonical_path(path: &Path) -> Result<PathBuf, SessionError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::fs::canonicalize(&path).map_err(|source| {
            let code = if source.kind() == std::io::ErrorKind::NotFound {
                StorageErrorCode::NotFound
            } else {
                StorageErrorCode::Io
            };
            SessionError::Backend(StorageFailure {
                code,
                message: format!("failed to canonicalize {}", path.display()),
                source: Some(Arc::new(source)),
            })
        })
    })
    .await
    .map_err(|source| join_failure("session canonicalize", source))?
}

async fn reserve_container(path: &Path, id: &str) -> Result<(), SessionError> {
    let path = path.to_path_buf();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(|file| drop(file))
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::AlreadyExists {
                    invariant(format!("SQLite session already exists: {id}"))
                } else {
                    io_failure(&path, "failed to reserve SQLite session file", source)
                }
            })
    })
    .await
    .map_err(|source| join_failure("session reservation", source))?
}

async fn list_container_paths(directory: &Path) -> Result<Vec<PathBuf>, SessionError> {
    let directory = directory.to_path_buf();
    tokio::task::spawn_blocking(move || match std::fs::read_dir(&directory) {
        Ok(entries) => {
            let mut paths = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(SQLITE_SESSION_EXTENSION) {
                    paths.push(directory.join(name));
                }
            }
            Ok(paths)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(io_failure(&directory, "failed to list SQLite session directory", source)),
    })
    .await
    .map_err(|source| join_failure("session listing", source))?
}

async fn cleanup_created(
    path: &Path,
    session_id: &str,
    shared: bool,
    storage: Option<Arc<SqliteStorage>>,
) {
    if let Some(storage) = storage {
        let _ = storage.close(&Context::background()).await;
        drop(storage);
    }
    if shared {
        let path = path.to_path_buf();
        let session_id = session_id.to_owned();
        let _ = tokio::task::spawn_blocking(move || storage::delete_session_row(&path, &session_id))
            .await;
    } else {
        let _ = remove_session_files_if_present(path).await;
    }
}

impl SqliteSessionRepo {
    /// Builds a repository from `options`. Paths are stored verbatim, matching
    /// the source; canonicalization happens at point of use.
    #[must_use]
    pub fn new(options: SqliteSessionRepoOptions) -> Self {
        Self {
            directory: options.directory,
            database_path: options.database_path,
            id_generator: Arc::new(UuidV7Generator::new()),
            pending: Arc::new(StdMutex::new(HashSet::new())),
            open: Arc::new(StdMutex::new(HashMap::new())),
            closed: AtomicBool::new(false),
        }
    }

    /// Closes every still-open session and seals new admissions.
    ///
    /// Each session close drains its admitted backend operations before the
    /// connection is released. One failure surfaces verbatim; several are
    /// aggregated without discarding their causes.
    pub async fn close(&self, cx: &Context) -> Result<(), SessionError> {
        cx.check().map_err(|_| aborted())?;
        self.closed.store(true, Ordering::Release);

        let sessions: Vec<Arc<dyn Session>> = {
            let open = self.open.lock().map_err(|_| lock_failed("close"))?;
            open.values().map(|record| Arc::clone(&record.session)).collect()
        };

        let mut errors = Vec::new();
        for session in sessions {
            if let Err(error) = session.close(cx).await {
                errors.push(error);
            }
        }

        match errors.len() {
            0 => Ok(()),
            1 => {
                let Some(error) = errors.pop() else {
                    return Ok(());
                };
                Err(error)
            }
            _ => {
                let Some(first) = errors.first().cloned() else {
                    return Ok(());
                };
                let source = Arc::new(CloseErrors(errors)) as Arc<dyn std::error::Error + Send + Sync>;
                Err(SessionError::Backend(StorageFailure {
                    code: StorageErrorCode::Closed,
                    message: format!("Failed to close SQLite Sessions: {first}"),
                    source: Some(source),
                }))
            }
        }
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            Err(repo_closed())
        } else {
            Ok(())
        }
    }

    fn reserve_id(&self, id: &str) -> Result<IdReservation, SessionError> {
        let mut pending = self.pending.lock().map_err(|_| lock_failed("reserve"))?;
        if pending.contains(id) {
            return Err(invariant(format!("Session is already open: {id}")));
        }
        pending.insert(id.to_owned());
        Ok(IdReservation {
            pending: Arc::clone(&self.pending),
            id: id.to_owned(),
        })
    }

    fn path_for_session(&self, id: &str) -> PathBuf {
        self.database_path
            .clone()
            .unwrap_or_else(|| self.directory.join(session_file_name(id)))
    }

    #[must_use]
    fn uses_shared_database(&self) -> bool {
        self.database_path.is_some()
    }

    async fn repository_path_for_metadata(
        &self,
        metadata: &SqliteSessionMetadata,
    ) -> Result<PathBuf, SessionError> {
        let expected_path = self.path_for_session(&metadata.session.id);
        let expected = canonical_path(&expected_path).await.map_err(|error| {
            if is_not_found(&error) {
                not_found(format!("session file does not exist: {}", expected_path.display()))
            } else {
                error
            }
        })?;
        let actual = canonical_path(&metadata.path).await.map_err(|error| {
            if is_not_found(&error) {
                not_found(format!("session file does not exist: {}", metadata.path.display()))
            } else {
                error
            }
        })?;
        if expected != actual {
            return Err(invariant(format!(
                "SQLite session metadata path is outside this repository: {}",
                metadata.path.display()
            )));
        }
        Ok(actual)
    }

    async fn capture_source_snapshot(
        &self,
        source: &SqliteSessionMetadata,
        cx: &Context,
    ) -> Result<ForkSourceSnapshot, SessionError> {
        cx.check().map_err(|_| aborted())?;
        let path = canonical_path(&source.path).await.map_err(|error| {
            if is_not_found(&error) {
                not_found(format!("session file does not exist: {}", source.path.display()))
            } else {
                error
            }
        })?;
        let identity = storage_identity(&path, &source.session.id);
        let open_storage = {
            let open = self.open.lock().map_err(|_| lock_failed("fork"))?;
            open.get(&identity).map(|record| Arc::clone(&record.storage))
        };
        if let Some(storage) = open_storage {
            return storage.capture_fork_source(cx).await;
        }
        let session_id = source.session.id.clone();
        tokio::task::spawn_blocking(move || storage::read_fork_source(&path, &session_id))
            .await
            .map_err(|source| join_failure("SQLite fork source", source))?
    }

    fn publish_open(
        &self,
        metadata: SqliteSessionMetadata,
        storage: Arc<SqliteStorage>,
        reservation: IdReservation,
    ) -> Result<Arc<dyn Session>, SessionError> {
        let mut open = self.open.lock().map_err(|_| lock_failed("publish"))?;
        self.ensure_open()?;
        let key = storage_identity(&metadata.path, &metadata.session.id);
        if open.contains_key(&key) {
            return Err(invariant(format!(
                "Session is already open: {}",
                metadata.session.id
            )));
        }

        let dyn_storage: Arc<dyn Storage> = storage.clone();
        let open_sessions = Arc::downgrade(&self.open);
        let callback_key = key.clone();
        let callback_storage = Arc::clone(&dyn_storage);
        let id_generator: Arc<dyn IdGenerator> = Arc::clone(&self.id_generator);
        let session = StorageBackedSession::new(
            metadata.session.clone(),
            Arc::clone(&dyn_storage),
            id_generator,
            Some(Box::new(move || {
                let Some(open_sessions) = open_sessions.upgrade() else {
                    return;
                };
                let mut open = match open_sessions.lock() {
                    Ok(open) => open,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Some(record) = open.get(&callback_key) {
                    if Arc::ptr_eq(&record.dyn_storage, &callback_storage) {
                        open.remove(&callback_key);
                    }
                }
            })),
        );
        let session: Arc<dyn Session> = session;
        open.insert(
            key,
            OpenRecord {
                storage,
                dyn_storage,
                session: Arc::clone(&session),
                _reservation: reservation,
            },
        );
        Ok(session)
    }
}

impl SessionRepo for SqliteSessionRepo {
    type Metadata = SqliteSessionMetadata;
    type CreateOptions = SqliteSessionCreateOptions;
    type ListOptions = ();

    fn create<'a>(
        &'a self,
        options: Self::CreateOptions,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Arc<dyn Session>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let created_at = pi_agent::now_millis();
            let id = match options.id {
                Some(id) => id,
                None => self.id_generator.next(Some(created_at))?,
            };
            let reservation = self.reserve_id(&id)?;

            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let path = self.path_for_session(&id);
            create_dir_all(parent_directory(&path)?).await?;

            let mut reserved_file = false;
            if !self.uses_shared_database() {
                reserve_container(&path, &id).await?;
                reserved_file = true;
            }

            let preliminary = SqliteSessionMetadata::from_row(
                id.clone(),
                created_at,
                options.parent_session_id.clone(),
                path.clone(),
            );
            let storage = match tokio::task::spawn_blocking({
                let path = path.clone();
                let preliminary = preliminary.clone();
                move || storage::create_session(&path, &preliminary)
            })
            .await
            {
                Ok(Ok(storage)) => storage,
                Ok(Err(error)) => {
                    if reserved_file {
                        let _ = remove_session_files_if_present(&path).await;
                    }
                    return Err(error);
                }
                Err(source) => {
                    if reserved_file {
                        let _ = remove_session_files_if_present(&path).await;
                    }
                    return Err(join_failure("SQLite create", source));
                }
            };

            let canonical = match canonical_path(&path).await {
                Ok(canonical) => canonical,
                Err(error) => {
                    cleanup_created(&path, &id, self.uses_shared_database(), Some(storage)).await;
                    return Err(error);
                }
            };

            let metadata = SqliteSessionMetadata::from_row(
                id,
                created_at,
                options.parent_session_id,
                canonical,
            );
            let session_id = metadata.session.id.clone();
            match self.publish_open(metadata, Arc::clone(&storage), reservation) {
                Ok(session) => Ok(session),
                Err(error) => {
                    cleanup_created(
                        &path,
                        &session_id,
                        self.uses_shared_database(),
                        Some(storage),
                    )
                    .await;
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
            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let reservation = self.reserve_id(&metadata.session.id)?;

            let path = self.repository_path_for_metadata(metadata).await?;
            let session_id = metadata.session.id.clone();
            let (stored, storage) = match tokio::task::spawn_blocking({
                let path = path.clone();
                move || storage::open_session(&path, &session_id)
            })
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => return Err(error),
                Err(source) => return Err(join_failure("SQLite open", source)),
            };

            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            match self.publish_open(stored, Arc::clone(&storage), reservation) {
                Ok(session) => Ok(session),
                Err(error) => {
                    let _ = storage.close(&Context::background()).await;
                    Err(error)
                }
            }
        })
    }

    fn list<'a>(
        &'a self,
        _options: Option<Self::ListOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Self::Metadata>, SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let paths: Vec<PathBuf> = if let Some(path) = self.database_path.clone() {
                vec![path]
            } else {
                list_container_paths(&self.directory).await?
            };

            let mut sessions = Vec::new();
            for path in paths {
                let Ok(canonical) = canonical_path(&path).await else {
                    continue;
                };
                match tokio::task::spawn_blocking({
                    let canonical = canonical.clone();
                    move || storage::list_container(&canonical)
                })
                .await
                {
                    Ok(Ok(rows)) => sessions.extend(rows),
                    _ => continue,
                }
            }

            sessions.sort_by(|left, right| {
                right
                    .session
                    .created_at
                    .cmp(&left.session.created_at)
                    .then_with(|| left.session.id.cmp(&right.session.id))
                    .then_with(|| left.path.cmp(&right.path))
            });
            Ok(sessions)
        })
    }

    fn delete<'a>(
        &'a self,
        metadata: &'a Self::Metadata,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move {
            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let _reservation = self.reserve_id(&metadata.session.id)?;
            let path = self.repository_path_for_metadata(metadata).await?;
            let session_id = metadata.session.id.clone();

            if self.uses_shared_database() {
                tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || storage::delete_session_row(&path, &session_id)
                })
                .await
                .map_err(|source| join_failure("SQLite delete", source))??;
            } else {
                tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || storage::verify_session_row(&path, &session_id)
                })
                .await
                .map_err(|source| join_failure("SQLite verify", source))??;
                remove_session_files(&path).await?;
            }
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
            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let created_at = pi_agent::now_millis();
            let id = match &options {
                ForkOptions::Branch { id: Some(id), .. } | ForkOptions::Tree { id: Some(id) } => {
                    id.clone()
                }
                ForkOptions::Branch { id: None, .. } | ForkOptions::Tree { id: None } => {
                    self.id_generator.next(Some(created_at))?
                }
            };
            let reservation = self.reserve_id(&id)?;

            let source_snapshot = self.capture_source_snapshot(source, cx).await?;
            let snapshot = create_fork_snapshot(&source_snapshot, &options)?;

            cx.check().map_err(|_| aborted())?;
            self.ensure_open()?;
            let path = self.path_for_session(&id);
            create_dir_all(parent_directory(&path)?).await?;

            let mut reserved_file = false;
            if !self.uses_shared_database() {
                reserve_container(&path, &id).await?;
                reserved_file = true;
            }

            let preliminary = SqliteSessionMetadata::from_row(
                id.clone(),
                created_at,
                Some(source.session.id.clone()),
                path.clone(),
            );
            let storage = match tokio::task::spawn_blocking({
                let path = path.clone();
                let preliminary = preliminary.clone();
                let snapshot = snapshot.clone();
                move || storage::create_fork_session(&path, &preliminary, &snapshot)
            })
            .await
            {
                Ok(Ok(storage)) => storage,
                Ok(Err(error)) => {
                    if reserved_file {
                        let _ = remove_session_files_if_present(&path).await;
                    }
                    return Err(error);
                }
                Err(source) => {
                    if reserved_file {
                        let _ = remove_session_files_if_present(&path).await;
                    }
                    return Err(join_failure("SQLite fork", source));
                }
            };

            let canonical = match canonical_path(&path).await {
                Ok(canonical) => canonical,
                Err(error) => {
                    cleanup_created(&path, &id, self.uses_shared_database(), Some(storage)).await;
                    return Err(error);
                }
            };

            let metadata = SqliteSessionMetadata::from_row(
                id,
                created_at,
                Some(source.session.id.clone()),
                canonical,
            );
            let session_id = metadata.session.id.clone();
            match self.publish_open(metadata, Arc::clone(&storage), reservation) {
                Ok(session) => Ok(session),
                Err(error) => {
                    cleanup_created(
                        &path,
                        &session_id,
                        self.uses_shared_database(),
                        Some(storage),
                    )
                    .await;
                    Err(error)
                }
            }
        })
    }
}
