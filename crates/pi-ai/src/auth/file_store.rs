//! Whole-file locked credential storage for `auth.json`.
//!
//! [`FileLockBackend`] serializes multi-process readers/writers with an `fs4`
//! exclusive lock held across the full read-modify-write critical section,
//! including async modify callbacks (for example OAuth refresh). The data file
//! is replaced atomically via a same-directory [`tempfile::NamedTempFile`].

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fs4::{FileExt, TryLockError};
use futures::future::BoxFuture;
use tempfile::Builder;

use super::config_value::resolve_config_value;
use super::error::{AuthError, StoreError};
use super::types::{ApiKeyCredential, Credential, CredentialInfo, CredentialStore};

/// Pretty-printed empty credential map written when seeding a missing file.
const EMPTY_JSON: &str = "{}";

/// Maximum exclusive-lock acquisition attempts (mirrors proper-lockfile retries).
const LOCK_ATTEMPTS: u32 = 10;

/// Initial async backoff between lock attempts.
const LOCK_MIN_DELAY: Duration = Duration::from_millis(100);

/// Cap for exponential lock-retry backoff.
const LOCK_MAX_DELAY: Duration = Duration::from_secs(10);

/// Shared file-backed JSON map with exclusive locking and atomic replace.
///
/// The lock is taken on a sibling `${path}.lock` file so the data file can be
/// replaced with `rename` without releasing multi-process mutual exclusion.
#[derive(Clone, Debug)]
pub struct FileLockBackend {
    path: PathBuf,
    #[cfg(test)]
    lock_contention: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    atomic_write_failure: Arc<Mutex<Option<std::io::ErrorKind>>>,
}

impl FileLockBackend {
    /// Create a backend for the given JSON data path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(test)]
            lock_contention: None,
            #[cfg(test)]
            atomic_write_failure: Arc::new(Mutex::new(None)),
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
        lock_path_for(&self.path)
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
        let lock_file = self.open_lock_file()?;
        acquire_lock_sync(&lock_file)?;
        let _guard = LockGuard { file: &lock_file };
        self.check_lock_identity(&lock_file)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next) = f(current.as_deref())?;
        self.check_lock_identity(&lock_file)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            self.check_lock_identity(&lock_file)?;
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
        let lock_file = self.open_lock_file()?;
        acquire_lock_sync(&lock_file)?;
        let _guard = LockGuard { file: &lock_file };
        self.check_lock_identity(&lock_file)?;
        let current = self.read_data_file()?;
        let (result, next) = f(current.as_deref())?;
        self.check_lock_identity(&lock_file)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            self.check_lock_identity(&lock_file)?;
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
        let lock_file = self.open_lock_file()?;
        acquire_lock_async(&lock_file, || {
            #[cfg(test)]
            self.notify_lock_contention();
        })
        .await?;
        let _guard = LockGuard { file: &lock_file };
        self.check_lock_identity(&lock_file)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next) = f(current).await?;
        self.check_lock_identity(&lock_file)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            self.check_lock_identity(&lock_file)?;
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
        let lock_file = self.open_lock_file()?;
        acquire_lock_async(&lock_file, || {
            #[cfg(test)]
            self.notify_lock_contention();
        })
        .await?;
        let _guard = LockGuard { file: &lock_file };
        self.check_lock_identity(&lock_file)?;
        self.ensure_data_file()?;
        let current = self.read_data_file()?;
        let (result, next, committed) = f(current).await?;
        self.check_lock_identity(&lock_file)?;
        if let Some(next) = next {
            self.atomic_write(&next)?;
            self.check_lock_identity(&lock_file)?;
        }
        commit(committed)?;
        Ok(result)
    }

    fn ensure_parent_dir(&self) -> Result<(), StoreError> {
        let Some(parent) = parent_dir(&self.path) else {
            return Ok(());
        };
        if !parent.exists() {
            create_dir_all_secure(parent)?;
        }
        set_owner_dir_mode(parent)
    }

    fn ensure_data_file(&self) -> Result<(), StoreError> {
        if self.path.exists() {
            return Ok(());
        }
        // Callers hold the sibling lock here, so the missing-path recheck and
        // seed commit are ordered with every normal read and write.
        self.atomic_write(EMPTY_JSON)
    }

    fn open_lock_file(&self) -> Result<File, StoreError> {
        let lock_path = self.lock_path();
        if let Some(parent) = parent_dir(&lock_path)
            && !parent.exists()
        {
            create_dir_all_secure(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                StoreError::message(format!(
                    "Failed to open auth storage lock {}: {error}",
                    lock_path.display()
                ))
            })?;
        set_owner_secret_mode(&lock_path)?;
        Ok(file)
    }

    fn read_data_file(&self) -> Result<Option<String>, StoreError> {
        match fs::read_to_string(&self.path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(StoreError::message(format!(
                "Failed to read auth storage {}: {error}",
                self.path.display()
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

        let parent = parent_dir(&self.path).unwrap_or_else(|| Path::new("."));
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
        tmp.persist(&self.path).map_err(|error| {
            StoreError::message(format!(
                "Failed to persist auth storage from {} to {}: {}",
                tmp_path.display(),
                self.path.display(),
                error.error
            ))
        })?;

        set_owner_secret_mode(&self.path)?;
        sync_parent_dir(parent);
        Ok(())
    }

    fn check_lock_identity(&self, lock_file: &File) -> Result<(), StoreError> {
        let path = self.lock_path();
        let Ok(path_meta) = fs::metadata(&path) else {
            return Err(StoreError::message("Auth storage lock was compromised"));
        };
        let handle_meta = lock_file
            .metadata()
            .map_err(|_| StoreError::message("Auth storage lock was compromised"))?;
        if !same_file_identity(&path_meta, &handle_meta) {
            return Err(StoreError::message("Auth storage lock was compromised"));
        }
        Ok(())
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

fn same_file_identity(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
            && a.modified().ok() == b.modified().ok()
            && a.created().ok() == b.created().ok()
    }
}

fn acquire_lock_sync(file: &File) -> Result<(), StoreError> {
    for attempt in 1..=LOCK_ATTEMPTS {
        match FileExt::try_lock(file) {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if attempt < LOCK_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::message("Failed to acquire auth storage lock"));
            }
            Err(TryLockError::Error(error)) => {
                return Err(StoreError::message(format!(
                    "Failed to acquire auth storage lock: {error}"
                )));
            }
        }
    }
    Err(StoreError::message("Failed to acquire auth storage lock"))
}

async fn acquire_lock_async(
    file: &File,
    mut on_contention: impl FnMut(),
) -> Result<(), StoreError> {
    let mut delay = LOCK_MIN_DELAY;
    for attempt in 1..=LOCK_ATTEMPTS {
        match FileExt::try_lock(file) {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if attempt < LOCK_ATTEMPTS => {
                on_contention();
                // Light jitter from attempt count keeps retries desynchronized
                // without introducing an extra RNG dependency here.
                let jitter = Duration::from_millis(u64::from(attempt.saturating_mul(7) % 37));
                tokio::time::sleep(delay.saturating_add(jitter)).await;
                delay = delay.saturating_mul(2).min(LOCK_MAX_DELAY);
            }
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::message("Failed to acquire auth storage lock"));
            }
            Err(TryLockError::Error(error)) => {
                return Err(StoreError::message(format!(
                    "Failed to acquire auth storage lock: {error}"
                )));
            }
        }
    }
    Err(StoreError::message("Failed to acquire auth storage lock"))
}

struct LockGuard<'a> {
    file: &'a File,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.file);
    }
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
        let lock_path = backend.lock_path();
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        FileExt::lock(&lock_file)?;

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
        FileExt::unlock(&lock_file)?;

        let observed = task.await??;
        assert_eq!(observed.as_deref(), Some(durable.as_str()));
        assert_eq!(fs::read_to_string(path)?, durable);
        drop(dir);
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
}
