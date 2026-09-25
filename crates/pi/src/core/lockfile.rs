//! Ownership-safe directory locks for durable native workflows.
//!
//! The lock layout follows `proper-lockfile`: a lock for `path` is an
//! atomically-created directory at `path.lock`, and its directory mtime is
//! refreshed while the owner is alive.  This implementation adds an explicit
//! owner marker and identity checks around every removal.  A stale contender
//! first verifies the same directory inode/mtime twice, atomically quarantines
//! that directory, and only then removes its known contents; it never removes
//! a path that may already belong to a newer owner.

use std::fmt;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const MIN_STALE_MS: u64 = 2_000;
const MIN_UPDATE_MS: u64 = 1_000;
const OWNER_MARKER_NAME: &str = ".pi-owner";

/// Acquisition and heartbeat policy for [`lock`].
#[derive(Clone, Debug)]
pub struct LockOptions {
    /// Age after which an unrefreshed lock may be reclaimed.
    pub stale_ms: u64,
    /// Heartbeat interval. Zero selects half of the normalized stale period;
    /// values are clamped to at least one second and at most stale/2.
    pub update_ms: u64,
    /// Number of retries after the initial acquisition attempt.
    pub retries: u32,
    /// Delay between acquisition retries.
    pub retry_ms: u64,
    /// Optional wall-clock bound for all retries. Zero means no time bound.
    pub max_retry_ms: u64,
    /// Resolve the target through the filesystem before deriving `.lock`.
    pub realpath: bool,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            stale_ms: 10_000,
            update_ms: 5_000,
            retries: 0,
            retry_ms: 0,
            max_retry_ms: 0,
            realpath: true,
        }
    }
}

impl LockOptions {
    fn normalized(&self) -> NormalizedLockOptions {
        let stale_ms = self.stale_ms.max(MIN_STALE_MS);
        let requested_update = if self.update_ms == 0 {
            stale_ms / 2
        } else {
            self.update_ms
        };
        let update_ms = requested_update.clamp(MIN_UPDATE_MS, stale_ms / 2);
        NormalizedLockOptions {
            stale_ms,
            update_ms,
            retries: self.retries,
            retry_ms: self.retry_ms,
            max_retry_ms: self.max_retry_ms,
            realpath: self.realpath,
        }
    }
}

#[derive(Clone, Copy)]
struct NormalizedLockOptions {
    stale_ms: u64,
    update_ms: u64,
    retries: u32,
    retry_ms: u64,
    max_retry_ms: u64,
    realpath: bool,
}

/// A successfully acquired lock.
///
/// Call [`LockRelease::release`] to stop the heartbeat and remove the lock.
/// Dropping this value stops the heartbeat but deliberately does not remove a
/// lock synchronously; the directory remains for stale-owner detection rather
/// than risking a blocking or identity-unsafe destructor cleanup.
pub struct LockRelease {
    lock_path: PathBuf,
    identity: LockIdentity,
    owner_token: String,
    state: Arc<LockState>,
    heartbeat: Option<JoinHandle<()>>,
}

impl fmt::Debug for LockRelease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LockRelease")
            .field("lock_path", &self.lock_path)
            .field("released", &self.state.released.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl LockRelease {
    /// Stops heartbeats and removes this lock only while ownership still
    /// matches the acquired directory identity and owner marker.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the lock was compromised, its identity no
    /// longer matches, or its contents could not be removed safely.  A lock
    /// already removed by another actor is treated as an idempotent release.
    pub async fn release(mut self) -> io::Result<()> {
        self.state.released.store(true, Ordering::Release);
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
            let _ = heartbeat.await;
        }
        if let Some(reason) = self.state.compromised() {
            return Err(io::Error::other(reason));
        }
        remove_owned_lock(&self.lock_path, self.identity, &self.owner_token).await
    }
}

impl Drop for LockRelease {
    fn drop(&mut self) {
        self.state.released.store(true, Ordering::Release);
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
    }
}

/// Acquires a lock using an atomic directory create.
///
/// The target is canonicalized when `options.realpath` is true, matching
/// `proper-lockfile`'s requirement that a realpath target already exists.  A
/// false `realpath` option still resolves the path lexically against the
/// current directory, so equivalent relative spellings share one lock name.
///
/// # Errors
///
/// Returns an `AlreadyExists` error when the lock remains held after the
/// configured retries, the target cannot be resolved, or an underlying
/// filesystem operation fails.  Stale lock removal and ownership failures are
/// returned explicitly instead of silently deleting another owner's lock.
pub async fn lock(path: &Path, options: LockOptions) -> io::Result<LockRelease> {
    let options = options.normalized();
    let target = resolve_target(path, options.realpath).await?;
    let lock_path = append_lock_suffix(&target);
    let started = Instant::now();
    let mut retries = 0_u32;

    loop {
        match acquire_once(&lock_path, options.update_ms).await {
            Ok(release) => return Ok(release),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                // A stale lock is reclaimable even when retries == 0.  This is
                // the same distinction proper-lockfile makes between stale
                // cleanup and a retry against a live owner.
                if remove_stale_lock(&lock_path, options.stale_ms).await? {
                    continue;
                }
                let elapsed = duration_millis(started.elapsed());
                if retries >= options.retries
                    || (options.max_retry_ms > 0 && elapsed >= options.max_retry_ms)
                {
                    return Err(error);
                }
                retries = retries.saturating_add(1);
                if options.retry_ms > 0 {
                    let delay = if options.max_retry_ms == 0 {
                        options.retry_ms
                    } else {
                        let elapsed = duration_millis(started.elapsed());
                        let remaining = options.max_retry_ms.saturating_sub(elapsed);
                        if remaining == 0 {
                            return Err(error);
                        }
                        options.retry_ms.min(remaining)
                    };
                    sleep(Duration::from_millis(delay)).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn resolve_target(path: &Path, realpath: bool) -> io::Result<PathBuf> {
    if realpath {
        tokio::fs::canonicalize(path).await
    } else {
        crate::core::tools::path_utils::resolve_lexically_absolute(path)
    }
}

fn append_lock_suffix(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

async fn acquire_once(lock_path: &Path, update_ms: u64) -> io::Result<LockRelease> {
    match tokio::fs::create_dir(lock_path).await {
        Ok(()) => {}
        Err(error) => return Err(error),
    }

    let owner_token = Uuid::new_v4().to_string();
    let marker_path = lock_path.join(OWNER_MARKER_NAME);
    if let Err(error) = write_owner_marker(&marker_path, &owner_token).await {
        let _ = tokio::fs::remove_file(&marker_path).await;
        let _ = tokio::fs::remove_dir(lock_path).await;
        return Err(error);
    }

    let metadata = match tokio::fs::symlink_metadata(lock_path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = remove_owned_contents(lock_path, Some(&owner_token)).await;
            return Err(error);
        }
    };
    if !metadata.file_type().is_dir() {
        let _ = remove_owned_contents(lock_path, Some(&owner_token)).await;
        return Err(other_error(format!(
            "lock path is not a directory: {}",
            lock_path.display()
        )));
    }
    let identity = lock_identity(&metadata);
    let expected_mtime = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            let _ = remove_owned_contents(lock_path, Some(&owner_token)).await;
            return Err(error);
        }
    };
    let state = Arc::new(LockState::new(expected_mtime));
    let heartbeat = Some(spawn_heartbeat(
        lock_path.to_path_buf(),
        identity,
        owner_token.clone(),
        Arc::clone(&state),
        update_ms,
    ));
    Ok(LockRelease {
        lock_path: lock_path.to_path_buf(),
        identity,
        owner_token,
        state,
        heartbeat,
    })
}

async fn write_owner_marker(path: &Path, token: &str) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(token.as_bytes()).await?;
    file.flush().await
}

async fn remove_stale_lock(lock_path: &Path, stale_ms: u64) -> io::Result<bool> {
    let first = match tokio::fs::symlink_metadata(lock_path).await {
        Ok(metadata) => lock_snapshot(&metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !first.is_directory || !is_stale(first.modified, stale_ms) {
        return Ok(false);
    }

    // Re-check both identity and mtime immediately before quarantine.  A
    // heartbeat that raced the first stat therefore prevents reclamation.
    let second = match tokio::fs::symlink_metadata(lock_path).await {
        Ok(metadata) => lock_snapshot(&metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if first.identity != second.identity || first.modified != second.modified {
        return Ok(false);
    }
    if !is_stale(second.modified, stale_ms) {
        return Ok(false);
    }

    let quarantine = stale_quarantine_path(lock_path);
    match rename_noreplace(lock_path, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    }
    let moved = match tokio::fs::symlink_metadata(&quarantine).await {
        Ok(metadata) => match lock_snapshot(&metadata) {
            Ok(snapshot) => snapshot,
            Err(error) => return restore_quarantine(&quarantine, lock_path, &error).await,
        },
        Err(error) => return restore_quarantine(&quarantine, lock_path, &error).await,
    };
    if moved.identity != first.identity || moved.modified != first.modified {
        let error = other_error(format!(
            "lock identity changed while reclaiming {}",
            lock_path.display()
        ));
        return restore_quarantine(&quarantine, lock_path, &error).await;
    }
    if let Err(error) = remove_owned_contents(&quarantine, None).await {
        return restore_quarantine(&quarantine, lock_path, &error).await;
    }
    Ok(true)
}

fn stale_quarantine_path(lock_path: &Path) -> PathBuf {
    let mut value = lock_path.as_os_str().to_os_string();
    value.push(".stale-");
    value.push(Uuid::new_v4().simple().to_string());
    PathBuf::from(value)
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    atomic_rename_noreplace(from, to)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
))]
fn atomic_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(windows)]
fn atomic_rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on Windows",
    ))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox",
    windows
)))]
fn atomic_rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this target",
    ))
}
/// Restores a quarantined lock after a post-rename identity check fails.
///
/// The no-replace rename leaves a newer owner untouched if it has already
/// installed a lock at the canonical path.
async fn restore_quarantine(
    quarantine: &Path,
    lock_path: &Path,
    cause: &io::Error,
) -> io::Result<()> {
    match tokio::fs::symlink_metadata(lock_path).await {
        Ok(_) => {
            return Err(other_error(format!(
                "{cause}; refusing to overwrite a newer lock at {}",
                lock_path.display()
            )));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(other_error(format!(
                "{cause}; failed to inspect lock restoration path {}: {error}",
                lock_path.display()
            )));
        }
    }
    rename_noreplace(quarantine, lock_path).map_err(|error| {
        other_error(format!(
            "{cause}; failed to restore lock at {}: {error}",
            lock_path.display()
        ))
    })
}

async fn remove_owned_lock(
    lock_path: &Path,
    identity: LockIdentity,
    owner_token: &str,
) -> io::Result<()> {
    let metadata = match tokio::fs::symlink_metadata(lock_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let current = lock_snapshot(&metadata)?;
    if !current.is_directory || current.identity != identity {
        return Err(other_error(format!(
            "lock ownership changed before release: {}",
            lock_path.display()
        )));
    }
    if !owner_marker_matches(lock_path, owner_token).await? {
        return Err(other_error(format!(
            "lock owner marker changed before release: {}",
            lock_path.display()
        )));
    }

    let quarantine = stale_quarantine_path(lock_path);
    match rename_noreplace(lock_path, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let moved = match tokio::fs::symlink_metadata(&quarantine).await {
        Ok(metadata) => match lock_snapshot(&metadata) {
            Ok(snapshot) => snapshot,
            Err(error) => return restore_quarantine(&quarantine, lock_path, &error).await,
        },
        Err(error) => return restore_quarantine(&quarantine, lock_path, &error).await,
    };
    if !moved.is_directory || moved.identity != identity {
        let error = other_error(format!(
            "lock identity changed while releasing {}",
            lock_path.display()
        ));
        return restore_quarantine(&quarantine, lock_path, &error).await;
    }
    match owner_marker_matches(&quarantine, owner_token).await {
        Ok(true) => {}
        Ok(false) => {
            let error = other_error(format!(
                "lock owner marker changed while releasing {}",
                lock_path.display()
            ));
            return restore_quarantine(&quarantine, lock_path, &error).await;
        }
        Err(error) => return restore_quarantine(&quarantine, lock_path, &error).await,
    }
    if let Err(error) = remove_owned_contents(&quarantine, Some(owner_token)).await {
        return restore_quarantine(&quarantine, lock_path, &error).await;
    }
    Ok(())
}

async fn remove_owned_contents(path: &Path, expected_owner: Option<&str>) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut marker_path = None;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name().as_os_str() != std::ffi::OsStr::new(OWNER_MARKER_NAME) {
            return Err(other_error(format!(
                "refusing to remove lock directory with unexpected entry: {}",
                entry.path().display()
            )));
        }
        let candidate = entry.path();
        let metadata = tokio::fs::symlink_metadata(&candidate).await?;
        if !metadata.file_type().is_file() {
            return Err(other_error(format!(
                "refusing to remove non-file lock owner marker: {}",
                candidate.display()
            )));
        }
        if let Some(expected_owner) = expected_owner {
            let bytes = tokio::fs::read(&candidate).await?;
            if bytes != expected_owner.as_bytes() {
                return Err(other_error(format!(
                    "lock owner marker does not match: {}",
                    candidate.display()
                )));
            }
        }
        marker_path = Some(candidate);
    }
    if expected_owner.is_some() && marker_path.is_none() {
        return Err(other_error(format!(
            "lock owner marker is missing: {}",
            path.display()
        )));
    }
    if let Some(marker_path) = marker_path {
        tokio::fs::remove_file(marker_path).await?;
    }
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn owner_marker_matches(path: &Path, expected: &str) -> io::Result<bool> {
    let marker = path.join(OWNER_MARKER_NAME);
    let metadata = match tokio::fs::symlink_metadata(&marker).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    Ok(tokio::fs::read(marker).await? == expected.as_bytes())
}

fn spawn_heartbeat(
    lock_path: PathBuf,
    identity: LockIdentity,
    owner_token: String,
    state: Arc<LockState>,
    update_ms: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_millis(update_ms)).await;
            if state.released.load(Ordering::Acquire) {
                return;
            }

            let metadata = match tokio::fs::symlink_metadata(&lock_path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    state.compromise(format!("failed to stat lock heartbeat: {error}"));
                    return;
                }
            };
            if !metadata.file_type().is_dir() || lock_identity(&metadata) != identity {
                state.compromise(format!(
                    "lock identity changed during heartbeat: {}",
                    lock_path.display()
                ));
                return;
            }
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(error) => {
                    state.compromise(format!("failed to read lock mtime: {error}"));
                    return;
                }
            };
            if modified != state.expected_mtime() {
                state.compromise(format!(
                    "lock mtime changed outside heartbeat: {}",
                    lock_path.display()
                ));
                return;
            }
            match owner_marker_matches(&lock_path, &owner_token).await {
                Ok(true) => {}
                Ok(false) => {
                    state.compromise(format!(
                        "lock owner marker changed during heartbeat: {}",
                        lock_path.display()
                    ));
                    return;
                }
                Err(error) => {
                    state.compromise(format!("failed to read lock owner marker: {error}"));
                    return;
                }
            }

            let next_mtime = match touch_mtime(&lock_path, identity) {
                Ok(mtime) => mtime,
                Err(error) => {
                    state.compromise(format!("failed to update lock heartbeat: {error}"));
                    return;
                }
            };
            let after = match tokio::fs::symlink_metadata(&lock_path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    state.compromise(format!("failed to verify lock heartbeat: {error}"));
                    return;
                }
            };
            if !after.file_type().is_dir() || lock_identity(&after) != identity {
                state.compromise(format!(
                    "lock identity changed after heartbeat: {}",
                    lock_path.display()
                ));
                return;
            }
            match owner_marker_matches(&lock_path, &owner_token).await {
                Ok(true) => state.set_expected_mtime(next_mtime),
                Ok(false) => {
                    state.compromise(format!(
                        "lock owner marker changed after heartbeat: {}",
                        lock_path.display()
                    ));
                    return;
                }
                Err(error) => {
                    state.compromise(format!("failed to verify lock owner marker: {error}"));
                    return;
                }
            }
        }
    })
}

fn touch_mtime(path: &Path, expected_identity: LockIdentity) -> io::Result<SystemTime> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_dir() || lock_identity(&metadata) != expected_identity {
        return Err(other_error(format!(
            "lock identity changed before heartbeat update: {}",
            path.display()
        )));
    }
    file.set_modified(SystemTime::now())?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_dir() || lock_identity(&metadata) != expected_identity {
        return Err(other_error(format!(
            "lock identity changed after heartbeat update: {}",
            path.display()
        )));
    }
    metadata.modified()
}

fn is_stale(modified: SystemTime, stale_ms: u64) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > Duration::from_millis(stale_ms))
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LockIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    length: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LockSnapshot {
    identity: LockIdentity,
    modified: SystemTime,
    is_directory: bool,
}

fn lock_identity(metadata: &std::fs::Metadata) -> LockIdentity {
    #[cfg(unix)]
    {
        LockIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        LockIdentity {
            length: metadata.len(),
        }
    }
}

fn lock_snapshot(metadata: &std::fs::Metadata) -> io::Result<LockSnapshot> {
    Ok(LockSnapshot {
        identity: lock_identity(metadata),
        modified: metadata.modified()?,
        is_directory: metadata.file_type().is_dir(),
    })
}

struct LockState {
    released: AtomicBool,
    compromised: Mutex<Option<String>>,
    expected_mtime: Mutex<SystemTime>,
}

impl LockState {
    fn new(expected_mtime: SystemTime) -> Self {
        Self {
            released: AtomicBool::new(false),
            compromised: Mutex::new(None),
            expected_mtime: Mutex::new(expected_mtime),
        }
    }

    fn compromise(&self, reason: String) {
        let mut compromised = lock_mutex(&self.compromised);
        if compromised.is_none() {
            *compromised = Some(reason);
        }
    }

    fn compromised(&self) -> Option<String> {
        lock_mutex(&self.compromised).clone()
    }

    fn expected_mtime(&self) -> SystemTime {
        *lock_mutex(&self.expected_mtime)
    }

    fn set_expected_mtime(&self, modified: SystemTime) {
        *lock_mutex(&self.expected_mtime) = modified;
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn other_error(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::Other, message.into())
}
