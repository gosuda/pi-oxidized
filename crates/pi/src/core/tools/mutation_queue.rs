//! Per-realpath serialization for file mutations (edit / write).
//!
//! Ports `.references/pi-2.0/packages/coding-agent/src/core/tools/file-mutation-queue.ts`.
//! Operations targeting the same canonical path run FIFO; operations on
//! distinct paths run concurrently. The queue key is
//! `realpath(resolve(path))`, falling back to the lexically resolved path
//! when the target does not yet exist (`ENOENT` / `ENOTDIR`). Map entries
//! are cleaned up when the last waiter for a key finishes, including on
//! cancellation via a drop guard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use thiserror::Error;
use tokio::sync::Semaphore;

use super::path_utils::resolve_lexically_absolute;

/// Errors produced while registering a path into the mutation queue.
#[derive(Debug, Error)]
pub enum MutationQueueError {
    /// Resolving the mutation-queue key for a path failed (working-directory
    /// lookup, or a `realpath` failure that is not "path missing").
    #[error("failed to resolve mutation queue key for {path}: {source}")]
    ResolveKey {
        /// The path whose key could not be resolved.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// The per-path semaphore was closed. Unreachable in practice: the
    /// semaphore is never closed by this module.
    #[error("mutation queue for {path} is unavailable")]
    QueueUnavailable {
        /// The path whose queue is unavailable.
        path: PathBuf,
    },
}

/// Global registry of per-key FIFO gates. Entries are held as [`Weak`] so a
/// finished key can be reclaimed; the live [`Arc`] is owned by each
/// outstanding waiter.
static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Weak<Semaphore>>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolve the queue key for `file_path` (TypeScript `getMutationQueueKey`).
///
/// Absolute-ize and lexically normalize first, then `realpath`. Missing
/// targets (`ENOENT` / `ENOTDIR`) fall back to the resolved path so a
/// create-on-write and a subsequent edit of the same path share a key.
async fn mutation_queue_key(file_path: &Path) -> Result<PathBuf, MutationQueueError> {
    let resolved =
        resolve_lexically_absolute(file_path).map_err(|source| MutationQueueError::ResolveKey {
            path: file_path.to_path_buf(),
            source,
        })?;
    match tokio::fs::canonicalize(&resolved).await {
        Ok(canonical) => Ok(canonical),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::NotADirectory =>
        {
            Ok(resolved)
        }
        Err(source) => Err(MutationQueueError::ResolveKey {
            path: file_path.to_path_buf(),
            source,
        }),
    }
}

/// Holds one waiter's Arc-cloned gate; on drop, removes the registry entry
/// when this is the last remaining strong reference.
struct QueueRegistration {
    key: PathBuf,
    gate: Arc<Semaphore>,
}

impl QueueRegistration {
    fn register(key: PathBuf) -> Self {
        let mut map = lock_registry();
        let gate = if let Some(existing) = map.get(&key).and_then(Weak::upgrade) {
            existing
        } else {
            let gate = Arc::new(Semaphore::new(1));
            map.insert(key.clone(), Arc::downgrade(&gate));
            gate
        };
        Self { key, gate }
    }
}

impl Drop for QueueRegistration {
    fn drop(&mut self) {
        let mut map = lock_registry();
        let own_gate = Arc::downgrade(&self.gate);
        let maps_to_this_gate = map
            .get(&self.key)
            .is_some_and(|mapped| Weak::ptr_eq(mapped, &own_gate));

        // Identity verification, last-reference detection, and removal must
        // be one registry-locked operation. Otherwise a concurrent registrar
        // can install a replacement gate between the count check and remove.
        if maps_to_this_gate && Arc::strong_count(&self.gate) == 1 {
            map.remove(&self.key);
        }
    }
}

/// Serialize `f` against every other mutation targeting the same realpath.
/// Distinct keys run in parallel. TypeScript `withFileMutationQueue`.
///
/// # Errors
///
/// Returns [`MutationQueueError`] when the queue key cannot be resolved or
/// the (never-closed) semaphore rejects the acquire.
pub async fn with_file_mutation_queue<T, F, Fut>(
    file_path: impl AsRef<Path>,
    f: F,
) -> Result<T, MutationQueueError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let path = file_path.as_ref();
    let key = mutation_queue_key(path).await?;
    // Registration is held for the whole operation so Drop cleans the map
    // even when the future is cancelled.
    let registration = QueueRegistration::register(key);
    let permit = registration
        .gate
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| MutationQueueError::QueueUnavailable {
            path: path.to_path_buf(),
        })?;
    // Declaration order: permit drops before registration (reverse of
    // declaration), releasing the next waiter before the map entry is
    // considered for cleanup.
    let result = f().await;
    drop(permit);
    Ok(result)
}

#[cfg(test)]
fn registry_holds_key(key: &Path) -> bool {
    lock_registry().get(key).and_then(Weak::upgrade).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;

    type TestResult = Result<(), Box<dyn Error + Send + Sync>>;

    #[tokio::test]
    async fn same_key_runs_serially() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("target.txt");
        std::fs::write(&path, b"seed")?;

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for index in 0..4 {
            let path = path.clone();
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let completed = Arc::clone(&completed);
            handles.push(tokio::spawn(async move {
                with_file_mutation_queue(&path, || async {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    completed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(index);
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }

        for handle in handles {
            handle
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))??;
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        let mut recorded = completed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        recorded.sort_unstable();
        assert_eq!(recorded, vec![0, 1, 2, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn distinct_keys_run_concurrently() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path_a = dir.path().join("a.txt");
        let path_b = dir.path().join("b.txt");
        std::fs::write(&path_a, b"a")?;
        std::fs::write(&path_b, b"b")?;

        let a_entered = Arc::new(Notify::new());
        let b_entered = Arc::new(Notify::new());
        let a_signal = Arc::clone(&a_entered);
        let b_wait = Arc::clone(&b_entered);
        let b_signal = Arc::clone(&b_entered);

        let a = tokio::spawn(async move {
            with_file_mutation_queue(&path_a, || async {
                a_signal.notify_one();
                // Wait for B to enter its critical section. If the two paths
                // shared a key this would deadlock; the timeout below fails.
                b_wait.notified().await;
                "a"
            })
            .await
        });
        let b = tokio::spawn(async move {
            // Wait until A holds its permit so the only way B starts is
            // cross-key parallelism.
            a_entered.notified().await;
            with_file_mutation_queue(&path_b, || async {
                b_signal.notify_one();
                "b"
            })
            .await
        });

        let a_result = tokio::time::timeout(Duration::from_secs(2), a)
            .await
            .map_err(|_| std::io::Error::other("A timed out; keys may be serialized"))?
            .map_err(|error| std::io::Error::other(error.to_string()))??;
        let b_result = tokio::time::timeout(Duration::from_secs(2), b)
            .await
            .map_err(|_| std::io::Error::other("B timed out; keys may be serialized"))?
            .map_err(|error| std::io::Error::other(error.to_string()))??;
        assert_eq!(a_result, "a");
        assert_eq!(b_result, "b");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_and_realpath_share_one_key() -> TestResult {
        let dir = tempfile::tempdir()?;
        let real = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&real, b"seed")?;
        std::os::unix::fs::symlink(&real, &link)?;

        let order = Arc::new(Mutex::new(Vec::new()));
        let order_a = Arc::clone(&order);
        let order_b = Arc::clone(&order);

        // A holds the realpath key via the symlink path; B targets the real
        // path and must wait for A to finish.
        let a = tokio::spawn(async move {
            with_file_mutation_queue(&link, || async {
                order_a
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("a-start");
                tokio::time::sleep(Duration::from_millis(30)).await;
                order_a
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("a-end");
            })
            .await
        });
        // Give A a chance to register and acquire first.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let b = tokio::spawn(async move {
            with_file_mutation_queue(&real, || async {
                order_b
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("b-start");
                order_b
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push("b-end");
            })
            .await
        });

        a.await
            .map_err(|error| std::io::Error::other(error.to_string()))??;
        b.await
            .map_err(|error| std::io::Error::other(error.to_string()))??;
        let recorded = order.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(recorded, vec!["a-start", "a-end", "b-start", "b-end"]);
        Ok(())
    }

    #[tokio::test]
    async fn missing_path_uses_resolved_key_and_runs() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("does-not-exist-yet.txt");
        let result = with_file_mutation_queue(&path, || async { 42 }).await?;
        assert_eq!(result, 42);
        Ok(())
    }

    #[tokio::test]
    async fn registry_is_cleaned_after_completion() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cleanup.txt");
        std::fs::write(&path, b"seed")?;
        let key = tokio::fs::canonicalize(&path).await?;

        with_file_mutation_queue(&path, || async {
            // Entry is live while the operation holds the registration.
            assert!(registry_holds_key(&key));
        })
        .await?;
        assert!(!registry_holds_key(&key));

        // Two sequential ops leave this key empty each time.
        with_file_mutation_queue(&path, || async {}).await?;
        with_file_mutation_queue(&path, || async {}).await?;
        assert!(!registry_holds_key(&key));
        Ok(())
    }

    #[test]
    fn stale_last_registration_cannot_remove_replacement_gate() -> TestResult {
        let key = PathBuf::from("replacement-race-key");
        let stale = QueueRegistration::register(key.clone());
        let replacement_gate = Arc::new(Semaphore::new(1));

        // Deterministically model the exact race state: a new registrar has
        // installed a replacement after the stale registration observed
        // itself as the final strong owner.
        lock_registry().insert(key.clone(), Arc::downgrade(&replacement_gate));
        drop(stale);

        let Some(mapped) = lock_registry().get(&key).and_then(Weak::upgrade) else {
            return Err("stale drop removed the replacement gate".into());
        };
        assert!(Arc::ptr_eq(&mapped, &replacement_gate));
        lock_registry().remove(&key);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_ops_on_same_key_leave_registry_empty() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, b"seed")?;
        let key = tokio::fs::canonicalize(&path).await?;

        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                with_file_mutation_queue(&path, || async {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        for handle in handles {
            handle
                .await
                .map_err(|error| std::io::Error::other(error.to_string()))??;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 8);
        assert!(!registry_holds_key(&key));
        Ok(())
    }
}
