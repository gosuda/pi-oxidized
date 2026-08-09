//! Whole-file locked credential storage for `auth.json`.
//!
//! [`FileLockBackend`] uses the same `${path}.lock` directory protocol as
//! TypeScript `proper-lockfile`, held across each read-modify-write transaction
//! and async refresh callback. A legacy `${path}.lock` *file* left by an older
//! binary is rejected rather than migrated in place: the advisory-file and
//! directory protocols cannot be made mutually exclusive at one path, so
//! unlinking a possibly-live inode would let an old process interleave
//! credential writes. Remove the stale file (or let the older process exit)
//! before retrying. Data commits remain same-directory atomic replacements
//! through [`tempfile::NamedTempFile`].

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::Builder;

use crate::lockfile::{LockError, LockGuard, LockOptions};

use super::config_value::resolve_config_value;
use super::error::{AuthError, StoreError};
use super::types::{ApiKeyCredential, Credential, CredentialInfo, CredentialStore};

/// Pretty-printed empty credential map written when seeding a missing file.
const EMPTY_JSON: &str = "{}";

/// Synchronous acquisition attempts used by the TypeScript authority.
const SYNC_LOCK_ATTEMPTS: u32 = 10;

/// Async retries after the initial acquisition attempt.
const ASYNC_LOCK_RETRIES: u32 = 10;

/// Delay between synchronous lock attempts.
const SYNC_LOCK_DELAY: Duration = Duration::from_millis(20);

/// Initial async backoff between lock attempts.
const LOCK_MIN_DELAY: Duration = Duration::from_millis(100);

/// Cap for exponential lock-retry backoff.
const LOCK_MAX_DELAY: Duration = Duration::from_secs(10);

/// Stale threshold for the auth storage lock, shared by the sync and async
/// acquisition paths so two writers of the same lock agree on when a holder
/// may be reclaimed. Matches the TypeScript async auth storage's 30-second
/// `stale` value; the sync path sets it explicitly rather than relying on
/// `LockOptions::new()`'s 10-second default, which previously let it reclaim
/// a lock the async path still considered live.
const AUTH_LOCK_STALE: Duration = Duration::from_secs(30);

/// Shared file-backed JSON map with exclusive locking and atomic replace.
///
/// The lock is a sibling `${path}.lock` directory, so replacing the data file
/// never releases cross-process mutual exclusion.
#[derive(Clone, Debug)]
pub struct FileLockBackend {
    path: PathBuf,
    /// Canonical path used for all lock acquisition, reads, and atomic writes.
    ///
    /// Resolved once at construction so that locking, reading, and persisting
    /// never diverge across symlink spellings: `NamedTempFile::persist` calls
    /// `rename(2)`, which does not follow a symlink at the final component and
    /// would otherwise replace the symlink with a regular file, splitting the
    /// credential file from its lock target.
    effective_path: PathBuf,
    #[cfg(test)]
    lock_contention: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    atomic_write_failure: Arc<Mutex<Option<std::io::ErrorKind>>>,
    #[cfg(test)]
    async_lock_retry_delay: Option<Duration>,
}

impl FileLockBackend {
    /// Create a backend for the given JSON data path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let effective_path = canonical_lock_target(&path).unwrap_or_else(|_| path.clone());
        Self {
            path,
            effective_path,
            #[cfg(test)]
            lock_contention: None,
            #[cfg(test)]
            atomic_write_failure: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            async_lock_retry_delay: None,
        }
    }

    #[cfg(test)]
    fn notify_lock_contention(&self) {
        if let Some(notify) = &self.lock_contention {
            notify.notify_one();
        }
    }

    #[cfg(test)]
    fn fail_next_atomic_write(&self, kind: std::io::ErrorKind) {
        let mut failure = self
            .atomic_write_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *failure = Some(kind);
    }

    /// Path of the JSON data file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of the sibling exclusive-lock file.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        lock_path_for(&self.effective_path)
    }

    /// Synchronous locked critical section.
    ///
    /// `fn` receives the current file contents (`None` only if the file vanishes
    /// after seeding). Returning `Ok((value, Some(next)))` replaces the file
    /// with `next`; `Ok((value, None))` leaves bytes unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when directory/file setup, lock acquisition,
    /// reading, callback execution, or atomic persistence fails.
    pub fn with_lock_sync<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(Option<&str>) -> Result<(T, Option<String>), StoreError>,
    {
        self.ensure_parent_dir()?;
        let guard = self.acquire_sync_lock()?;
        Self::check_lock_identity(&guard)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next) = f(current.as_deref())?;
        Self::check_lock_identity(&guard)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            Self::check_lock_identity(&guard)?;
        }
        Ok(result)
    }

    /// Synchronous locked initialization or migration transaction.
    ///
    /// Unlike [`Self::with_lock_sync`], this method does not seed a missing
    /// data file before invoking `f`. The callback can therefore distinguish a
    /// missing destination from an existing one and install initial content
    /// without racing normal readers or writers using the same sibling lock.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when directory/file setup, lock acquisition,
    /// reading, callback execution, or atomic persistence fails.
    pub fn with_lock_sync_unseeded<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(Option<&str>) -> Result<(T, Option<String>), StoreError>,
    {
        self.ensure_parent_dir()?;
        let guard = self.acquire_sync_lock()?;
        Self::check_lock_identity(&guard)?;
        let current = self.read_data_file()?;
        let (result, next) = f(current.as_deref())?;
        Self::check_lock_identity(&guard)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            Self::check_lock_identity(&guard)?;
        }
        Ok(result)
    }

    /// Async locked critical section.
    ///
    /// The exclusive lock is held across the entire `fn` future so callers can
    /// perform network I/O (token refresh) without interleaving writers.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when directory/file setup, lock acquisition,
    /// reading, callback execution, or atomic persistence fails.
    pub async fn with_lock_async<T, F, Fut>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(Option<String>) -> Fut,
        Fut: Future<Output = Result<(T, Option<String>), StoreError>>,
    {
        self.ensure_parent_dir()?;
        let guard = self.acquire_async_lock().await?;
        Self::check_lock_identity(&guard)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next) = f(current).await?;
        Self::check_lock_identity(&guard)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            Self::check_lock_identity(&guard)?;
        }
        Ok(result)
    }

    async fn with_lock_async_commit<T, C, F, Fut, Commit>(
        &self,
        f: F,
        commit: Commit,
    ) -> Result<T, StoreError>
    where
        F: FnOnce(Option<String>) -> Fut,
        Fut: Future<Output = Result<(T, Option<String>, C), StoreError>>,
        Commit: FnOnce(C) -> Result<(), StoreError>,
    {
        self.ensure_parent_dir()?;
        let guard = self.acquire_async_lock().await?;
        Self::check_lock_identity(&guard)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next, committed) = f(current).await?;
        Self::check_lock_identity(&guard)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            Self::check_lock_identity(&guard)?;
        }
        commit(committed)?;
        Ok(result)
    }

    fn ensure_parent_dir(&self) -> Result<(), StoreError> {
        let Some(parent) = parent_dir(&self.effective_path) else {
            return Ok(());
        };
        if !parent.exists() {
            create_dir_all_secure(parent)?;
        }
        set_owner_dir_mode(parent)
    }

    fn ensure_data_file(&self) -> Result<(), StoreError> {
        if self.effective_path.exists() {
            return Ok(());
        }
        // Callers hold the sibling lock here, so the missing-path recheck and
        // seed commit are ordered with every normal read and write.
        self.atomic_write(EMPTY_JSON)
    }

    fn acquire_sync_lock(&self) -> Result<LockGuard, StoreError> {
        let unresolved = absolute_lock_target(&self.path)?;
        let target = self.effective_path.clone();
        // Reject legacy file locks at both spellings rather than migrating them:
        // an old process blocked on the advisory-file lock would otherwise
        // interleave credential writes after an in-place unlink.
        reject_legacy_file_locks(&[unresolved, target.clone()])?;
        let options = LockOptions::new().attempts(1).stale(AUTH_LOCK_STALE);
        for _ in 1..SYNC_LOCK_ATTEMPTS {
            match LockGuard::acquire_with(&target, &options) {
                Ok(guard) => return Ok(guard),
                Err(LockError::Contended { .. }) => std::thread::sleep(SYNC_LOCK_DELAY),
                Err(error) => return Err(lock_error_to_store(&error)),
            }
        }
        // Final attempt: contention here exhausts the budget. Name the wedged
        // lock directory so an operator knows what to inspect — the `Contended`
        // display text alone ("Lock file is already being held") carries no path.
        match LockGuard::acquire_with(&target, &options) {
            Ok(guard) => Ok(guard),
            Err(LockError::Contended { .. }) => Err(StoreError::message(format!(
                "Failed to acquire auth storage lock {} after {SYNC_LOCK_ATTEMPTS} attempts",
                lock_path_for(&target).display()
            ))),
            Err(error) => Err(lock_error_to_store(&error)),
        }
    }

    async fn acquire_async_lock(&self) -> Result<LockGuard, StoreError> {
        // Sync and async must resolve the same lock target so a symlinked auth
        // path cannot split ownership across two sibling directories. Both use
        // the canonical spelling, which maps every symlink spelling of the auth
        // path onto a single lock directory.
        let unresolved = absolute_lock_target(&self.path)?;
        let target = self.effective_path.clone();
        reject_legacy_file_locks(&[unresolved, target.clone()])?;
        let options = LockOptions::new().attempts(1).stale(AUTH_LOCK_STALE);
        let mut delay = LOCK_MIN_DELAY;
        // Tests substitute a near-zero initial delay so exhausting the full
        // retry budget takes milliseconds instead of most of a minute; the
        // doubling schedule itself stays on the production path.
        #[cfg(test)]
        if let Some(override_delay) = self.async_lock_retry_delay {
            delay = override_delay;
        }
        for retry in 0..ASYNC_LOCK_RETRIES {
            match LockGuard::acquire_with(&target, &options) {
                Ok(guard) => return Ok(guard),
                Err(LockError::Contended { .. }) => {
                    #[cfg(test)]
                    self.notify_lock_contention();
                    sleep_with_jitter(delay, retry).await;
                    delay = delay.saturating_mul(2).min(LOCK_MAX_DELAY);
                }
                Err(error) => return Err(lock_error_to_store(&error)),
            }
        }
        // Final attempt: contention here exhausts the retry budget. Name the
        // wedged lock directory so an operator knows what to inspect — the
        // `Contended` display text alone carries no path.
        match LockGuard::acquire_with(&target, &options) {
            Ok(guard) => Ok(guard),
            Err(LockError::Contended { .. }) => Err(StoreError::message(format!(
                "Failed to acquire auth storage lock {} after {ASYNC_LOCK_RETRIES} retries",
                lock_path_for(&target).display()
            ))),
            Err(error) => Err(lock_error_to_store(&error)),
        }
    }

    fn read_data_file(&self) -> Result<Option<String>, StoreError> {
        match fs::read_to_string(&self.effective_path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::message(format!(
                "Failed to read auth storage {}: {error}",
                self.effective_path.display()
            ))),
        }
    }

    fn atomic_write(&self, content: &str) -> Result<(), StoreError> {
        #[cfg(test)]
        {
            let failure = self
                .atomic_write_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(kind) = failure {
                return Err(StoreError::message(format!(
                    "Failed to persist auth storage {}: {}",
                    self.path.display(),
                    std::io::Error::from(kind)
                )));
            }
        }

        let parent = parent_dir(&self.effective_path).unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            create_dir_all_secure(parent)?;
        }

        let mut builder = Builder::new();
        builder.prefix(".auth-");
        builder.suffix(".tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o600));
        }

        let mut tmp = builder.tempfile_in(parent).map_err(|error| {
            StoreError::message(format!(
                "Failed to create temp auth storage in {}: {error}",
                parent.display()
            ))
        })?;

        tmp.write_all(content.as_bytes()).map_err(|error| {
            StoreError::message(format!(
                "Failed to write temp auth storage {}: {error}",
                tmp.path().display()
            ))
        })?;
        tmp.flush().map_err(|error| {
            StoreError::message(format!(
                "Failed to flush temp auth storage {}: {error}",
                tmp.path().display()
            ))
        })?;
        tmp.as_file().sync_all().map_err(|error| {
            StoreError::message(format!(
                "Failed to sync temp auth storage {}: {error}",
                tmp.path().display()
            ))
        })?;

        // Re-assert 0600 before persist so umask cannot leave a looser mode.
        set_owner_secret_mode(tmp.path())?;

        let tmp_path = tmp.path().to_path_buf();
        tmp.persist(&self.effective_path).map_err(|error| {
            StoreError::message(format!(
                "Failed to persist auth storage from {} to {}: {}",
                tmp_path.display(),
                self.effective_path.display(),
                error.error
            ))
        })?;

        set_owner_secret_mode(&self.effective_path)?;
        sync_parent_dir(parent);
        Ok(())
    }

    fn check_lock_identity(guard: &LockGuard) -> Result<(), StoreError> {
        guard
            .check_ownership()
            .map_err(|_| StoreError::message("Auth storage lock was compromised"))
    }
}

/// Credential store backed by a pretty-printed `auth.json` file.
#[derive(Clone)]
pub struct FileCredentialStore {
    backend: FileLockBackend,
    cache: Arc<Mutex<BTreeMap<String, Credential>>>,
}

impl FileCredentialStore {
    /// Open (or create) a file-backed credential store at `auth_path`.
    #[must_use]
    pub fn new(auth_path: impl Into<PathBuf>) -> Self {
        let store = Self {
            backend: FileLockBackend::new(auth_path),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        };
        store.reload();
        store
    }

    /// Path of the underlying `auth.json` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.backend.path()
    }

    /// Shared lock/atomic backend (for sibling stores that reuse the lifecycle).
    #[must_use]
    pub fn backend(&self) -> &FileLockBackend {
        &self.backend
    }

    /// Reload credentials from disk.
    ///
    /// On failure the last valid in-memory snapshot is preserved.
    pub fn reload(&self) {
        let loaded = self.backend.with_lock_sync(|content| {
            let data = parse_storage_data(content)?;
            Ok((data, None))
        });
        if let Ok(data) = loaded
            && let Ok(mut cache) = self.cache.lock()
        {
            *cache = data;
        }
    }

    fn cache_snapshot(&self) -> Result<BTreeMap<String, Credential>, StoreError> {
        self.cache
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| StoreError::message("Auth storage cache lock poisoned"))
    }
}

impl CredentialStore for FileCredentialStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async move {
            let cache = self.cache_snapshot()?;
            Ok(cache.get(provider_id).cloned().map(resolve_credential_copy))
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        Box::pin(async move {
            let cache = self.cache_snapshot()?;
            Ok(cache
                .iter()
                .map(|(provider_id, credential)| CredentialInfo {
                    provider_id: provider_id.clone(),
                    kind: credential.kind(),
                })
                .collect())
        })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: Box<
            dyn FnOnce(
                    Option<Credential>,
                ) -> BoxFuture<'static, Result<Option<Credential>, AuthError>>
                + Send
                + 'a,
        >,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async move {
            let provider = provider_id.to_owned();
            let cache = Arc::clone(&self.cache);
            self.backend
                .with_lock_async_commit(
                    move |content| {
                        let f = f;
                        async move {
                            let mut data = parse_storage_data(content.as_deref())?;
                            let current = data.get(&provider).cloned();
                            let next = f(current.clone()).await?;
                            match next {
                                // None means no-op: leave file bytes unchanged,
                                // then publish the authoritative locked snapshot.
                                None => Ok((current, None, data)),
                                Some(credential) => {
                                    data.insert(provider, credential.clone());
                                    let serialized = serialize_storage_data(&data)?;
                                    Ok((Some(credential), Some(serialized), data))
                                }
                            }
                        }
                    },
                    move |data| replace_cache_mutex(&cache, data),
                )
                .await
        })
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let provider = provider_id.to_owned();
            let cache = Arc::clone(&self.cache);
            self.backend
                .with_lock_async_commit(
                    move |content| async move {
                        let mut data = parse_storage_data(content.as_deref())?;
                        data.remove(&provider);
                        let serialized = serialize_storage_data(&data)?;
                        Ok(((), Some(serialized), data))
                    },
                    move |data| replace_cache_mutex(&cache, data),
                )
                .await
        })
    }
}

fn replace_cache_mutex(
    cache: &Mutex<BTreeMap<String, Credential>>,
    data: BTreeMap<String, Credential>,
) -> Result<(), StoreError> {
    let mut guard = cache
        .lock()
        .map_err(|_| StoreError::message("Auth storage cache lock poisoned"))?;
    *guard = data;
    Ok(())
}

/// One-off synchronous read of a stored credential without resolving templates.
///
/// Returns `None` when the file is missing, unreadable, malformed, or lacks the
/// provider entry. Never acquires the store lock.
#[must_use]
pub fn read_stored_credential(provider_id: &str, auth_path: &Path) -> Option<Credential> {
    let content = fs::read_to_string(auth_path).ok()?;
    let data: BTreeMap<String, Credential> = serde_json::from_str(&content).ok()?;
    data.get(provider_id).cloned()
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

fn parent_dir(path: &Path) -> Option<&Path> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Some(parent),
        _ => None,
    }
}

fn create_dir_all_secure(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder.create(path).map_err(|error| {
            StoreError::message(format!(
                "Failed to create auth storage directory {}: {error}",
                path.display()
            ))
        })?;
        // Re-assert 0700 on the leaf in case an intermediate already existed.
        set_owner_dir_mode(path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(|error| {
            StoreError::message(format!(
                "Failed to create auth storage directory {}: {error}",
                path.display()
            ))
        })
    }
}

fn set_owner_secret_mode(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            StoreError::message(format!(
                "Failed to set mode 0600 on {}: {error}",
                path.display()
            ))
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn set_owner_dir_mode(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            StoreError::message(format!(
                "Failed to set mode 0700 on {}: {error}",
                path.display()
            ))
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn sync_parent_dir(parent: &Path) {
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

fn absolute_lock_target(path: &Path) -> Result<PathBuf, StoreError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| {
            StoreError::message(format!(
                "Failed to resolve auth storage path {}: {error}",
                path.display()
            ))
        })
}

fn canonical_lock_target(path: &Path) -> Result<PathBuf, StoreError> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|error| {
            StoreError::message(format!(
                "Failed to resolve auth storage path {}: {error}",
                path.display()
            ))
        });
    }
    // Dangling symlink chain: `path.exists()` is false for a broken symlink, so
    // the generic parent-join below would return the link path itself and
    // `persist` would replace the symlink. Follow the full chain, resolving each
    // relative target against its link parent, and reject cycles as ELOOP.
    let mut current = path.to_path_buf();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    loop {
        if current.exists() {
            return fs::canonicalize(&current).map_err(|error| {
                StoreError::message(format!(
                    "Failed to resolve auth storage path {}: {error}",
                    current.display()
                ))
            });
        }
        let meta = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Err(StoreError::message(format!(
                    "Failed to inspect auth storage path {}: {err}",
                    current.display()
                )));
            }
        };
        if !meta.file_type().is_symlink() {
            break;
        }
        if !seen.insert(current.clone()) {
            return Err(StoreError::message(format!(
                "Too many levels of symbolic links (ELOOP) for {}",
                path.display()
            )));
        }
        if seen.len() > 40 {
            return Err(StoreError::message(format!(
                "Too many levels of symbolic links (ELOOP) for {}",
                path.display()
            )));
        }
        let link_target = fs::read_link(&current).map_err(|error| {
            StoreError::message(format!(
                "Failed to resolve auth storage symlink {}: {error}",
                current.display()
            ))
        })?;
        let link_parent = parent_dir(&current).unwrap_or_else(|| Path::new("."));
        let target_path = if link_target.is_absolute() {
            link_target
        } else {
            link_parent.join(link_target)
        };
        if seen.contains(&target_path) {
            return Err(StoreError::message(format!(
                "Too many levels of symbolic links (ELOOP) for {}",
                path.display()
            )));
        }
        current = target_path;
    }
    let parent = parent_dir(&current).unwrap_or_else(|| Path::new("."));
    let parent = match fs::canonicalize(parent) {
        Ok(canonical) => canonical,
        Err(_) => absolute_lock_target(parent)?,
    };
    let Some(file_name) = current.file_name() else {
        return Err(StoreError::message(format!(
            "Auth storage path {} has no file name",
            current.display()
        )));
    };
    Ok(parent.join(file_name))
}

async fn sleep_with_jitter(delay: Duration, _retry: u32) {
    let jitter_ms = u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 37);
    let jitter = Duration::from_millis(jitter_ms);
    tokio::time::sleep(delay.saturating_add(jitter)).await;
}

/// Reject any legacy `${path}.lock` file left by an older binary.
///
/// The directory protocol and the legacy advisory-file protocol cannot be made
/// mutually exclusive at a single path: an old process that has the file open
/// and is blocked on its flock would, after an in-place migration, acquire the
/// flock on an orphaned inode and write concurrently with a directory holder.
/// We therefore refuse to proceed while a legacy file lock is present rather
/// than unlink or migrate a possibly-live inode. An active directory lock at
/// the same path is left untouched.
///
/// # Errors
///
/// Returns [`StoreError`] when a lock path cannot be inspected or when a
/// legacy file lock is present.
fn reject_legacy_file_locks(targets: &[PathBuf]) -> Result<(), StoreError> {
    let mut lock_paths: Vec<PathBuf> = targets.iter().map(|path| lock_path_for(path)).collect();
    lock_paths.sort();
    lock_paths.dedup();
    for lock_path in lock_paths {
        let metadata = match fs::symlink_metadata(&lock_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(StoreError::message(format!(
                    "Failed to inspect auth storage lock {}: {error}",
                    lock_path.display()
                )));
            }
        };
        if metadata.is_dir() {
            // Active directory lock: nothing legacy to reject.
            continue;
        }
        if metadata.is_file() {
            return Err(StoreError::message(format!(
                "Auth storage lock {} is a legacy file lock; remove it (or wait \
                 for any older process using it to exit) before retrying",
                lock_path.display()
            )));
        }
        return Err(StoreError::message(format!(
            "Auth storage lock {} is neither a file nor a directory",
            lock_path.display()
        )));
    }
    Ok(())
}

fn lock_error_to_store(error: &LockError) -> StoreError {
    StoreError::message(format!("Failed to acquire auth storage lock: {error}"))
}

fn parse_storage_data(content: Option<&str>) -> Result<BTreeMap<String, Credential>, StoreError> {
    let Some(content) = content else {
        return Ok(BTreeMap::new());
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(trimmed)
        .map_err(|error| StoreError::message(format!("Failed to parse auth storage JSON: {error}")))
}

fn serialize_storage_data(data: &BTreeMap<String, Credential>) -> Result<String, StoreError> {
    serde_json::to_string_pretty(data)
        .map_err(|error| StoreError::message(format!("Failed to serialize auth storage: {error}")))
}

fn resolve_credential_copy(credential: Credential) -> Credential {
    match credential {
        Credential::ApiKey(ApiKeyCredential { key, env }) => {
            let resolved_key = key
                .as_deref()
                .and_then(|raw| resolve_config_value(raw, env.as_ref()));
            Credential::ApiKey(ApiKeyCredential {
                key: resolved_key,
                env,
            })
        }
        other @ Credential::Oauth(_) => other,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::auth::types::{CredentialKind, OAuthCredential, ProviderEnv};

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            key: Some(key.into()),
            env: None,
        })
    }

    fn oauth(refresh: &str, access: &str, expires: i64) -> Credential {
        Credential::Oauth(OAuthCredential {
            refresh: refresh.into(),
            access: access.into(),
            expires,
            extra: BTreeMap::from([("accountId".into(), json!("acct"))]),
        })
    }

    fn oauth_with_extras() -> Credential {
        Credential::Oauth(OAuthCredential {
            refresh: "refresh-token".into(),
            access: "access-token".into(),
            expires: 1_750_000_000_000,
            extra: BTreeMap::from([
                ("accountId".into(), json!("acct-1")),
                ("enterpriseUrl".into(), json!("https://github.example")),
                ("availableModelIds".into(), json!(["gpt-4.1", "o4-mini"])),
            ]),
        })
    }

    fn temp_auth_path() -> TestResult<(TempDir, PathBuf)> {
        let dir = TempDir::new()?;
        let path = dir.path().join("agent").join("auth.json");
        Ok((dir, path))
    }

    /// Acquire a guard at the canonical lock target the backend resolves.
    ///
    /// `LockGuard::acquire(&path)` resolves the target with the unresolved
    /// absolute spelling, while `acquire_sync_lock`/`acquire_async_lock`
    /// resolve it with `canonical_lock_target`. Where those spellings diverge
    /// (a symlinked temp dir, e.g. macOS `/var` -> `/private/var`) the bare
    /// `acquire` would hold a *different* sibling directory than the backend
    /// creates, so the backend acquires unopposed and the test proves nothing.
    /// This helper resolves through `canonical_lock_target` first so the test
    /// guard and the backend always contend on the same lock directory.
    fn acquire_canonical_lock(path: &Path) -> TestResult<LockGuard> {
        let canonical = canonical_lock_target(path)?;
        Ok(LockGuard::acquire(&canonical)?)
    }

    async fn refresh_expired(
        current: Option<Credential>,
        refresh_calls: Arc<AtomicUsize>,
        delay: Duration,
    ) -> Result<Option<Credential>, AuthError> {
        let Some(Credential::Oauth(mut credential)) = current else {
            return Err(AuthError::message("expected OAuth credential"));
        };
        if credential.expires > 1_000 {
            return Ok(None);
        }
        tokio::time::sleep(delay).await;
        refresh_calls.fetch_add(1, Ordering::SeqCst);
        credential.refresh = "r1".into();
        credential.access = "a1".into();
        credential.expires = 9_000;
        Ok(Some(Credential::Oauth(credential)))
    }

    #[tokio::test]
    async fn concurrent_modify_serializes_refresh_callback() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store_a = FileCredentialStore::new(&path);
        store_a
            .modify(
                "openai-codex",
                Box::new(|_| Box::pin(async { Ok(Some(oauth("r0", "a0", 1))) })),
            )
            .await?;
        let store_b = FileCredentialStore::new(&path);
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let first_entered = Arc::new(tokio::sync::Notify::new());

        let calls_a = Arc::clone(&refresh_calls);
        let entered_a = Arc::clone(&first_entered);
        let task_a = tokio::spawn(async move {
            store_a
                .modify(
                    "openai-codex",
                    Box::new(move |current| {
                        entered_a.notify_one();
                        Box::pin(refresh_expired(current, calls_a, Duration::from_millis(80)))
                    }),
                )
                .await
        });
        first_entered.notified().await;

        let calls_b = Arc::clone(&refresh_calls);
        let task_b = tokio::spawn(async move {
            store_b
                .modify(
                    "openai-codex",
                    Box::new(move |current| {
                        Box::pin(refresh_expired(current, calls_b, Duration::ZERO))
                    }),
                )
                .await
        });

        let result_a = task_a.await??;
        let result_b = task_b.await??;
        for result in [&result_a, &result_b] {
            let Some(Credential::Oauth(credential)) = result else {
                return Err("expected refreshed OAuth result".into());
            };
            assert_eq!(credential.refresh, "r1");
            assert_eq!(credential.access, "a1");
            assert_eq!(credential.expires, 9_000);
        }

        let parsed: BTreeMap<String, Credential> =
            serde_json::from_str(&fs::read_to_string(&path)?)?;
        let Some(Credential::Oauth(credential)) = parsed.get("openai-codex") else {
            return Err("expected persisted OAuth credential".into());
        };
        assert_eq!(credential.refresh, "r1");
        assert_eq!(credential.extra.get("accountId"), Some(&json!("acct")));
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn modify_none_preserves_byte_identity() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "anthropic",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("sk-live"))) })),
            )
            .await?;
        let before = fs::read(&path)?;

        let returned = store
            .modify(
                "anthropic",
                Box::new(|current| {
                    Box::pin(async move {
                        assert_eq!(current, Some(api_key("sk-live")));
                        Ok(None)
                    })
                }),
            )
            .await?;
        assert_eq!(returned, Some(api_key("sk-live")));
        assert_eq!(before, fs::read(&path)?);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_json_surfaces_and_is_not_rewritten() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let Some(parent) = path.parent() else {
            return Err("auth path must have a parent".into());
        };
        fs::create_dir_all(parent)?;
        let garbage = b"{not-json";
        fs::write(&path, garbage)?;

        let store = FileCredentialStore::new(&path);
        let result = store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("sk"))) })),
            )
            .await;
        let Err(error) = result else {
            return Err("malformed JSON unexpectedly succeeded".into());
        };
        assert!(error.to_string().contains("JSON"));
        assert_eq!(fs::read(&path)?, garbage);
        assert!(read_stored_credential("openai", &path).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn oauth_extras_and_raw_api_key_roundtrip() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        let env = ProviderEnv::from([("CLOUDFLARE_ACCOUNT_ID".into(), "acct".into())]);
        store
            .modify(
                "openai-codex",
                Box::new(|_| Box::pin(async { Ok(Some(oauth_with_extras())) })),
            )
            .await?;
        store
            .modify(
                "openai",
                Box::new(move |_| {
                    Box::pin(async move {
                        Ok(Some(Credential::ApiKey(ApiKeyCredential {
                            key: Some("$OPENAI_API_KEY".into()),
                            env: Some(env),
                        })))
                    })
                }),
            )
            .await?;

        let parsed: BTreeMap<String, Credential> =
            serde_json::from_str(&fs::read_to_string(&path)?)?;
        let Some(Credential::Oauth(oauth)) = parsed.get("openai-codex") else {
            return Err("missing OAuth credential".into());
        };
        assert_eq!(oauth.extra.get("accountId"), Some(&json!("acct-1")));
        assert_eq!(
            oauth.extra.get("enterpriseUrl"),
            Some(&json!("https://github.example"))
        );
        assert_eq!(
            oauth.extra.get("availableModelIds"),
            Some(&json!(["gpt-4.1", "o4-mini"]))
        );
        let Some(Credential::ApiKey(api)) = parsed.get("openai") else {
            return Err("missing API-key credential".into());
        };
        assert_eq!(api.key.as_deref(), Some("$OPENAI_API_KEY"));
        assert_eq!(
            api.env
                .as_ref()
                .and_then(|env| env.get("CLOUDFLARE_ACCOUNT_ID"))
                .map(String::as_str),
            Some("acct")
        );
        assert!(matches!(
            read_stored_credential("openai", &path),
            Some(Credential::ApiKey(ApiKeyCredential { key: Some(key), .. }))
                if key == "$OPENAI_API_KEY"
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_modes_on_first_create_and_rewrite() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("one"))) })),
            )
            .await?;
        let Some(parent) = path.parent() else {
            return Err("auth path must have a parent".into());
        };
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(parent)?.permissions().mode() & 0o777, 0o700);

        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("two"))) })),
            )
            .await?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    async fn write_versions(
        store: FileCredentialStore,
        prefix: &'static str,
    ) -> Result<(), StoreError> {
        for index in 0..20 {
            store
                .modify(
                    "openai",
                    Box::new(move |_| {
                        Box::pin(async move { Ok(Some(api_key(&format!("{prefix}-{index}")))) })
                    }),
                )
                .await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn atomic_replace_keeps_parseable_old_or_new() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("v1"))) })),
            )
            .await?;
        let task_a = tokio::spawn(write_versions(FileCredentialStore::new(&path), "a"));
        let task_b = tokio::spawn(write_versions(FileCredentialStore::new(&path), "b"));
        task_a.await??;
        task_b.await??;

        let parsed: BTreeMap<String, Credential> =
            serde_json::from_str(&fs::read_to_string(&path)?)?;
        let Some(Credential::ApiKey(api)) = parsed.get("openai") else {
            return Err("expected final API-key credential".into());
        };
        let Some(key) = api.key.as_deref() else {
            return Err("expected final API key".into());
        };
        assert!(key.starts_with("a-") || key.starts_with("b-"));
        Ok(())
    }

    #[tokio::test]
    async fn delete_removes_provider_and_list_skips_command_resolution() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "openai",
                Box::new(|_| {
                    Box::pin(async {
                        Ok(Some(Credential::ApiKey(ApiKeyCredential {
                            key: Some("!echo should-not-run-on-list".into()),
                            env: None,
                        })))
                    })
                }),
            )
            .await?;
        assert_eq!(
            store.list().await?,
            vec![CredentialInfo {
                provider_id: "openai".into(),
                kind: CredentialKind::ApiKey,
            }]
        );

        store.delete("openai").await?;
        assert_eq!(store.read("openai").await?, None);
        assert!(store.list().await?.is_empty());
        assert!(read_stored_credential("openai", &path).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn missing_file_is_seeded_only_after_sibling_lock_is_acquired() -> TestResult {
        let (dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        let mut backend = FileLockBackend::new(&path);
        let contention = Arc::new(tokio::sync::Notify::new());
        backend.lock_contention = Some(Arc::clone(&contention));
        let guard = acquire_canonical_lock(&path)?;

        let task_backend = backend.clone();
        let task = tokio::spawn(async move {
            task_backend
                .with_lock_async(|content| async move { Ok::<_, StoreError>((content, None)) })
                .await
        });
        contention.notified().await;
        assert!(
            !path.exists(),
            "a waiter must not seed before acquiring the sibling lock"
        );

        let durable =
            serialize_storage_data(&BTreeMap::from([("openai".to_owned(), api_key("durable"))]))?;
        fs::write(&path, &durable)?;
        drop(guard);

        let observed = task.await??;
        assert_eq!(observed.as_deref(), Some(durable.as_str()));
        assert_eq!(fs::read_to_string(path)?, durable);
        drop(dir);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_file_lock_is_rejected_by_async_path() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        let backend = FileLockBackend::new(&path);
        let lock_path = backend.lock_path();
        fs::write(&lock_path, [])?;

        let result = backend
            .with_lock_async(|content| async move { Ok::<_, StoreError>((content, None)) })
            .await;
        let Err(error) = result else {
            return Err("legacy file lock was accepted by the async path".into());
        };
        assert!(
            error.to_string().contains("legacy file lock"),
            "rejection must name the legacy file lock: {error}"
        );
        assert!(
            lock_path.is_file(),
            "legacy file lock must remain intact (no unlink, no directory migration)"
        );
        Ok(())
    }

    #[test]
    fn legacy_file_lock_is_rejected_by_sync_path() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        let backend = FileLockBackend::new(&path);
        let lock_path = backend.lock_path();
        fs::write(&lock_path, [])?;

        // Rejection happens before the retry loop, so it must fail fast rather
        // than sleeping through the synchronous retry budget.
        let started = std::time::Instant::now();
        let result = backend.with_lock_sync(|content| Ok((content.map(str::to_owned), None)));
        let elapsed = started.elapsed();
        let Err(error) = result else {
            return Err("legacy file lock was accepted by the sync path".into());
        };
        assert!(
            error.to_string().contains("legacy file lock"),
            "rejection must name the legacy file lock: {error}"
        );
        assert!(lock_path.is_file(), "legacy file lock must remain intact");
        assert!(
            elapsed < Duration::from_millis(150),
            "legacy rejection must fail fast, elapsed={elapsed:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn legacy_file_lock_is_rejected_by_async_without_waiting() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        let backend = FileLockBackend::new(&path);
        let lock_path = backend.lock_path();
        fs::write(&lock_path, [])?;

        let started = std::time::Instant::now();
        let result = backend
            .with_lock_async(|content| async move { Ok::<_, StoreError>((content, None)) })
            .await;
        let elapsed = started.elapsed();
        let Err(error) = result else {
            return Err("legacy file lock was accepted by the async path".into());
        };
        assert!(
            error.to_string().contains("legacy file lock"),
            "rejection must name the legacy file lock: {error}"
        );
        assert!(lock_path.is_file(), "legacy file lock must remain intact");
        assert!(
            elapsed < Duration::from_millis(500),
            "legacy rejection must fail fast, elapsed={elapsed:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sync_and_async_lock_share_one_canonical_path_through_symlink() -> TestResult {
        use std::os::unix::fs::symlink;
        use tokio::time::timeout;

        let real = TempDir::new()?;
        let real_agent = real.path().join("agent");
        fs::create_dir_all(&real_agent)?;
        // Expose the agent directory through a symlink so the auth path crosses
        // a link component: the condition under which unresolved and
        // canonicalized spellings diverge and would split ownership if the two
        // acquirers used different resolution functions.
        let link_root = TempDir::new()?;
        let linked = link_root.path().join("agent-link");
        symlink(&real_agent, &linked)?;
        let path = linked.join("auth.json");

        // The unresolved and canonical spellings must differ (otherwise there
        // is nothing to unify). Both acquirers must lock the canonical one.
        let unresolved = absolute_lock_target(&path)?;
        let canonical = canonical_lock_target(&path)?;
        assert_ne!(unresolved, canonical, "symlink must make spellings diverge");

        // Hold the canonical sibling directory so both acquirers must contend on
        // it. If either acquirer used the unresolved spelling it would create a
        // different `${path}.lock` and proceed unopposed.
        let shared_lock = lock_path_for(&canonical);
        fs::create_dir(&shared_lock)?;

        // The synchronous path must lock the canonical path, not an unresolved
        // alias that would let it proceed past a held sibling.
        let sync_backend = FileLockBackend::new(&path);
        let sync_result =
            sync_backend.with_lock_sync(|content| Ok((content.map(str::to_owned), None)));
        assert!(
            sync_result.is_err(),
            "sync must contend on the shared canonical lock path"
        );

        // The async path must contend on the same canonical directory rather
        // than locking a separately-resolved path and proceeding unopposed.
        let mut async_backend = FileLockBackend::new(&path);
        let contention = Arc::new(tokio::sync::Notify::new());
        async_backend.lock_contention = Some(Arc::clone(&contention));
        let task = tokio::spawn(async move {
            async_backend
                .with_lock_async(|content| async move { Ok::<_, StoreError>((content, None)) })
                .await
        });
        if timeout(Duration::from_secs(2), contention.notified())
            .await
            .is_err()
        {
            return Err("async did not contend on the shared canonical lock path".into());
        }

        // Release the held directory so the async acquirer can finish cleanly.
        fs::remove_dir(&shared_lock)?;
        task.await??;
        Ok(())
    }

    #[test]
    fn sync_lock_exhaustion_names_lock_path_and_attempt_count() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        // Hold the lock with a live guard so every acquisition attempt reports
        // contention and the sync budget (SYNC_LOCK_ATTEMPTS) is exhausted.
        let _guard = acquire_canonical_lock(&path)?;

        let backend = FileLockBackend::new(&path);
        let result = backend.with_lock_sync(|content| Ok((content.map(str::to_owned), None)));
        let Err(error) = result else {
            return Err("sync acquisition unexpectedly succeeded under a held lock".into());
        };
        let message = error.to_string();
        let lock_dir = lock_path_for(&canonical_lock_target(&path)?);
        assert!(
            message.contains(&lock_dir.display().to_string()),
            "exhaustion error must name the wedged lock directory {lock_dir:?}: {message}"
        );
        assert!(
            message.contains(&format!("{SYNC_LOCK_ATTEMPTS} attempts")),
            "exhaustion error must state the sync attempt count: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn async_lock_exhaustion_names_lock_path_and_retry_count() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        fs::create_dir_all(path.parent().ok_or("auth path must have a parent")?)?;
        let _guard = acquire_canonical_lock(&path)?;

        let mut backend = FileLockBackend::new(&path);
        backend.async_lock_retry_delay = Some(Duration::from_millis(1));
        let result = backend
            .with_lock_async(|content| async move { Ok::<_, StoreError>((content, None)) })
            .await;
        let Err(error) = result else {
            return Err("async acquisition unexpectedly succeeded under a held lock".into());
        };
        let message = error.to_string();
        let lock_dir = lock_path_for(&canonical_lock_target(&path)?);
        assert!(
            message.contains(&lock_dir.display().to_string()),
            "exhaustion error must name the wedged lock directory {lock_dir:?}: {message}"
        );
        assert!(
            message.contains(&format!("{ASYNC_LOCK_RETRIES} retries")),
            "exhaustion error must state the async retry count: {message}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_modify_does_not_publish_unpersisted_cache() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("old"))) })),
            )
            .await?;
        let durable = fs::read_to_string(&path)?;

        store
            .backend
            .fail_next_atomic_write(std::io::ErrorKind::PermissionDenied);
        let result = store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("new"))) })),
            )
            .await;
        assert!(result.is_err(), "injected persistence failure must surface");
        assert_eq!(fs::read_to_string(&path)?, durable);
        assert_eq!(store.read("openai").await?, Some(api_key("old")));
        Ok(())
    }

    #[tokio::test]
    async fn failed_delete_does_not_evict_persisted_cache() -> TestResult {
        let (_dir, path) = temp_auth_path()?;
        let store = FileCredentialStore::new(&path);
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("old"))) })),
            )
            .await?;
        let durable = fs::read_to_string(&path)?;

        store
            .backend
            .fail_next_atomic_write(std::io::ErrorKind::PermissionDenied);
        let result = store.delete("openai").await;
        assert!(result.is_err(), "injected persistence failure must surface");
        assert_eq!(fs::read_to_string(&path)?, durable);
        assert_eq!(store.read("openai").await?, Some(api_key("old")));
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn atomic_write_preserves_symlink_leaf() -> TestResult {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let real_path = dir.path().join("real.json");
        fs::write(&real_path, "{}")?;
        let link_path = dir.path().join("link.json");
        symlink(&real_path, &link_path)?;
        let store = FileCredentialStore::new(&link_path);
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("leaf-test"))) })),
            )
            .await?;
        let meta = fs::symlink_metadata(&link_path)?;
        assert!(
            meta.file_type().is_symlink(),
            "symlink leaf should be preserved after atomic_write, got {:?}",
            meta.file_type()
        );
        let real_content = fs::read_to_string(&real_path)?;
        assert!(
            real_content.contains("leaf-test"),
            "real file should contain new credential, got {real_content:?}"
        );
        let via_link = fs::read_to_string(&link_path)?;
        assert_eq!(
            real_content, via_link,
            "reading through symlink should see same content"
        );
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn atomic_write_preserves_dangling_symlink_leaf() -> TestResult {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let link_path = dir.path().join("link.json");
        // Relative dangling symlink: link -> "real.json" in same dir, target does not exist yet.
        symlink("real.json", &link_path)?;
        assert!(
            fs::symlink_metadata(&link_path).is_ok_and(|m| m.file_type().is_symlink()),
            "link should be a symlink"
        );
        assert!(
            !link_path.exists(),
            "dangling symlink should report !exists()"
        );
        let real_path = dir.path().join("real.json");
        assert!(!real_path.exists(), "target should not exist before write");
        let store = FileCredentialStore::new(&link_path);
        // Effective path must resolve to the symlink target, not the link itself.
        assert_eq!(
            store.backend.effective_path,
            fs::canonicalize(dir.path())?.join("real.json"),
            "effective_path should resolve dangling symlink to its target"
        );
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("dangling-test"))) })),
            )
            .await?;
        let meta = fs::symlink_metadata(&link_path)?;
        assert!(
            meta.file_type().is_symlink(),
            "dangling symlink leaf should be preserved after atomic_write, got {:?}",
            meta.file_type()
        );
        assert!(real_path.exists(), "target should be created after write");
        let real_content = fs::read_to_string(&real_path)?;
        assert!(
            real_content.contains("dangling-test"),
            "real file should contain new credential, got {real_content:?}"
        );
        let via_link = fs::read_to_string(&link_path)?;
        assert_eq!(
            real_content, via_link,
            "reading through symlink should see same content"
        );
        // Lock path must be at the target, not the link.
        assert_eq!(
            store.backend.lock_path(),
            store.backend.effective_path.with_extension("json.lock"),
            "lock should be sibling of effective_path"
        );
        Ok(())
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn atomic_write_preserves_symlink_chain() -> TestResult {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let real_path = dir.path().join("real.json");
        let second_path = dir.path().join("second.json");
        let link_path = dir.path().join("link.json");
        // Chain: link -> second -> real.json, all dangling initially.
        symlink("real.json", &second_path)?;
        symlink("second.json", &link_path)?;
        assert!(!link_path.exists());
        assert!(!second_path.exists());
        assert!(!real_path.exists());
        let store = FileCredentialStore::new(&link_path);
        assert_eq!(
            store.backend.effective_path,
            fs::canonicalize(dir.path())?.join("real.json"),
            "effective_path should resolve full chain to real.json"
        );
        store
            .modify(
                "openai",
                Box::new(|_| Box::pin(async { Ok(Some(api_key("chain-test"))) })),
            )
            .await?;
        for path in [&link_path, &second_path] {
            let meta = fs::symlink_metadata(path)?;
            assert!(
                meta.file_type().is_symlink(),
                "chain symlink {} should remain symlink, got {:?}",
                path.display(),
                meta.file_type()
            );
        }
        assert!(real_path.exists(), "real target should be created");
        let real_content = fs::read_to_string(&real_path)?;
        assert!(
            real_content.contains("chain-test"),
            "real file should contain chain-test, got {real_content:?}"
        );
        assert_eq!(
            fs::read_to_string(&link_path)?,
            real_content,
            "read through link should match real"
        );
        assert_eq!(
            fs::read_to_string(&second_path)?,
            real_content,
            "read through second should match real"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn canonical_lock_target_rejects_symlink_cycle() -> TestResult {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir()?;
        let a_path = dir.path().join("a.json");
        let b_path = dir.path().join("b.json");
        symlink("b.json", &a_path)?;
        symlink("a.json", &b_path)?;
        let err = canonical_lock_target(&a_path).expect_err("cycle should be rejected");
        assert!(
            err.to_string().contains("ELOOP"),
            "cycle error should mention ELOOP, got {err:?}"
        );
        // Self-loop
        let self_path = dir.path().join("self.json");
        symlink("self.json", &self_path)?;
        let err = canonical_lock_target(&self_path).expect_err("self-loop should be rejected");
        assert!(
            err.to_string().contains("ELOOP"),
            "self-loop error should mention ELOOP, got {err:?}"
        );
        Ok(())
    }
}
