#![cfg(unix)]

//! Unix-domain server discovery through the native remote client handshake.
//!
//! Discovery intentionally does not treat a filename as proof that a server
//! exists.  Candidates are canonical UUIDv4 socket names, checked with
//! `lstat`, and then connected through [`super::Client::connect`] with a
//! per-probe deadline.  Protocol, stale-socket, and timeout failures omit a
//! candidate; filesystem enumeration failures remain explicit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use tokio::fs;
use tokio::time::timeout;

use super::{Client, ClientError, ClientOptions};
use crate::remote::schemas::ServerId;
use crate::remote::transport::{EndpointSpec, build_transport};

use std::os::unix::fs::FileTypeExt;

const DEFAULT_DISCOVERY_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;
const UNIX_SOCKET_SUFFIX: &str = ".sock";
const MAX_CONCURRENT_DISCOVERY_PROBES: usize = 16;

/// One discovered, server-addressed Unix socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnixServerRoute {
    /// Canonical logical server identity encoded by the socket filename.
    pub server_id: ServerId,
    /// Socket path used by the native client transport.
    pub path: PathBuf,
}

/// Options for [`discover_unix_servers`].
#[derive(Clone, Debug)]
pub struct DiscoverUnixServersOptions {
    /// Directory containing server-addressed Unix sockets.
    pub directory: PathBuf,
    /// Maximum time for each connection and hello handshake.
    /// Defaults to one second.
    pub timeout_ms: Option<u64>,
}

/// Discovers reachable local servers by probing their Unix socket handshakes.
///
/// At most sixteen probes run concurrently.  The returned routes are sorted by
/// canonical server id, independent of directory enumeration and task
/// scheduling order.
///
/// # Errors
///
/// Returns a [`ClientError`] when timeout options, directory enumeration, or a
/// non-ignorable candidate probe fails.  Missing directories and candidate
/// sockets that disappear during shutdown are treated as an empty result, as
/// in the source client.
pub async fn discover_unix_servers(
    options: DiscoverUnixServersOptions,
) -> Result<Vec<UnixServerRoute>, ClientError> {
    let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_DISCOVERY_TIMEOUT_MS);
    if !(1..=MAX_TIMER_DELAY_MS).contains(&timeout_ms) {
        return Err(ClientError::protocol(format!(
            "Unix discovery timeoutMs must be an integer between 1 and {MAX_TIMER_DELAY_MS}"
        )));
    }

    let mut entries = match fs::read_dir(&options.directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ClientError::disconnected(format!(
                "failed to enumerate Unix server directory {}: {error}",
                options.directory.display()
            )));
        }
    };
    let mut candidates = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        ClientError::disconnected(format!(
            "failed to enumerate Unix server directory {}: {error}",
            options.directory.display()
        ))
    })? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(server_name) = name.strip_suffix(UNIX_SOCKET_SUFFIX) else {
            continue;
        };
        let Ok(server_id) = ServerId::new(server_name.to_owned()) else {
            continue;
        };
        candidates.push(UnixServerRoute {
            server_id,
            path: options.directory.join(name),
        });
    }

    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let candidates = Arc::new(candidates);
    let next_index = Arc::new(AtomicUsize::new(0));
    let routes = Arc::new(Mutex::new(Vec::<UnixServerRoute>::new()));
    let failure = Arc::new(Mutex::new(None::<ClientError>));
    let worker_count = candidates.len().min(MAX_CONCURRENT_DISCOVERY_PROBES);
    let mut workers = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let candidates = Arc::clone(&candidates);
        let next_index = Arc::clone(&next_index);
        let routes = Arc::clone(&routes);
        let failure = Arc::clone(&failure);
        workers.push(async move {
            loop {
                if lock_mutex(&failure).is_some() {
                    return;
                }
                let index = next_index.fetch_add(1, Ordering::Relaxed);
                let Some(candidate) = candidates.get(index).cloned() else {
                    return;
                };
                let metadata = match fs::symlink_metadata(&candidate.path).await {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        set_failure(
                            &failure,
                            ClientError::disconnected(format!(
                                "failed to inspect Unix server socket {}: {error}",
                                candidate.path.display()
                            )),
                        );
                        return;
                    }
                };
                if !metadata.file_type().is_socket() {
                    continue;
                }
                match probe_unix_server(candidate, timeout_ms).await {
                    Ok(Some(route)) => lock_mutex(&routes).push(route),
                    Ok(None) => {}
                    Err(error) => {
                        set_failure(&failure, error);
                        return;
                    }
                }
            }
        });
    }
    let _ = join_all(workers).await;

    if let Some(error) = lock_mutex(&failure).take() {
        return Err(error);
    }
    let mut routes = lock_mutex(&routes).clone();
    routes.sort_by(|left, right| left.server_id.as_str().cmp(right.server_id.as_str()));
    Ok(routes)
}

async fn probe_unix_server(
    route: UnixServerRoute,
    timeout_ms: u64,
) -> Result<Option<UnixServerRoute>, ClientError> {
    let transport_factory = build_transport(&EndpointSpec::Unix {
        path: route.path.clone(),
        max_pending_bytes: None,
    })
    .map_err(|error| ClientError::protocol(format!("Unix transport options rejected: {error}")))?;
    let client = Client::new(ClientOptions {
        transport_factory,
        server_id: route.server_id.as_str().to_owned(),
        max_frame_length: None,
        on_listener_error: None,
    })
    .map_err(|error| ClientError::protocol(error.to_string()))?;

    let outcome = match timeout(Duration::from_millis(timeout_ms), client.connect()).await {
        Ok(Ok(_hello)) => Ok(Some(route)),
        Ok(Err(error)) if ignorable_probe_error(&error) => Ok(None),
        Ok(Err(error)) => Err(error),
        Err(_elapsed) => Ok(None),
    };
    // Client::dispose synchronously closes an accepted transport and fences a
    // still-connecting attempt.  The spawned transport opener observes that
    // fence and closes a late connection instead of leaking it.
    client.dispose();
    outcome
}

fn ignorable_probe_error(error: &ClientError) -> bool {
    match error {
        ClientError::Protocol(_) | ClientError::Disconnected(_) => true,
        ClientError::Server(server) => server.code == "version",
        ClientError::Disposed(_) | ClientError::Cancelled(_) => false,
    }
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set_failure(failure: &Mutex<Option<ClientError>>, error: ClientError) {
    let mut guard = lock_mutex(failure);
    if guard.is_none() {
        *guard = Some(error);
    }
}
